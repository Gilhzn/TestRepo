use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::m1::change::{Change, Tai64N};
use crate::m1::identity::Identity;
use crate::m1::signing::{Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attestation {
    pub agent: Identity,
    pub session_pubkey: VerifyingKey,
    pub valid_from: Tai64N,
    pub valid_until: Tai64N,
    pub invoker_pubkey: VerifyingKey,
    pub sig_by_invoker: Signature,
}

#[derive(Serialize)]
struct CanonicalView<'a> {
    agent: &'a Identity,
    session_pubkey: &'a VerifyingKey,
    valid_from: &'a Tai64N,
    valid_until: &'a Tai64N,
    invoker_pubkey: &'a VerifyingKey,
}

impl Attestation {
    pub fn issue(
        agent: Identity,
        session_pubkey: VerifyingKey,
        valid_from: Tai64N,
        valid_until: Tai64N,
        invoker_key: &SigningKey,
    ) -> Result<Self> {
        match &agent {
            Identity::Agent { invoker, .. } => {
                if !matches!(**invoker, Identity::Human { .. }) {
                    return Err(Error::AttestationInvalidAgent);
                }
            }
            Identity::Human { .. } => {
                return Err(Error::AttestationInvalidAgent);
            }
        }

        let invoker_pubkey = invoker_key.verifying_key();

        let view = CanonicalView {
            agent: &agent,
            session_pubkey: &session_pubkey,
            valid_from: &valid_from,
            valid_until: &valid_until,
            invoker_pubkey: &invoker_pubkey,
        };

        let msg = bincode::serialize(&view)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        let sig_by_invoker = invoker_key.sign(&msg);

        Ok(Self {
            agent,
            session_pubkey,
            valid_from,
            valid_until,
            invoker_pubkey,
            sig_by_invoker,
        })
    }

    pub fn verify(&self) -> Result<()> {
        self.agent.validate()?;

        match &self.agent {
            Identity::Agent { invoker, .. } => {
                if !matches!(**invoker, Identity::Human { .. }) {
                    return Err(Error::AttestationInvalidAgent);
                }
            }
            Identity::Human { .. } => {
                return Err(Error::AttestationInvalidAgent);
            }
        }

        if self.valid_from >= self.valid_until {
            return Err(Error::AttestationInvalidWindow);
        }

        let msg = self.canonical_bytes();
        self.invoker_pubkey
            .verify(&msg, &self.sig_by_invoker)
            .map_err(|_| Error::BadSignature)?;

        Ok(())
    }

    pub fn authorize(&self, change: &Change) -> Result<()> {
        self.verify()?;

        if change.author != self.agent {
            return Err(Error::AttestationAgentMismatch);
        }

        if change.author_key != self.session_pubkey {
            return Err(Error::AttestationKeyMismatch);
        }

        if change.ts < self.valid_from {
            return Err(Error::AttestationNotYetValid {
                now: change.ts,
                valid_from: self.valid_from,
            });
        }

        if change.ts > self.valid_until {
            return Err(Error::AttestationExpired {
                now: change.ts,
                valid_until: self.valid_until,
            });
        }

        Ok(())
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let view = CanonicalView {
            agent: &self.agent,
            session_pubkey: &self.session_pubkey,
            valid_from: &self.valid_from,
            valid_until: &self.valid_until,
            invoker_pubkey: &self.invoker_pubkey,
        };
        bincode::serialize(&view).expect("canonical serialization is infallible")
    }

