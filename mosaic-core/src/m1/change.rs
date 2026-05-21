use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::m1::identity::Identity;
use crate::m1::signing::{Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChangeId(pub Hash);

impl ChangeId {
    pub fn to_hex(&self) -> String {
        self.0.to_hex()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Tai64N(pub u64, pub u32);

impl Tai64N {
    pub fn now() -> Self {
        let d = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self(d.as_secs(), d.subsec_nanos())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    Text,
    Binary,
    Tree,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub kind: FileKind,
    pub patch: Vec<u8>,
    pub conflicts: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub author: Identity,
    pub ts: Tai64N,
    pub deps: Vec<ChangeId>,
    pub intent: Option<String>,
    pub body: Vec<FileChange>,
    pub sig: Signature,
    pub author_key: VerifyingKey,
}

#[derive(Serialize)]
struct CanonicalView<'a> {
    author: &'a Identity,
    ts: &'a Tai64N,
    deps: &'a [ChangeId],
    intent: &'a Option<String>,
    body: &'a [FileChange],
    author_key: &'a VerifyingKey,
}

impl Change {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let view = CanonicalView {
            author: &self.author,
            ts: &self.ts,
            deps: &self.deps,
            intent: &self.intent,
            body: &self.body,
            author_key: &self.author_key,
        };
        bincode::serialize(&view).expect("canonical serialization is infallible")
    }

    pub fn id(&self) -> ChangeId {
        ChangeId(Hash::of(&self.canonical_bytes()))
    }

    pub fn verify(&self) -> Result<()> {
        self.author.validate()?;
        let msg = self.canonical_bytes();
        self.author_key.verify(&msg, &self.sig)
    }
}

pub struct ChangeBuilder {
    author: Identity,
    signing_key: SigningKey,
    ts: Option<Tai64N>,
    deps: Vec<ChangeId>,
    intent: Option<String>,
    body: Vec<FileChange>,
}

impl ChangeBuilder {
    pub fn new(author: Identity, signing_key: SigningKey) -> Self {
        Self {
            author,
            signing_key,
            ts: None,
            deps: Vec::new(),
            intent: None,
            body: Vec::new(),
        }
    }

    pub fn ts(mut self, ts: Tai64N) -> Self {
        self.ts = Some(ts);
        self
    }

    pub fn intent(mut self, intent: impl Into<String>) -> Self {
        self.intent = Some(intent.into());
        self
    }

    pub fn dep(mut self, id: ChangeId) -> Self {
        self.deps.push(id);
        self
    }

    pub fn deps(mut self, ids: impl IntoIterator<Item = ChangeId>) -> Self {
        self.deps.extend(ids);
        self
    }

    pub fn file(mut self, fc: FileChange) -> Self {
        self.body.push(fc);
        self
    }

    pub fn build(self) -> Result<Change> {
        self.author.validate()?;
        let author_key = self.signing_key.verifying_key();
        let ts = self.ts.unwrap_or_else(Tai64N::now);

        let view = CanonicalView {
            author: &self.author,
            ts: &ts,
            deps: &self.deps,
            intent: &self.intent,
            body: &self.body,
            author_key: &author_key,
        };
        let msg = bincode::serialize(&view)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        let sig = self.signing_key.sign(&msg);

        Ok(Change {
            author: self.author,
            ts,
            deps: self.deps,
            intent: self.intent,
            body: self.body,
            sig,
            author_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_author() -> Identity {
        Identity::human("eyal@example.com", Some("Eyal".into())).unwrap()
    }

    fn sample_file() -> FileChange {
        FileChange {
            path: "src/main.rs".into(),
            kind: FileKind::Text,
            patch: b"@@ -1 +1 @@".to_vec(),
            conflicts: Vec::new(),
        }
    }

    #[test]
    fn build_sign_verify_round_trip() {
        let sk = SigningKey::generate();
        let change = ChangeBuilder::new(sample_author(), sk)
            .intent("fix bug X")
            .file(sample_file())
            .build()
            .unwrap();
        change.verify().unwrap();
    }

    #[test]
    fn mutating_invalidates_signature() {
        let sk = SigningKey::generate();
        let mut change = ChangeBuilder::new(sample_author(), sk)
            .intent("initial")
            .file(sample_file())
            .build()
            .unwrap();
        change.verify().unwrap();

        change.intent = Some("tampered".into());
        assert!(matches!(change.verify(), Err(Error::BadSignature)));
    }

    #[test]
    fn mutating_body_invalidates_signature() {
        let sk = SigningKey::generate();
        let mut change = ChangeBuilder::new(sample_author(), sk)
            .file(sample_file())
            .build()
            .unwrap();
        change.body.push(FileChange {
            path: "extra.rs".into(),
            kind: FileKind::Text,
            patch: Vec::new(),
            conflicts: Vec::new(),
        });
        assert!(matches!(change.verify(), Err(Error::BadSignature)));
    }

    #[test]
    fn different_timestamps_yield_different_ids() {
        let sk = SigningKey::generate();
        let a = ChangeBuilder::new(sample_author(), sk.clone())
            .ts(Tai64N(1_000, 0))
            .file(sample_file())
            .build()
            .unwrap();
        let b = ChangeBuilder::new(sample_author(), sk)
            .ts(Tai64N(2_000, 0))
            .file(sample_file())
            .build()
            .unwrap();
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn identical_inputs_yield_identical_ids() {
        let sk = SigningKey::generate();
        let ts = Tai64N(42, 7);
        let a = ChangeBuilder::new(sample_author(), sk.clone())
            .ts(ts)
            .intent("same")
            .file(sample_file())
            .build()
            .unwrap();
        let b = ChangeBuilder::new(sample_author(), sk)
            .ts(ts)
            .intent("same")
            .file(sample_file())
            .build()
            .unwrap();
        assert_eq!(a.id(), b.id());
        assert_eq!(a.sig.to_bytes(), b.sig.to_bytes());
    }

    #[test]
    fn deps_round_trip_through_serde() {
        let sk = SigningKey::generate();
        let parent = ChangeBuilder::new(sample_author(), sk.clone())
            .ts(Tai64N(1, 0))
            .file(sample_file())
            .build()
            .unwrap();
        let parent_id = parent.id();

        let child = ChangeBuilder::new(sample_author(), sk)
            .ts(Tai64N(2, 0))
            .dep(parent_id)
            .file(sample_file())
            .build()
            .unwrap();

        let bytes = bincode::serialize(&child).unwrap();
        let back: Change = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.deps, vec![parent_id]);
        back.verify().unwrap();
        assert_eq!(back.id(), child.id());
    }

    #[test]
    fn now_produces_nonzero_timestamp() {
        let t = Tai64N::now();
        assert!(t.0 > 0);
    }

    #[test]
    fn agent_authored_change_verifies() {
        let invoker = sample_author();
        let agent = Identity::agent("claude-code", "sess-1", invoker).unwrap();
        let sk = SigningKey::generate();
        let change = ChangeBuilder::new(agent, sk)
            .intent("agent edit")
            .file(sample_file())
            .build()
            .unwrap();
        change.verify().unwrap();
    }
}
