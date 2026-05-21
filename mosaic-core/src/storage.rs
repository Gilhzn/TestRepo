//! Content-addressable storage (CAS).
//!
//! Blobs are addressed by BLAKE3 of their *uncompressed* content, stored on
//! disk in zstd-compressed form. Layout mirrors Git's loose-object scheme:
//! the first byte of the hex hash names a directory, the rest names the file.

use crate::error::{Error, Result};
use crate::hash::Hash;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const ZSTD_LEVEL: i32 = 3;

pub trait Cas {
    fn put(&self, bytes: &[u8]) -> Result<Hash>;
    fn get(&self, hash: &Hash) -> Result<Vec<u8>>;
    fn has(&self, hash: &Hash) -> Result<bool>;
}

pub struct FsCas {
    root: PathBuf,
}

impl FsCas {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path_for(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Cas for FsCas {
    fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = Hash::of(bytes);
        let path = self.path_for(&hash);

        if path.exists() {
            return Ok(hash);
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp = path.with_extension("tmp");
        {
            let f = fs::File::create(&tmp)?;
            let mut enc = zstd::Encoder::new(f, ZSTD_LEVEL)?;
            enc.write_all(bytes)?;
            enc.finish()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> Result<Vec<u8>> {
        let path = self.path_for(hash);
        let f = fs::File::open(&path)?;
        let mut dec = zstd::Decoder::new(f)?;
        let mut buf = Vec::new();
        dec.read_to_end(&mut buf)?;

        let actual = Hash::of(&buf);
        if actual != *hash {
            return Err(Error::HashMismatch {
                expected: hash.to_hex(),
                actual: actual.to_hex(),
            });
        }
        Ok(buf)
    }

    fn has(&self, hash: &Hash) -> Result<bool> {
        Ok(self.path_for(hash).exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn put_then_get_round_trip() {
        let dir = TempDir::new().unwrap();
        let cas = FsCas::open(dir.path()).unwrap();
        let data = b"mosaic test payload";

        let h = cas.put(data).unwrap();
        assert!(cas.has(&h).unwrap());
        assert_eq!(cas.get(&h).unwrap(), data);
    }

    #[test]
    fn put_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let cas = FsCas::open(dir.path()).unwrap();
        let data = b"same content";

        let h1 = cas.put(data).unwrap();
        let h2 = cas.put(data).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_inputs_yield_different_hashes() {
        let dir = TempDir::new().unwrap();
        let cas = FsCas::open(dir.path()).unwrap();
        let a = cas.put(b"one").unwrap();
        let b = cas.put(b"two").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn missing_blob_errors() {
        let dir = TempDir::new().unwrap();
        let cas = FsCas::open(dir.path()).unwrap();
        let phantom = Hash::of(b"never stored");
        assert!(!cas.has(&phantom).unwrap());
        assert!(cas.get(&phantom).is_err());
    }

    #[test]
    fn detects_corruption() {
        let dir = TempDir::new().unwrap();
        let cas = FsCas::open(dir.path()).unwrap();
        let h = cas.put(b"original").unwrap();

        let path = cas.path_for(&h);
        let mut tampered = fs::File::create(&path).unwrap();
        let mut enc = zstd::Encoder::new(&mut tampered, ZSTD_LEVEL).unwrap();
        enc.write_all(b"different bytes!").unwrap();
        enc.finish().unwrap();

        match cas.get(&h) {
            Err(Error::HashMismatch { .. }) => {}
            other => panic!("expected HashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn large_blob_compresses() {
        let dir = TempDir::new().unwrap();
        let cas = FsCas::open(dir.path()).unwrap();
        let data = vec![0xABu8; 1_000_000];

        let h = cas.put(&data).unwrap();
        let got = cas.get(&h).unwrap();
        assert_eq!(got, data);

        let on_disk = fs::metadata(cas.path_for(&h)).unwrap().len();
        assert!(on_disk < 10_000, "expected heavy compression, got {on_disk} bytes");
    }
}
