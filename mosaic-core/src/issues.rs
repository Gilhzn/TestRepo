//! GitHub-style issue tracker: numbered issues + append-only event streams.
//!
//! Issues and IssueEvents are content-addressed, ed25519-signed artifacts that
//! mirror the canonical-bytes / sig / verify pattern from [`crate::review`].
//! An [`Issue`] is stored as a single JSON document while its [`IssueEvent`]s
//! (comments, status changes, (un)labels, cross-references) are appended to a
//! per-issue JSONL stream under `<root>/.mosaic/issues/` (see
//! [`crate::repo::Repository`]).

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::m1::change::Tai64N;
use crate::m1::identity::Identity;
use crate::m1::signing::{Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueStatus {
    Open,
    Closed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub author: Identity,
    pub author_key: VerifyingKey,
    pub ts: Tai64N,
    pub labels: Vec<String>,
    pub sig: Signature,
}

#[derive(Serialize)]
struct IssueCanonicalView<'a> {
    number: u64,
    title: &'a str,
    body: &'a str,
    author: &'a Identity,
    author_key: &'a VerifyingKey,
    ts: &'a Tai64N,
    labels: &'a [String],
}

impl Issue {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let view = IssueCanonicalView {
            number: self.number,
            title: &self.title,
            body: &self.body,
            author: &self.author,
            author_key: &self.author_key,
            ts: &self.ts,
            labels: &self.labels,
        };
        bincode::serialize(&view).expect("canonical serialization is infallible")
    }

    pub fn id(&self) -> Hash {
        Hash::of(&self.canonical_bytes())
    }

    pub fn verify(&self) -> Result<()> {
        self.author.validate()?;
        if self.title.is_empty() {
            return Err(Error::InvalidIssue("title is empty".into()));
        }
        let msg = self.canonical_bytes();
        self.author_key
            .verify(&msg, &self.sig)
            .map_err(|_| Error::BadSignature)?;
        Ok(())
    }
}

pub struct IssueBuilder {
    number: u64,
    title: String,
    body: String,
    author: Identity,
    signing_key: SigningKey,
    ts: Option<Tai64N>,
    labels: Vec<String>,
}

impl IssueBuilder {
    pub fn new(number: u64, author: Identity, signing_key: SigningKey) -> Self {
        Self {
            number,
            title: String::new(),
            body: String::new(),
            author,
            signing_key,
            ts: None,
            labels: Vec::new(),
        }
    }

    pub fn ts(mut self, ts: Tai64N) -> Self {
        self.ts = Some(ts);
        self
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.labels.push(label.into());
        self
    }

    pub fn labels(mut self, labels: impl IntoIterator<Item = String>) -> Self {
        self.labels.extend(labels);
        self
    }