    pub fn id(&self) -> Hash {
        Hash::of(&self.canonical_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m1::change::ChangeBuilder;
    use crate::m1::change::FileChange;
    use crate::m1::change::FileKind;

    fn sample_human() -> Identity {
        Identity::human("eyal@example.com", None).unwrap()
    }

    fn sample_agent(invoker: Identity) -> Identity {
        Identity::agent("claude-code", "sess-1", invoker).unwrap()
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
    fn issue_then_verify_round_trip() {
        let human = sample_human();
        let agent = sample_agent(human);
        let invoker_key = SigningKey::generate();
        let session_key = SigningKey::generate();
        let session_pubkey = session_key.verifying_key();

        let now = Tai64N::now();
        let valid_from = Tai64N(now.0, 0);
        let valid_until = Tai64N(now.0 + 3600, 0);

        let att = Attestation::issue(agent, session_pubkey, valid_from, valid_until, &invoker_key)
            .unwrap();
        att.verify().unwrap();
    }

    #[test]
    fn verify_rejects_tampered_validity() {
        let human = sample_human();
        let agent = sample_agent(human);
        let invoker_key = SigningKey::generate();
        let session_key = SigningKey::generate();
        let session_pubkey = session_key.verifying_key();

        let now = Tai64N::now();
        let valid_from = Tai64N(now.0, 0);
        let valid_until = Tai64N(now.0 + 3600, 0);

        let mut att = Attestation::issue(agent, session_pubkey, valid_from, valid_until, &invoker_key)
            .unwrap();

        att.valid_until.0 = now.0 - 1;
        assert!(att.verify().is_err());
    }

    #[test]
    fn verify_rejects_wrong_invoker_key() {
        let human = sample_human();
        let agent = sample_agent(human);
        let invoker_key = SigningKey::generate();
        let wrong_key = SigningKey::generate();
        let session_key = SigningKey::generate();
        let session_pubkey = session_key.verifying_key();

        let now = Tai64N::now();
        let valid_from = Tai64N(now.0, 0);
        let valid_until = Tai64N(now.0 + 3600, 0);

        let mut att = Attestation::issue(agent, session_pubkey, valid_from, valid_until, &invoker_key)
            .unwrap();

        att.invoker_pubkey = wrong_key.verifying_key();
        assert!(att.verify().is_err());
    }

    #[test]
    fn verify_rejects_non_agent_identity() {
        let human = sample_human();
        let invoker_key = SigningKey::generate();
        let session_key = SigningKey::generate();
        let session_pubkey = session_key.verifying_key();

        let now = Tai64N::now();
        let valid_from = Tai64N(now.0, 0);
        let valid_until = Tai64N(now.0 + 3600, 0);

        let err = Attestation::issue(human, session_pubkey, valid_from, valid_until, &invoker_key);
        assert!(err.is_err());
    }

    #[test]
    fn authorize_happy_path() {
        let human = sample_human();
        let agent = sample_agent(human);
        let invoker_key = SigningKey::generate();
        let session_key = SigningKey::generate();
        let session_pubkey = session_key.verifying_key();

        let now = Tai64N::now();
        let valid_from = Tai64N(now.0 - 10, 0);
        let valid_until = Tai64N(now.0 + 3600, 0);

        let att = Attestation::issue(
            agent.clone(),
            session_pubkey,
            valid_from,
            valid_until,
            &invoker_key,
        )
        .unwrap();

        let change = ChangeBuilder::new(agent, session_key)
            .ts(now)
            .intent("agent work")
            .file(sample_file())
            .build()
            .unwrap();

        att.authorize(&change).unwrap();
    }

    #[test]
    fn authorize_rejects_change_with_wrong_session_key() {
        let human = sample_human();
        let agent = sample_agent(human);
        let invoker_key = SigningKey::generate();
        let session_key = SigningKey::generate();
        let session_pubkey = session_key.verifying_key();

        let now = Tai64N::now();
        let valid_from = Tai64N(now.0 - 10, 0);
        let valid_until = Tai64N(now.0 + 3600, 0);

        let att = Attestation::issue(agent.clone(), session_pubkey, valid_from, valid_until, &invoker_key)
            .unwrap();

        let wrong_key = SigningKey::generate();
        let change = ChangeBuilder::new(agent, wrong_key)
            .ts(now)
            .intent("agent work")
            .file(sample_file())
            .build()
            .unwrap();

        assert!(matches!(
            att.authorize(&change),
            Err(Error::AttestationKeyMismatch)
        ));
    }

    #[test]
    fn authorize_rejects_change_outside_validity_window() {
        let human = sample_human();
        let agent = sample_agent(human);
        let invoker_key = SigningKey::generate();
        let session_key = SigningKey::generate();
        let session_pubkey = session_key.verifying_key();

        let now = Tai64N::now();
        let valid_from = Tai64N(now.0 + 100, 0);
        let valid_until = Tai64N(now.0 + 3600, 0);

        let att =
            Attestation::issue(agent.clone(), session_pubkey, valid_from, valid_until, &invoker_key)
                .unwrap();

        let change = ChangeBuilder::new(agent, session_key.clone())
            .ts(now)
            .intent("agent work")
            .file(sample_file())
            .build()
            .unwrap();

        assert!(matches!(
            att.authorize(&change),
            Err(Error::AttestationNotYetValid { .. })
        ));

        let expired_from = Tai64N(now.0 - 3600, 0);
        let expired_until = Tai64N(now.0 - 100, 0);

        let agent2 = sample_agent(sample_human());
        let att2 = Attestation::issue(
            agent2.clone(),
            session_pubkey,
            expired_from,
            expired_until,
            &invoker_key,
        )
        .unwrap();

        let change2 = ChangeBuilder::new(agent2, session_key)
            .ts(now)
            .intent("agent work")
            .file(sample_file())
            .build()
            .unwrap();

        assert!(matches!(
            att2.authorize(&change2),
            Err(Error::AttestationExpired { .. })
        ));
    }

    #[test]
    fn authorize_rejects_change_with_mismatched_agent_identity() {
        let human1 = sample_human();
        let agent1 = sample_agent(human1);
        let human2 = Identity::human("dana@example.com", None).unwrap();
        let agent2 = Identity::agent("claude-code", "sess-2", human2).unwrap();
        assert_ne!(agent1, agent2);

        let invoker_key = SigningKey::generate();
        let session_key = SigningKey::generate();
        let session_pubkey = session_key.verifying_key();

        let now = Tai64N::now();
        let valid_from = Tai64N(now.0 - 10, 0);
        let valid_until = Tai64N(now.0 + 3600, 0);

        let att = Attestation::issue(agent1, session_pubkey, valid_from, valid_until, &invoker_key)
            .unwrap();

        let change = ChangeBuilder::new(agent2, session_key)
            .ts(now)
            .intent("agent work")
            .file(sample_file())
            .build()
            .unwrap();

        assert!(matches!(
            att.authorize(&change),
            Err(Error::AttestationAgentMismatch)
        ));
    }

    #[test]
    fn canonical_bytes_excludes_signature() {
        let human = sample_human();
        let agent = sample_agent(human);
        let invoker_key = SigningKey::generate();
        let session_key = SigningKey::generate();
        let session_pubkey = session_key.verifying_key();

        let now = Tai64N::now();
        let valid_from = Tai64N(now.0, 0);
        let valid_until = Tai64N(now.0 + 3600, 0);

        let mut att = Attestation::issue(agent, session_pubkey, valid_from, valid_until, &invoker_key)
            .unwrap();

        let canonical_before = att.canonical_bytes();

        att.sig_by_invoker =
            invoker_key.sign(b"some other message");

        let canonical_after = att.canonical_bytes();

        assert_eq!(canonical_before, canonical_after);
    }

    #[test]
    fn attestation_id_is_content_addressed() {
        let human = sample_human();
        let agent = sample_agent(human);
        let invoker_key = SigningKey::generate();
        let session_key = SigningKey::generate();
        let session_pubkey = session_key.verifying_key();

        let now = Tai64N::now();
        let valid_from = Tai64N(now.0, 0);
        let valid_until = Tai64N(now.0 + 3600, 0);

        let att1 = Attestation::issue(
            agent.clone(),
            session_pubkey,
            valid_from,
            valid_until,
            &invoker_key,
        )
        .unwrap();
        let att2 = Attestation::issue(
            agent,
            session_pubkey,
            valid_from,
            valid_until,
            &invoker_key,
        )
        .unwrap();

        assert_eq!(att1.id(), att2.id());

        let different_until = Tai64N(now.0 + 7200, 0);
        let att3 = Attestation::issue(
            sample_agent(sample_human()),
            session_pubkey,
            valid_from,
            different_until,
            &invoker_key,
        )
        .unwrap();

        assert_ne!(att1.id(), att3.id());
    }
}
