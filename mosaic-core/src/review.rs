//! Code review: comment threads + approval workflow.
//!
//! Comments and Approvals are content-addressed, ed25519-signed artifacts that
//! mirror the canonical-bytes / sig / verify pattern from [`crate::attestation`].
//! They are stored in append-only JSONL files under
//! `<root>/.mosaic/reviews/` (see [`crate::repo::Repository`]).

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::m1::change::{ChangeId, Tai64N};
use crate::m1::identity::Identity;
use crate::m1::signing::{Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Where in the change a comment is anchored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommentAnchor {
    /// Top-level comment on the change as a whole.
    Change,
    /// Anchored to a specific file in the change.
    File { path: String },
    /// Anchored to a specific line of a specific file.
    Line { path: String, line: u32 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comment {
    pub change: ChangeId,
    pub author: Identity,
    pub author_key: VerifyingKey,
    pub ts: Tai64N,
    pub anchor: CommentAnchor,
    pub body: String,
    pub reply_to: Option<Hash>,
    pub sig: Signature,
}

#[derive(Serialize)]
struct CommentCanonicalView<'a> {
    change: &'a ChangeId,
    author: &'a Identity,
    author_key: &'a VerifyingKey,
    ts: &'a Tai64N,
    anchor: &'a CommentAnchor,
    body: &'a str,
    reply_to: &'a Option<Hash>,
}

impl Comment {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let view = CommentCanonicalView {
            change: &self.change,
            author: &self.author,
            author_key: &self.author_key,
            ts: &self.ts,
            anchor: &self.anchor,
            body: &self.body,
            reply_to: &self.reply_to,
        };
        bincode::serialize(&view).expect("canonical serialization is infallible")
    }

    pub fn id(&self) -> Hash {
        Hash::of(&self.canonical_bytes())
    }

    pub fn verify(&self) -> Result<()> {
        self.author.validate()?;
        if self.body.is_empty() {
            return Err(Error::InvalidComment("body is empty".into()));
        }
        let msg = self.canonical_bytes();
        self.author_key
            .verify(&msg, &self.sig)
            .map_err(|_| Error::BadSignature)?;
        Ok(())
    }
}

pub struct CommentBuilder {
    change: ChangeId,
    author: Identity,
    signing_key: SigningKey,
    ts: Option<Tai64N>,
    anchor: CommentAnchor,
    body: String,
    reply_to: Option<Hash>,
}

impl CommentBuilder {
    pub fn new(change: ChangeId, author: Identity, signing_key: SigningKey) -> Self {
        Self {
            change,
            author,
            signing_key,
            ts: None,
            anchor: CommentAnchor::Change,
            body: String::new(),
            reply_to: None,
        }
    }

    pub fn ts(mut self, ts: Tai64N) -> Self {
        self.ts = Some(ts);
        self
    }

    pub fn anchor(mut self, anchor: CommentAnchor) -> Self {
        self.anchor = anchor;
        self
    }

    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    pub fn reply_to(mut self, parent: Hash) -> Self {
        self.reply_to = Some(parent);
        self
    }

    pub fn build(self) -> Result<Comment> {
        self.author.validate()?;
        if self.body.is_empty() {
            return Err(Error::InvalidComment("body is empty".into()));
        }
        let ts = self.ts.unwrap_or_else(Tai64N::now);
        let author_key = self.signing_key.verifying_key();
        let view = CommentCanonicalView {
            change: &self.change,
            author: &self.author,
            author_key: &author_key,
            ts: &ts,
            anchor: &self.anchor,
            body: &self.body,
            reply_to: &self.reply_to,
        };
        let msg = bincode::serialize(&view)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        let sig = self.signing_key.sign(&msg);
        Ok(Comment {
            change: self.change,
            author: self.author,
            author_key,
            ts,
            anchor: self.anchor,
            body: self.body,
            reply_to: self.reply_to,
            sig,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Approved,
    RequestedChanges,
    Commented,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Approval {
    pub change: ChangeId,
    pub reviewer: Identity,
    pub reviewer_key: VerifyingKey,
    pub ts: Tai64N,
    pub verdict: Verdict,
    pub body: Option<String>,
    pub sig: Signature,
}

#[derive(Serialize)]
struct ApprovalCanonicalView<'a> {
    change: &'a ChangeId,
    reviewer: &'a Identity,
    reviewer_key: &'a VerifyingKey,
    ts: &'a Tai64N,
    verdict: &'a Verdict,
    body: &'a Option<String>,
}

impl Approval {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let view = ApprovalCanonicalView {
            change: &self.change,
            reviewer: &self.reviewer,
            reviewer_key: &self.reviewer_key,
            ts: &self.ts,
            verdict: &self.verdict,
            body: &self.body,
        };
        bincode::serialize(&view).expect("canonical serialization is infallible")
    }

    pub fn id(&self) -> Hash {
        Hash::of(&self.canonical_bytes())
    }

    pub fn verify(&self) -> Result<()> {
        self.reviewer.validate()?;
        let msg = self.canonical_bytes();
        self.reviewer_key
            .verify(&msg, &self.sig)
            .map_err(|_| Error::BadSignature)?;
        Ok(())
    }
}

pub struct ApprovalBuilder {
    change: ChangeId,
    reviewer: Identity,
    signing_key: SigningKey,
    ts: Option<Tai64N>,
    verdict: Verdict,
    body: Option<String>,
}

impl ApprovalBuilder {
    pub fn new(
        change: ChangeId,
        reviewer: Identity,
        signing_key: SigningKey,
        verdict: Verdict,
    ) -> Self {
        Self {
            change,
            reviewer,
            signing_key,
            ts: None,
            verdict,
            body: None,
        }
    }

    pub fn ts(mut self, ts: Tai64N) -> Self {
        self.ts = Some(ts);
        self
    }

    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn build(self) -> Result<Approval> {
        self.reviewer.validate()?;
        let ts = self.ts.unwrap_or_else(Tai64N::now);
        let reviewer_key = self.signing_key.verifying_key();
        let view = ApprovalCanonicalView {
            change: &self.change,
            reviewer: &self.reviewer,
            reviewer_key: &reviewer_key,
            ts: &ts,
            verdict: &self.verdict,
            body: &self.body,
        };
        let msg = bincode::serialize(&view)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        let sig = self.signing_key.sign(&msg);
        Ok(Approval {
            change: self.change,
            reviewer: self.reviewer,
            reviewer_key,
            ts,
            verdict: self.verdict,
            body: self.body,
            sig,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::Repository;
    use tempfile::TempDir;

    fn sample_change_id() -> ChangeId {
        ChangeId(Hash::of(b"sample-change"))
    }

    fn sample_human() -> Identity {
        Identity::human("reviewer@example.com", Some("Reviewer".into())).unwrap()
    }

    #[test]
    fn comment_sign_verify_round_trip() {
        let key = SigningKey::generate();
        let author = sample_human();
        let comment = CommentBuilder::new(sample_change_id(), author, key)
            .body("looks good to me")
            .build()
            .unwrap();
        comment.verify().unwrap();
    }

    #[test]
    fn comment_tamper_breaks_signature() {
        let key = SigningKey::generate();
        let author = sample_human();
        let mut comment = CommentBuilder::new(sample_change_id(), author, key)
            .body("ship it")
            .build()
            .unwrap();
        comment.body = "DO NOT ship it".into();
        assert!(matches!(comment.verify(), Err(Error::BadSignature)));
    }

    #[test]
    fn approval_sign_verify_round_trip() {
        let key = SigningKey::generate();
        let reviewer = sample_human();
        let approval = ApprovalBuilder::new(sample_change_id(), reviewer, key, Verdict::Approved)
            .body("ok")
            .build()
            .unwrap();
        approval.verify().unwrap();
    }

    #[test]
    fn verdict_roundtrips_through_serde() {
        for v in [Verdict::Approved, Verdict::RequestedChanges, Verdict::Commented] {
            let bytes = bincode::serialize(&v).unwrap();
            let back: Verdict = bincode::deserialize(&bytes).unwrap();
            assert_eq!(v, back);

            let j = serde_json::to_string(&v).unwrap();
            let back_j: Verdict = serde_json::from_str(&j).unwrap();
            assert_eq!(v, back_j);
        }
    }

    #[test]
    fn review_store_persists_multiple_comments_in_order() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let key = SigningKey::generate();
        let author = sample_human();
        let cid = sample_change_id();

        let c1 = CommentBuilder::new(cid, author.clone(), key.clone())
            .ts(Tai64N(100, 0))
            .body("first")
            .build()
            .unwrap();
        let c2 = CommentBuilder::new(cid, author.clone(), key.clone())
            .ts(Tai64N(200, 0))
            .body("second")
            .build()
            .unwrap();
        let c3 = CommentBuilder::new(cid, author, key)
            .ts(Tai64N(300, 0))
            .body("third")
            .build()
            .unwrap();

        repo.add_comment(&c1).unwrap();
        repo.add_comment(&c2).unwrap();
        repo.add_comment(&c3).unwrap();

        let loaded = repo.comments_for(&cid).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].body, "first");
        assert_eq!(loaded[1].body, "second");
        assert_eq!(loaded[2].body, "third");
    }

    #[test]
    fn add_comment_rejects_tampered() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let key = SigningKey::generate();
        let author = sample_human();
        let cid = sample_change_id();

        let mut c = CommentBuilder::new(cid, author, key)
            .body("ok")
            .build()
            .unwrap();
        c.body = "tampered".into();

        assert!(matches!(repo.add_comment(&c), Err(Error::BadSignature)));
        assert!(repo.comments_for(&cid).unwrap().is_empty());
    }
}
