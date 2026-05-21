use crate::session::Session;
use crate::speculation::Speculation;
use mosaic_core::ast::Lang;
use mosaic_core::error::{Error, Result};
use mosaic_core::m1::change::ChangeId;
use mosaic_core::m1::identity::Identity;
use mosaic_core::m1::signing::SigningKey;
use mosaic_core::m1_dag::refs::Frontier;
use mosaic_core::repo::Repository;
use mosaic_core::semantic::{analyze_three_way, ThreeWaySemanticReport};
use std::path::{Path, PathBuf};

/// Primary entry point for an agent (or any program) using Mosaic.
pub struct MosaicAgent {
    root: PathBuf,
    identity: Identity,
    signing_key: SigningKey,
}

impl MosaicAgent {
    /// Initialize a fresh repository at `path` and persist the given identity.
    pub fn init(
        path: impl AsRef<Path>,
        identity: Identity,
        signing_key: SigningKey,
    ) -> Result<Self> {
        let repo = Repository::init(path.as_ref())?;
        repo.save_identity(&identity, &signing_key)?;
        Ok(Self {
            root: path.as_ref().to_path_buf(),
            identity,
            signing_key,
        })
    }

    /// Attach to an existing repository, using the identity it has on disk.
    pub fn attach(path: impl AsRef<Path>) -> Result<Self> {
        let repo = Repository::open(path.as_ref())?;
        let (identity, signing_key) = repo.load_identity()?;
        Ok(Self {
            root: path.as_ref().to_path_buf(),
            identity,
            signing_key,
        })
    }

    /// Attach to an existing repository, overriding the stored identity with
    /// a per-session agent identity. Use this when an agent is invoked
    /// inside a human-owned repository: the human's identity stays the
    /// default but each agent run is signed under its own identity.
    pub fn attach_as_agent(
        path: impl AsRef<Path>,
        agent_identity: Identity,
        signing_key: SigningKey,
    ) -> Result<Self> {
        // Confirm the repo is openable.
        let _ = Repository::open(path.as_ref())?;
        // Validate that the override identity really is an agent.
        match &agent_identity {
            Identity::Agent { .. } => {}
            _ => {
                return Err(Error::InvalidIdentity(
                    "attach_as_agent requires an Agent identity",
                ))
            }
        }
        Ok(Self {
            root: path.as_ref().to_path_buf(),
            identity: agent_identity,
            signing_key,
        })
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Begin a new editing session. The session collects file edits in
    /// memory; calling `commit` writes them to the repository as one
    /// atomic, signed `Change` whose parents are the supplied branch's tips.
    pub fn begin_session(&self, intent: impl Into<String>) -> Result<Session> {
        self.begin_session_on("main", intent)
    }

    pub fn begin_session_on(
        &self,
        branch: impl Into<String>,
        intent: impl Into<String>,
    ) -> Result<Session> {
        Session::open(
            self.root.clone(),
            self.identity.clone(),
            self.signing_key.clone(),
            branch.into(),
            intent.into(),
        )
    }

    /// List all branches.
    pub fn branches(&self) -> Result<Vec<String>> {
        let repo = self.open_repo()?;
        repo.refs().list()
    }

    /// Fetch the tip frontier of a branch.
    pub fn branch(&self, name: &str) -> Result<Frontier> {
        let repo = self.open_repo()?;
        repo.refs().get(name)
    }

    /// Recent change ids on a branch, oldest to newest. Returns an empty
    /// list if the branch does not yet exist.
    pub fn history_on(&self, branch: &str) -> Result<Vec<ChangeId>> {
        let repo = self.open_repo()?;
        let tips = match repo.refs().get(branch) {
            Ok(f) => f,
            Err(Error::RefNotFound(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let empty = Frontier::default();
        Ok(mosaic_core::sync::missing_changes_for(&repo, &empty, &tips))
    }

    /// Begin a speculative branch off `base_branch`. The agent can commit
    /// onto it without disturbing other branches; later it can `promote` to
    /// publish the work or `discard` to throw it away.
    pub fn try_branch(
        &self,
        speculation_name: impl Into<String>,
        base_branch: &str,
    ) -> Result<Speculation> {
        Speculation::create(self, speculation_name.into(), base_branch)
    }

    /// Read a file's current content from disk, in the working copy
    /// associated with `branch`. (For now this is a flat checkout: it just
    /// reads from the repository root path. A virtual filesystem lands in M7.)
    pub fn read_file(&self, relative: &str) -> Result<Vec<u8>> {
        let path = self.root.join(relative);
        std::fs::read(path).map_err(Error::Io)
    }

    /// Run the semantic analyzer on a base/ours/theirs trio for a given file.
    /// Useful for an agent that wants to consult Mosaic before deciding how
    /// to resolve a merge.
    pub fn analyze_three_way(
        &self,
        lang: Lang,
        base: &[u8],
        ours: &[u8],
        theirs: &[u8],
    ) -> Result<ThreeWaySemanticReport> {
        analyze_three_way(lang, base, ours, theirs)
    }

    pub(crate) fn open_repo(&self) -> Result<Repository> {
        Repository::open(&self.root)
    }

    pub(crate) fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn human() -> (Identity, SigningKey) {
        (
            Identity::human("eyal@example.com", Some("Eyal".into())).unwrap(),
            SigningKey::generate(),
        )
    }

    #[test]
    fn init_persists_identity_for_attach() {
        let dir = TempDir::new().unwrap();
        let (idn, key) = human();
        let agent = MosaicAgent::init(dir.path(), idn.clone(), key).unwrap();
        assert_eq!(agent.identity(), &idn);

        let reopened = MosaicAgent::attach(dir.path()).unwrap();
        assert_eq!(reopened.identity(), &idn);
    }

    #[test]
    fn attach_as_agent_requires_agent_identity() {
        let dir = TempDir::new().unwrap();
        let (idn, key) = human();
        MosaicAgent::init(dir.path(), idn, key).unwrap();

        let human2 = Identity::human("h@example.com", None).unwrap();
        let result = MosaicAgent::attach_as_agent(dir.path(), human2, SigningKey::generate());
        assert!(matches!(result, Err(Error::InvalidIdentity(_))));
    }

    #[test]
    fn attach_as_agent_with_real_agent_identity() {
        let dir = TempDir::new().unwrap();
        let (idn, key) = human();
        MosaicAgent::init(dir.path(), idn.clone(), key).unwrap();

        let agent_id = Identity::agent("agent-a", "sess-1", idn).unwrap();
        let agent =
            MosaicAgent::attach_as_agent(dir.path(), agent_id.clone(), SigningKey::generate())
                .unwrap();
        assert_eq!(agent.identity(), &agent_id);
    }
}
