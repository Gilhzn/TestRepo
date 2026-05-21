use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Identity {
    Human {
        email: String,
        name: Option<String>,
    },
    Agent {
        name: String,
        session_id: String,
        invoker: Box<Identity>,
    },
}

impl Identity {
    pub fn human(email: impl Into<String>, name: Option<String>) -> Result<Self> {
        let id = Self::Human {
            email: email.into(),
            name,
        };
        id.validate()?;
        Ok(id)
    }

    pub fn agent(
        name: impl Into<String>,
        session_id: impl Into<String>,
        invoker: Identity,
    ) -> Result<Self> {
        let id = Self::Agent {
            name: name.into(),
            session_id: session_id.into(),
            invoker: Box::new(invoker),
        };
        id.validate()?;
        Ok(id)
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Human { email, name } => {
                if email.trim().is_empty() {
                    return Err(Error::InvalidIdentity("email is empty"));
                }
                if let Some(n) = name {
                    if n.trim().is_empty() {
                        return Err(Error::InvalidIdentity("name is empty"));
                    }
                }
                Ok(())
            }
            Self::Agent {
                name,
                session_id,
                invoker,
            } => {
                if name.trim().is_empty() {
                    return Err(Error::InvalidIdentity("agent name is empty"));
                }
                if session_id.trim().is_empty() {
                    return Err(Error::InvalidIdentity("agent session_id is empty"));
                }
                if matches!(**invoker, Self::Agent { .. }) {
                    return Err(Error::InvalidIdentity(
                        "agent invoker must be a Human, not another Agent",
                    ));
                }
                invoker.validate()
            }
        }
    }

    pub fn id(&self) -> String {
        match self {
            Self::Human { email, .. } => format!("human:{email}"),
            Self::Agent {
                name, session_id, ..
            } => format!("agent:{name}/{session_id}"),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Human { email, name } => match name {
                Some(n) => format!("{n} <{email}>"),
                None => email.clone(),
            },
            Self::Agent {
                name,
                session_id,
                invoker,
            } => format!(
                "{name} (session {session_id}, invoked by {})",
                invoker.display()
            ),
        }
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_id_is_stable() {
        let h = Identity::human("eyal@example.com", Some("Eyal".into())).unwrap();
        assert_eq!(h.id(), "human:eyal@example.com");
        let h2 = Identity::human("eyal@example.com", None).unwrap();
        assert_eq!(h.id(), h2.id());
    }

    #[test]
    fn agent_id_is_stable() {
        let invoker = Identity::human("eyal@example.com", None).unwrap();
        let a = Identity::agent("claude-code", "sess-abc123", invoker).unwrap();
        assert_eq!(a.id(), "agent:claude-code/sess-abc123");
    }

    #[test]
    fn display_renders_human_readable() {
        let h = Identity::human("eyal@example.com", Some("Eyal".into())).unwrap();
        assert_eq!(h.display(), "Eyal <eyal@example.com>");
        let invoker = Identity::human("eyal@example.com", None).unwrap();
        let a = Identity::agent("claude-code", "sess-1", invoker).unwrap();
        assert!(a.display().contains("claude-code"));
        assert!(a.display().contains("sess-1"));
        assert!(a.display().contains("eyal@example.com"));
    }

    #[test]
    fn rejects_empty_email() {
        assert!(Identity::human("", None).is_err());
        assert!(Identity::human("   ", None).is_err());
    }

    #[test]
    fn rejects_empty_name() {
        assert!(Identity::human("a@b.com", Some("".into())).is_err());
        assert!(Identity::human("a@b.com", Some("   ".into())).is_err());
    }

    #[test]
    fn rejects_empty_agent_fields() {
        let invoker = Identity::human("a@b.com", None).unwrap();
        assert!(Identity::agent("", "sess", invoker.clone()).is_err());
        assert!(Identity::agent("name", "", invoker).is_err());
    }

    #[test]
    fn rejects_agent_invoked_by_agent() {
        let human = Identity::human("a@b.com", None).unwrap();
        let bot = Identity::agent("bot1", "s1", human).unwrap();
        let err = Identity::agent("bot2", "s2", bot);
        assert!(err.is_err());
    }

    #[test]
    fn serde_round_trip_human() {
        let h = Identity::human("eyal@example.com", Some("Eyal".into())).unwrap();
        let bytes = bincode::serialize(&h).unwrap();
        let back: Identity = bincode::deserialize(&bytes).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn serde_round_trip_agent() {
        let invoker = Identity::human("eyal@example.com", None).unwrap();
        let a = Identity::agent("claude", "sess-x", invoker).unwrap();
        let bytes = bincode::serialize(&a).unwrap();
        let back: Identity = bincode::deserialize(&bytes).unwrap();
        assert_eq!(a, back);
    }
}
