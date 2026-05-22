//! Encryption at rest for the content-addressable store.
//!
//! `EncryptedCas` is a standalone `Cas` implementation that keeps blobs on
//! disk under their **plaintext** BLAKE3 hash (so content-addressing and
//! cross-repo dedup still work) but stores the bytes ChaCha20-Poly1305 AEAD-
//! encrypted. A 32-byte master key lives in `<root>/key` (the operator is
//! responsible for protecting that file — typical deployment is `chmod 600`
//! plus full-disk encryption or a KMS-fetched key at boot).
//!
//! On-disk blob layout: `nonce(12) || ciphertext+tag`. Read verifies the
//! decrypted plaintext re-hashes to the requested key, so a tampered or
//! truncated file is detected even before the AEAD tag check would catch it.
//!
//! This is the control a CISO in finance / health / defense asks about
//! first: "is our source encrypted at rest, with keys we hold?".

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::storage::Cas;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use std::fs;
use std::path::{Path, PathBuf};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const ZSTD_LEVEL: i32 = 3;

pub struct EncryptedCas {
    root: PathBuf,
    cipher: ChaCha20Poly1305,
}

impl EncryptedCas {
    /// Open (or create) an encrypted store at `root`. If `<root>/key` does
    /// not exist, a fresh random key is generated and written — back it up,
    /// because losing it means losing the data.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let key_path = root.join("key");
        let key_bytes = match fs::read(&key_path) {
            Ok(b) if b.len() == KEY_LEN => b,
            Ok(_) => {
                return Err(Error::Serialization(format!(
                    "{} is not a {KEY_LEN}-byte key",
                    key_path.display()
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut k = [0u8; KEY_LEN];
                use rand::RngCore;
                rand::rngs::OsRng.fill_bytes(&mut k);
                fs::write(&key_path, k)?;
                k.to_vec()
            }
            Err(e) => return Err(Error::Io(e)),
        };
        Self::with_key(root, &key_bytes)
    }

    /// Open with an explicit key (e.g. fetched from a KMS at boot).
    pub fn with_key(root: impl Into<PathBuf>, key_bytes: &[u8]) -> Result<Self> {
        if key_bytes.len() != KEY_LEN {
            return Err(Error::InvalidKeyLength {
                expected: KEY_LEN,
                actual: key_bytes.len(),
            });
        }
        let key = Key::from_slice(key_bytes);
        let cipher = ChaCha20Poly1305::new(key);
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root, cipher })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn blob_path(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join("blobs").join(&hex[..2]).join(&hex[2..])
    }

    fn random_nonce() -> [u8; NONCE_LEN] {
        use rand::RngCore;
        let mut n = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut n);
        n
    }
}

impl Cas for EncryptedCas {
    fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = Hash::of(bytes);
        let path = self.blob_path(&hash);
        if path.exists() {
            return Ok(hash);
        }
        // Compress first (so we encrypt smaller payloads), then seal.
        let compressed = zstd::encode_all(bytes, ZSTD_LEVEL)
            .map_err(|e| Error::Serialization(format!("zstd: {e}")))?;
        let nonce_bytes = Self::random_nonce();
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(nonce, compressed.as_ref())
            .map_err(|_| Error::Serialization("AEAD encrypt failed".into()))?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        fs::write(&tmp, &out)?;
        fs::rename(&tmp, &path)?;
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> Result<Vec<u8>> {
        let path = self.blob_path(hash);
        let raw = fs::read(&path)?;
        if raw.len() < NONCE_LEN {
            return Err(Error::Serialization("encrypted blob too short".into()));
        }
        let (nonce_bytes, ciphertext) = raw.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        let compressed = self
            .cipher
            .decrypt(nonce, ciphertext)
            .map_err(|_| Error::BadSignature)?;
        let plaintext = zstd::decode_all(compressed.as_slice())
            .map_err(|e| Error::Serialization(format!("zstd decode: {e}")))?;

        let actual = Hash::of(&plaintext);
        if actual != *hash {
            return Err(Error::HashMismatch {
                expected: hash.to_hex(),
                actual: actual.to_hex(),
            });
        }
        Ok(plaintext)
    }

