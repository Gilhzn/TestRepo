//! Push authorization.
//!
//! Every Change is already individually signed (ed25519). The server's job
//! is to decide which signing keys it trusts. Two modes:
//!
//!   * **Open** (default when no allowed_signers file exists) — accept any
//!     well-signed change. Useful for local development and demos.
//!   * **Allowlist** — only accept changes whose author_key appears in
//!     `<repo_root>/.mosaic/allowed_signers.txt` (one hex pubkey per line,
//!     blank lines and `#` comments ignored).
//!
//! Authorization happens after the bundle's signatures are verified, so we
//! never have to trust the client's claim about which key signed what.

use mosaic_core::sync::Bundle;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct Policy {
    mode: Mode,
}

#[derive(Debug, Clone, Default)]
enum Mode {
    #[default]
    Open,
    Allowlist {
        keys: HashSet<[u8; 32]>,
    },
}

impl Policy {
    pub fn open() -> Self {
        Self { mode: Mode::Open }
    }

    pub fn allowlist<I, K>(keys: I) -> Self
    where
        I: IntoIterator<Item = K>,
        K: Into<[u8; 32]>,
    {
        Self {
            mode: Mode::Allowlist {
                keys: keys.into_iter().map(Into::into).collect(),
            },
        }
    }

    pub fn load(repo_root: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = repo_root
            .as_ref()
            .join(".mosaic")
            .join("allowed_signers.txt");
        match fs::read_to_string(&path) {
            Ok(s) => Ok(Self::from_text(&s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::open()),
            Err(e) => Err(e),
        }
    }

    pub fn from_text(s: &str) -> Self {
        let mut keys: HashSet<[u8; 32]> = HashSet::new();
        for line in s.lines() {
            let raw = line.trim();
            if raw.is_empty() || raw.starts_with('#') {
                continue;
            }
            if let Ok(bytes) = hex::decode(raw) {
                if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
                    keys.insert(arr);
                }
            }
        }
        Self {
            mode: Mode::Allowlist { keys },
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self.mode, Mode::Open)
    }

    pub fn allows(&self, pubkey: &[u8; 32]) -> bool {
        match &self.mode {
            Mode::Open => true,
            Mode::Allowlist { keys } => keys.contains(pubkey),
        }
    }

    pub fn allowed_count(&self) -> Option<usize> {
        match &self.mode {
            Mode::Open => None,
            Mode::Allowlist { keys } => Some(keys.len()),
        }
    }

