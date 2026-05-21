//! Append-only audit log.
//!
//! Every signed action the system performs lands here so a compliance team
//! can prove later: who did what, when, on whose behalf, with what session.
//!
//! Specifically supports two questions enterprises ask:
//!
//!  - "Replay everything Agent X did in session Y"  → `replay_session()`
//!  - "Give me an audit report between dates A and B" → `range()`
//!
//! On-disk layout: `<repo>/.mosaic/audit/<YYYY-MM>.jsonl`. Append-only —
//! never rewritten. Each line is a self-signed `AuditEvent` so tampering
//! is detectable by re-verifying.

use crate::error::{Error, Result};
use crate::hash::{Hash, Hasher};
use crate::m1::change::{ChangeId, Tai64N};
use crate::m1::identity::Identity;
use crate::m1::signing::{Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AuditAction {
    Push {
        changes: Vec<ChangeId>,
        branch: String,
    },
    Comment {
        change: ChangeId,
        comment_id: Hash,
    },
    Approval {
        change: ChangeId,
        verdict: String,
    },
    BranchAdvanced {
        branch: String,
        new_tips: Vec<Hash>,
    },
    BranchAbandoned {
        branch: String,
    },
    AttestationIssued {
        attestation_id: Hash,
        for_agent: String,
    },
    PolicyChanged {
        kind: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEvent {
    pub ts: Tai64N,
    pub actor: Identity,
    pub actor_key: VerifyingKey,
    pub session_id: Option<String>,
    pub action: AuditAction,
    pub sig: Signature,
}

#[derive(Serialize)]
struct CanonicalView<'a> {
    ts: &'a Tai64N,
    actor: &'a Identity,
    actor_key: &'a VerifyingKey,
    session_id: &'a Option<String>,
    action: &'a AuditAction,
}

impl AuditEvent {
    pub fn issue(
        actor: Identity,
        session_id: Option<String>,
        action: AuditAction,
        signing_key: &SigningKey,
    ) -> Result<Self> {
        let actor_key = signing_key.verifying_key();
        let ts = Tai64N::now();
        let view = CanonicalView {
            ts: &ts,
            actor: &actor,
            actor_key: &actor_key,
            session_id: &session_id,
            action: &action,
        };
        let bytes = bincode::serialize(&view)
            .map_err(|e| Error::Serialization(format!("audit canonical: {e}")))?;
        let sig = signing_key.sign(&bytes);
        Ok(Self {
            ts,
            actor,
            actor_key,
            session_id,
            action,
            sig,
        })
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let view = CanonicalView {
            ts: &self.ts,
            actor: &self.actor,
            actor_key: &self.actor_key,
            session_id: &self.session_id,
            action: &self.action,
        };
        bincode::serialize(&view)
            .map_err(|e| Error::Serialization(format!("audit canonical: {e}")))
    }

    pub fn id(&self) -> Result<Hash> {
        let mut h = Hasher::new();
        h.update(b"mosaic.audit.v1");
        h.update(&self.canonical_bytes()?);
        Ok(h.finalize())
    }

    pub fn verify(&self) -> Result<()> {
        let bytes = self.canonical_bytes()?;
        self.actor_key.verify(&bytes, &self.sig)
    }
}

pub struct AuditLog {
    root: PathBuf,
}

impl AuditLog {
    pub fn open(repo_root: impl AsRef<Path>) -> Result<Self> {
        let root = repo_root.as_ref().join(".mosaic").join("audit");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn file_for(&self, ts: &Tai64N) -> PathBuf {
        // Bucket by year-month using a rough seconds → date conversion.
        // For simplicity, bucket by the upper bits of seconds; precise
        // calendar mapping would require chrono.
        let bucket = ts.0 / (30 * 24 * 3600); // ~monthly
        self.root.join(format!("{bucket:08}.jsonl"))
    }

    pub fn append(&self, event: &AuditEvent) -> Result<()> {
        event.verify()?;
        let path = self.file_for(&event.ts);
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let line = serde_json::to_string(event)
            .map_err(|e| Error::Serialization(format!("audit append: {e}")))?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }

    fn all_events(&self) -> Result<Vec<AuditEvent>> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        let mut files: Vec<_> = fs::read_dir(&self.root)?
            .filter_map(|r| r.ok())
            .collect();
        files.sort_by_key(|e| e.file_name());
        for entry in files {
            let f = fs::File::open(entry.path())?;
            for line in BufReader::new(f).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let ev: AuditEvent = serde_json::from_str(&line)
                    .map_err(|e| Error::Serialization(format!("audit read: {e}")))?;
                out.push(ev);
            }
        }
        Ok(out)
    }

    pub fn replay_session(&self, session_id: &str) -> Result<Vec<AuditEvent>> {
        Ok(self
            .all_events()?
            .into_iter()
            .filter(|e| e.session_id.as_deref() == Some(session_id))
            .collect())
    }

    pub fn range(&self, from: u64, to: u64) -> Result<Vec<AuditEvent>> {
        Ok(self
            .all_events()?
            .into_iter()
            .filter(|e| e.ts.0 >= from && e.ts.0 <= to)
            .collect())
    }

    pub fn by_actor(&self, actor_id: &str) -> Result<Vec<AuditEvent>> {
        Ok(self
            .all_events()?
            .into_iter()
            .filter(|e| e.actor.id() == actor_id)
            .collect())
    }

    pub fn sessions(&self) -> Result<BTreeSet<String>> {
        Ok(self
            .all_events()?
            .into_iter()
            .filter_map(|e| e.session_id)
            .collect())
    }

    pub fn count(&self) -> Result<usize> {
        Ok(self.all_events()?.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn human() -> (Identity, SigningKey) {
        (
            Identity::human("dev@example.com", None).unwrap(),
            SigningKey::generate(),
        )
    }

    fn agent_id(invoker: Identity, session: &str) -> Identity {
        Identity::agent("claude-code", session, invoker).unwrap()
    }

    #[test]
    fn issue_then_verify_round_trip() {
        let (idn, key) = human();
        let event = AuditEvent::issue(
            idn,
            None,
            AuditAction::BranchAbandoned {
                branch: "old".into(),
            },
            &key,
        )
        .unwrap();
        assert!(event.verify().is_ok());
    }

    #[test]
    fn tampering_breaks_signature() {
        let (idn, key) = human();
        let mut event = AuditEvent::issue(
            idn,
            None,
            AuditAction::BranchAbandoned {
                branch: "old".into(),
            },
            &key,
        )
        .unwrap();
        event.action = AuditAction::BranchAbandoned {
            branch: "different".into(),
        };
        assert!(event.verify().is_err());
    }

    #[test]
    fn append_and_replay_session() {
        let dir = TempDir::new().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        let (human_id, human_key) = human();
        let agent_a = agent_id(human_id.clone(), "sess-1");
        let agent_b = agent_id(human_id.clone(), "sess-2");

        for (actor, session, branch) in [
            (agent_a.clone(), "sess-1", "feat-a"),
            (agent_a.clone(), "sess-1", "feat-a"),
            (agent_b.clone(), "sess-2", "feat-b"),
            (agent_a.clone(), "sess-1", "feat-a"),
        ] {
            let ev = AuditEvent::issue(
                actor,
                Some(session.into()),
                AuditAction::BranchAdvanced {
                    branch: branch.into(),
                    new_tips: vec![],
                },
                &human_key,
            )
            .unwrap();
            log.append(&ev).unwrap();
        }

        let sess1 = log.replay_session("sess-1").unwrap();
        assert_eq!(sess1.len(), 3);
        let sess2 = log.replay_session("sess-2").unwrap();
        assert_eq!(sess2.len(), 1);

        let sessions = log.sessions().unwrap();
        assert!(sessions.contains("sess-1"));
        assert!(sessions.contains("sess-2"));
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn by_actor_filters_correctly() {
        let dir = TempDir::new().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        let (alice, alice_key) = (
            Identity::human("alice@example.com", None).unwrap(),
            SigningKey::generate(),
        );
        let (bob, bob_key) = (
            Identity::human("bob@example.com", None).unwrap(),
            SigningKey::generate(),
        );
        log.append(
            &AuditEvent::issue(
                alice.clone(),
                None,
                AuditAction::BranchAbandoned {
                    branch: "x".into(),
                },
                &alice_key,
            )
            .unwrap(),
        )
        .unwrap();
        log.append(
            &AuditEvent::issue(
                bob.clone(),
                None,
                AuditAction::BranchAbandoned {
                    branch: "y".into(),
                },
                &bob_key,
            )
            .unwrap(),
        )
        .unwrap();
        let alices = log.by_actor("human:alice@example.com").unwrap();
        assert_eq!(alices.len(), 1);
        let bobs = log.by_actor("human:bob@example.com").unwrap();
        assert_eq!(bobs.len(), 1);
    }

    #[test]
    fn refuses_to_append_unsigned() {
        let dir = TempDir::new().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        let (idn, key) = human();
        let mut event = AuditEvent::issue(
            idn,
            None,
            AuditAction::BranchAbandoned {
                branch: "x".into(),
            },
            &key,
        )
        .unwrap();
        event.action = AuditAction::BranchAbandoned {
            branch: "evil".into(),
        };
        assert!(log.append(&event).is_err());
    }

    #[test]
    fn empty_log_returns_empty_results() {
        let dir = TempDir::new().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        assert_eq!(log.count().unwrap(), 0);
        assert!(log.replay_session("any").unwrap().is_empty());
        assert!(log.by_actor("any").unwrap().is_empty());
    }
}