    fn has(&self, hash: &Hash) -> Result<bool> {
        Ok(self.blob_path(hash).exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn put_get_round_trip() {
        let dir = TempDir::new().unwrap();
        let cas = EncryptedCas::open(dir.path()).unwrap();
        let data = b"top secret payload";
        let h = cas.put(data).unwrap();
        assert!(cas.has(&h).unwrap());
        assert_eq!(cas.get(&h).unwrap(), data);
    }

    #[test]
    fn bytes_on_disk_are_not_plaintext() {
        let dir = TempDir::new().unwrap();
        let cas = EncryptedCas::open(dir.path()).unwrap();
        let data = b"SENSITIVE-MARKER-STRING-12345";
        let h = cas.put(data).unwrap();

        let raw = fs::read(cas.blob_path(&h)).unwrap();
        // The plaintext marker must NOT appear in the stored bytes.
        let needle = b"SENSITIVE-MARKER";
        assert!(
            !raw.windows(needle.len()).any(|w| w == needle),
            "plaintext marker leaked into encrypted blob"
        );
    }

    #[test]
    fn put_is_idempotent_and_content_addressed() {
        let dir = TempDir::new().unwrap();
        let cas = EncryptedCas::open(dir.path()).unwrap();
        let h1 = cas.put(b"same").unwrap();
        let h2 = cas.put(b"same").unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn wrong_key_cannot_decrypt() {
        let dir = TempDir::new().unwrap();
        let h = {
            let cas = EncryptedCas::with_key(dir.path(), &[7u8; KEY_LEN]).unwrap();
            cas.put(b"locked away").unwrap()
        };
        // Re-open with a DIFFERENT key — decryption must fail.
        let other = EncryptedCas::with_key(dir.path(), &[9u8; KEY_LEN]).unwrap();
        assert!(other.get(&h).is_err());
    }

    #[test]
    fn same_key_reopen_reads_back() {
        let dir = TempDir::new().unwrap();
        let key = [42u8; KEY_LEN];
        let h = {
            let cas = EncryptedCas::with_key(dir.path(), &key).unwrap();
            cas.put(b"persistent secret").unwrap()
        };
        let reopened = EncryptedCas::with_key(dir.path(), &key).unwrap();
        assert_eq!(reopened.get(&h).unwrap(), b"persistent secret");
    }

    #[test]
    fn tampering_with_ciphertext_is_detected() {
        let dir = TempDir::new().unwrap();
        let cas = EncryptedCas::open(dir.path()).unwrap();
        let h = cas.put(b"integrity matters").unwrap();

        // Flip a byte in the stored ciphertext.
        let path = cas.blob_path(&h);
        let mut raw = fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xFF;
        fs::write(&path, &raw).unwrap();

        // AEAD tag check should reject it.
        assert!(cas.get(&h).is_err());
    }

    #[test]
    fn auto_generates_key_file_on_first_open() {
        let dir = TempDir::new().unwrap();
        assert!(!dir.path().join("key").exists());
        let _cas = EncryptedCas::open(dir.path()).unwrap();
        let key = fs::read(dir.path().join("key")).unwrap();
        assert_eq!(key.len(), KEY_LEN);
    }

    #[test]
    fn missing_blob_errors() {
        let dir = TempDir::new().unwrap();
        let cas = EncryptedCas::open(dir.path()).unwrap();
        let phantom = Hash::of(b"never stored");
        assert!(!cas.has(&phantom).unwrap());
        assert!(cas.get(&phantom).is_err());
    }

    #[test]
    fn large_blob_round_trips() {
        let dir = TempDir::new().unwrap();
        let cas = EncryptedCas::open(dir.path()).unwrap();
        let data = vec![0xABu8; 2_000_000];
        let h = cas.put(&data).unwrap();
        assert_eq!(cas.get(&h).unwrap(), data);
    }
}