    pub fn authorize_bundle(&self, bundle: &Bundle) -> Result<(), String> {
        if self.is_open() {
            return Ok(());
        }

        // Pre-verify each attestation in the bundle. An attestation is
        // usable iff its invoker_pubkey is on the allowlist and the
        // signature checks out.
        let mut valid_attestations: Vec<&mosaic_core::attestation::Attestation> = Vec::new();
        for att in &bundle.attestations {
            if att.verify().is_err() {
                continue;
            }
            if self.allows(&att.invoker_pubkey.to_bytes()) {
                valid_attestations.push(att);
            }
        }

        for change in &bundle.changes {
            let pubkey = change.author_key.to_bytes();
            if self.allows(&pubkey) {
                continue;
            }
            // Otherwise the change must be covered by a valid attestation.
            let chain_ok = valid_attestations
                .iter()
                .any(|att| att.authorize(change).is_ok());
            if !chain_ok {
                return Err(format!(
                    "change {}: author key {} is not on the allowlist and no \
                     valid attestation from a trusted invoker covers it",
                    &change.id().to_hex()[..12],
                    hex::encode(pubkey)
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mosaic_core::m1::change::{ChangeBuilder, FileChange, FileKind};
    use mosaic_core::m1::identity::Identity;
    use mosaic_core::m1::signing::SigningKey;
    use mosaic_core::repo::Repository;
    use mosaic_core::sync::build_bundle;
    use tempfile::TempDir;

    fn make_bundle(key: &SigningKey) -> Bundle {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let id = Identity::human("dev@example.com", None).unwrap();
        let change = ChangeBuilder::new(id, key.clone())
            .intent("test")
            .file(FileChange {
                path: "x".into(),
                kind: FileKind::Text,
                patch: b"x".to_vec(),
                conflicts: Vec::new(),
            })
            .build()
            .unwrap();
        let cid = repo.commit(change).unwrap();
        build_bundle(&repo, &[cid]).unwrap()
    }

    fn make_attested_bundle(
        invoker_key: &SigningKey,
        session_key: &SigningKey,
    ) -> Bundle {
        use mosaic_core::attestation::Attestation;
        use mosaic_core::m1::change::Tai64N;

        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let human = Identity::human("eyal@example.com", None).unwrap();
        let agent = Identity::agent("claude-code", "sess-1", human).unwrap();

        let now = Tai64N::now();
        let valid_from = Tai64N(now.0 - 60, 0);
        let valid_until = Tai64N(now.0 + 3600, 0);
        let att = Attestation::issue(
            agent.clone(),
            session_key.verifying_key(),
            valid_from,
            valid_until,
            invoker_key,
        )
        .unwrap();

        let change = mosaic_core::m1::change::ChangeBuilder::new(agent, session_key.clone())
            .ts(now)
            .intent("via agent")
            .file(FileChange {
                path: "x".into(),
                kind: FileKind::Text,
                patch: b"x".to_vec(),
                conflicts: Vec::new(),
            })
            .build()
            .unwrap();
        let cid = repo.commit(change).unwrap();
        let bundle = build_bundle(&repo, &[cid]).unwrap();
        bundle.with_attestation(att)
    }

    #[test]
    fn attestation_allows_session_signed_change() {
        let invoker_key = SigningKey::generate();
        let session_key = SigningKey::generate();
        let bundle = make_attested_bundle(&invoker_key, &session_key);

        // Trust only the invoker's long-term key; the session key is NOT
        // directly trusted, but the attestation should bridge it.
        let policy = Policy::allowlist([invoker_key.verifying_key().to_bytes()]);
        assert!(policy.authorize_bundle(&bundle).is_ok());
    }

    #[test]
    fn attestation_from_untrusted_invoker_is_rejected() {
        let invoker_key = SigningKey::generate(); // NOT on the allowlist
        let session_key = SigningKey::generate();
        let bundle = make_attested_bundle(&invoker_key, &session_key);

        let other = SigningKey::generate();
        let policy = Policy::allowlist([other.verifying_key().to_bytes()]);
        assert!(policy.authorize_bundle(&bundle).is_err());
    }

    #[test]
    fn open_policy_accepts_anything() {
        let key = SigningKey::generate();
        let bundle = make_bundle(&key);
        assert!(Policy::open().authorize_bundle(&bundle).is_ok());
    }

    #[test]
    fn allowlist_rejects_unknown_key() {
        let trusted = SigningKey::generate();
        let attacker = SigningKey::generate();
        let bundle = make_bundle(&attacker);
        let policy = Policy::allowlist([trusted.verifying_key().to_bytes()]);
        assert!(policy.authorize_bundle(&bundle).is_err());
    }

    #[test]
    fn allowlist_accepts_known_key() {
        let trusted = SigningKey::generate();
        let bundle = make_bundle(&trusted);
        let policy = Policy::allowlist([trusted.verifying_key().to_bytes()]);
        assert!(policy.authorize_bundle(&bundle).is_ok());
    }

    #[test]
    fn from_text_skips_blank_and_comments() {
        let key = SigningKey::generate();
        let hex_key = hex::encode(key.verifying_key().to_bytes());
        let text = format!(
            "# this is a comment\n\
             \n\
             {hex_key}\n\
             # another comment\n"
        );
        let policy = Policy::from_text(&text);
        assert!(policy.allows(&key.verifying_key().to_bytes()));
        assert_eq!(policy.allowed_count(), Some(1));
    }

    #[test]
    fn load_missing_file_returns_open_policy() {
        let dir = TempDir::new().unwrap();
        let policy = Policy::load(dir.path()).unwrap();
        assert!(policy.is_open());
    }

    #[test]
    fn load_existing_file_returns_allowlist() {
        let dir = TempDir::new().unwrap();
        let mosaic_dir = dir.path().join(".mosaic");
        std::fs::create_dir_all(&mosaic_dir).unwrap();
        let key = SigningKey::generate();
        let hex_key = hex::encode(key.verifying_key().to_bytes());
        std::fs::write(
            mosaic_dir.join("allowed_signers.txt"),
            format!("{hex_key}\n"),
        )
        .unwrap();
        let policy = Policy::load(dir.path()).unwrap();
        assert!(!policy.is_open());
        assert_eq!(policy.allowed_count(), Some(1));
        assert!(policy.allows(&key.verifying_key().to_bytes()));
    }
}