    pub fn build(self) -> Result<Issue> {
        self.author.validate()?;
        if self.title.is_empty() {
            return Err(Error::InvalidIssue("title is empty".into()));
        }
        let ts = self.ts.unwrap_or_else(Tai64N::now);
        let author_key = self.signing_key.verifying_key();
        let view = IssueCanonicalView {
            number: self.number,
            title: &self.title,
            body: &self.body,
            author: &self.author,
            author_key: &author_key,
            ts: &ts,
            labels: &self.labels,
        };
        let msg = bincode::serialize(&view).map_err(|e| Error::Serialization(e.to_string()))?;
        let sig = self.signing_key.sign(&msg);
        Ok(Issue {
            number: self.number,
            title: self.title,
            body: self.body,
            author: self.author,
            author_key,
            ts,
            labels: self.labels,
            sig,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum IssueEventKind {
    Comment { body: String },
    StatusChanged { to: IssueStatus },
    Labeled { label: String },
    Unlabeled { label: String },
    /// cross-reference a change id
    Referenced { change: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IssueEvent {
    pub number: u64,
    pub author: Identity,
    pub author_key: VerifyingKey,
    pub ts: Tai64N,
    pub kind: IssueEventKind,
    pub sig: Signature,
}

#[derive(Serialize)]
struct IssueEventCanonicalView<'a> {
    number: u64,
    author: &'a Identity,
    author_key: &'a VerifyingKey,
    ts: &'a Tai64N,
    kind: &'a IssueEventKind,
}

impl IssueEvent {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let view = IssueEventCanonicalView {
            number: self.number,
            author: &self.author,
            author_key: &self.author_key,
            ts: &self.ts,
            kind: &self.kind,
        };
        bincode::serialize(&view).expect("canonical serialization is infallible")
    }

    pub fn id(&self) -> Hash {
        Hash::of(&self.canonical_bytes())
    }

    pub fn verify(&self) -> Result<()> {
        self.author.validate()?;
        if let IssueEventKind::Comment { body } = &self.kind {
            if body.is_empty() {
                return Err(Error::InvalidIssue("comment body is empty".into()));
            }
        }
        let msg = self.canonical_bytes();
        self.author_key
            .verify(&msg, &self.sig)
            .map_err(|_| Error::BadSignature)?;
        Ok(())
    }
}

pub struct IssueEventBuilder {
    number: u64,
    author: Identity,
    signing_key: SigningKey,
    ts: Option<Tai64N>,
    kind: IssueEventKind,
}

impl IssueEventBuilder {
    pub fn new(
        number: u64,
        author: Identity,
        signing_key: SigningKey,
        kind: IssueEventKind,
    ) -> Self {
        Self {
            number,
            author,
            signing_key,
            ts: None,
            kind,
        }
    }

    pub fn ts(mut self, ts: Tai64N) -> Self {
        self.ts = Some(ts);
        self
    }

    pub fn build(self) -> Result<IssueEvent> {
        self.author.validate()?;
        if let IssueEventKind::Comment { body } = &self.kind {
            if body.is_empty() {
                return Err(Error::InvalidIssue("comment body is empty".into()));
            }
        }
        let ts = self.ts.unwrap_or_else(Tai64N::now);
        let author_key = self.signing_key.verifying_key();
        let view = IssueEventCanonicalView {
            number: self.number,
            author: &self.author,
            author_key: &author_key,
            ts: &ts,
            kind: &self.kind,
        };
        let msg = bincode::serialize(&view).map_err(|e| Error::Serialization(e.to_string()))?;
        let sig = self.signing_key.sign(&msg);
        Ok(IssueEvent {
            number: self.number,
            author: self.author,
            author_key,
            ts,
            kind: self.kind,
            sig,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::Repository;
    use tempfile::TempDir;

    fn sample_human() -> Identity {
        Identity::human("filer@example.com", Some("Filer".into())).unwrap()
    }

    #[test]
    fn issue_sign_verify_round_trip() {
        let key = SigningKey::generate();
        let author = sample_human();
        let issue = IssueBuilder::new(1, author, key)
            .title("login crashes on empty password")
            .body("steps to reproduce ...")
            .label("bug")
            .build()
            .unwrap();
        issue.verify().unwrap();
    }

    #[test]
    fn issue_tamper_breaks_signature() {
        let key = SigningKey::generate();
        let author = sample_human();
        let mut issue = IssueBuilder::new(1, author, key)
            .title("real title")
            .body("body")
            .build()
            .unwrap();
        issue.title = "malicious retitle".into();
        assert!(matches!(issue.verify(), Err(Error::BadSignature)));
    }

    #[test]
    fn event_sign_verify_round_trip() {
        let key = SigningKey::generate();
        let author = sample_human();
        let event = IssueEventBuilder::new(
            1,
            author,
            key,
            IssueEventKind::Comment {
                body: "i can repro this too".into(),
            },
        )
        .build()
        .unwrap();
        event.verify().unwrap();
    }

    #[test]
    fn issue_store_allocates_monotonic_numbers() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let key = SigningKey::generate();
        let author = sample_human();

        let i1 = repo
            .create_issue("first", "b1", author.clone(), key.clone(), Vec::new())
            .unwrap();
        let i2 = repo
            .create_issue("second", "b2", author.clone(), key.clone(), Vec::new())
            .unwrap();
        let i3 = repo
            .create_issue("third", "b3", author, key, Vec::new())
            .unwrap();

        assert_eq!(i1.number, 1);
        assert_eq!(i2.number, 2);
        assert_eq!(i3.number, 3);

        let listed = repo.list_issues().unwrap();
        assert_eq!(listed.len(), 3);
    }

    #[test]
    fn issue_status_reflects_latest_status_change() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let key = SigningKey::generate();
        let author = sample_human();

        let issue = repo
            .create_issue("flaky test", "body", author.clone(), key.clone(), Vec::new())
            .unwrap();
        assert_eq!(repo.issue_status(issue.number).unwrap(), IssueStatus::Open);

        let close = IssueEventBuilder::new(
            issue.number,
            author.clone(),
            key.clone(),
            IssueEventKind::StatusChanged {
                to: IssueStatus::Closed,
            },
        )
        .ts(Tai64N(100, 0))
        .build()
        .unwrap();
        repo.add_issue_event(&close).unwrap();
        assert_eq!(repo.issue_status(issue.number).unwrap(), IssueStatus::Closed);

        let reopen = IssueEventBuilder::new(
            issue.number,
            author,
            key,
            IssueEventKind::StatusChanged {
                to: IssueStatus::Open,
            },
        )
        .ts(Tai64N(200, 0))
        .build()
        .unwrap();
        repo.add_issue_event(&reopen).unwrap();
        assert_eq!(repo.issue_status(issue.number).unwrap(), IssueStatus::Open);
    }

    #[test]
    fn issue_events_returns_events_in_append_order() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let key = SigningKey::generate();
        let author = sample_human();

        let issue = repo
            .create_issue("track me", "body", author.clone(), key.clone(), Vec::new())
            .unwrap();

        let e1 = IssueEventBuilder::new(
            issue.number,
            author.clone(),
            key.clone(),
            IssueEventKind::Comment {
                body: "first".into(),
            },
        )
        .ts(Tai64N(1, 0))
        .build()
        .unwrap();
        let e2 = IssueEventBuilder::new(
            issue.number,
            author.clone(),
            key.clone(),
            IssueEventKind::Labeled {
                label: "bug".into(),
            },
        )
        .ts(Tai64N(2, 0))
        .build()
        .unwrap();
        let e3 = IssueEventBuilder::new(
            issue.number,
            author,
            key,
            IssueEventKind::Comment {
                body: "third".into(),
            },
        )
        .ts(Tai64N(3, 0))
        .build()
        .unwrap();

        repo.add_issue_event(&e1).unwrap();
        repo.add_issue_event(&e2).unwrap();
        repo.add_issue_event(&e3).unwrap();

        let loaded = repo.issue_events(issue.number).unwrap();
        assert_eq!(loaded.len(), 3);
        assert!(matches!(
            &loaded[0].kind,
            IssueEventKind::Comment { body } if body == "first"
        ));
        assert!(matches!(
            &loaded[1].kind,
            IssueEventKind::Labeled { label } if label == "bug"
        ));
        assert!(matches!(
            &loaded[2].kind,
            IssueEventKind::Comment { body } if body == "third"
        ));
    }

    #[test]
    fn add_issue_event_rejects_tampered() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let key = SigningKey::generate();
        let author = sample_human();

        let issue = repo
            .create_issue("track me", "body", author.clone(), key.clone(), Vec::new())
            .unwrap();

        let mut event = IssueEventBuilder::new(
            issue.number,
            author,
            key,
            IssueEventKind::Comment { body: "ok".into() },
        )
        .build()
        .unwrap();
        event.kind = IssueEventKind::Comment {
            body: "tampered".into(),
        };

        assert!(matches!(
            repo.add_issue_event(&event),
            Err(Error::BadSignature)
        ));
        assert!(repo.issue_events(issue.number).unwrap().is_empty());
    }
}
