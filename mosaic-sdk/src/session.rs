use mosaic_core::error::{Error, Result};
use mosaic_core::m1::change::{ChangeBuilder, ChangeId, FileChange, FileKind};
use mosaic_core::m1::identity::Identity;
use mosaic_core::m1::signing::SigningKey;
use mosaic_core::repo::Repository;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A unit of work an agent is performing. Collects file-level edits in
/// memory; on `commit()` writes them to the repository as one signed
/// `Change` whose parents are the branch's tips at commit time.
pub struct Session {
    root: PathBuf,
    identity: Identity,
    signing_key: SigningKey,
    branch: String,
    intent: String,
    files: BTreeMap<String, FileChange>,
    aborted: bool,
}

impl Session {
    pub(crate) fn open(
        root: PathBuf,
        identity: Identity,
        signing_key: SigningKey,
        branch: String,
        intent: String,
    ) -> Result<Self> {
        if intent.trim().is_empty() {
            return Err(Error::InvalidIdentity("session intent must not be empty"));
        }
        Ok(Self {
            root,
            identity,
            signing_key,
            branch,
            intent,
            files: BTreeMap::new(),
            aborted: false,
        })
    }

    pub fn intent(&self) -> &str {
        &self.intent
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Stage a text file edit. `path` is the in-repo path; `new_bytes` is
    /// the final content for this session. If multiple edits to the same
    /// path occur, the last one wins.
    pub fn stage_text(&mut self, path: impl Into<String>, new_bytes: Vec<u8>) {
        let path = path.into();
        self.files.insert(
            path.clone(),
            FileChange {
                path,
                kind: FileKind::Text,
                patch: new_bytes,
                conflicts: Vec::new(),
            },
        );
    }

    pub fn stage_binary(&mut self, path: impl Into<String>, new_bytes: Vec<u8>) {
        let path = path.into();
        self.files.insert(
            path.clone(),
            FileChange {
                path,
                kind: FileKind::Binary,
                patch: new_bytes,
                conflicts: Vec::new(),
            },
        );
    }

    /// Read+transform+write helper. Reads the current file content from the
    /// working copy, applies `f`, and stages the result. Returns the new
    /// content for inspection.
    pub fn edit_text<F: FnOnce(&str) -> String>(
        &mut self,
        path: impl Into<String>,
        f: F,
    ) -> Result<String> {
        let path = path.into();
        let abs = self.root.join(&path);
        let prior = std::fs::read_to_string(&abs).unwrap_or_default();
        let after = f(&prior);
        // Mirror the change into the working copy so subsequent reads see it.
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        std::fs::write(&abs, &after).map_err(Error::Io)?;
        self.stage_text(path, after.clone().into_bytes());
        Ok(after)
    }

    pub fn files_staged(&self) -> usize {
        self.files.len()
    }

    /// Discard everything staged. Subsequent calls fail.
    pub fn abort(mut self) {
        self.aborted = true;
        self.files.clear();
    }

    /// Finalize the session. Atomically writes a signed Change and advances
    /// the target branch. Returns the new change id.
    pub fn commit(self) -> Result<ChangeId> {
        if self.aborted {
            return Err(Error::Serialization("session was aborted".into()));
        }
        if self.files.is_empty() {
            return Err(Error::Serialization(
                "session has no staged files".into(),
            ));
        }

        let mut repo = Repository::open(&self.root)?;
        let tips = repo.refs().get(&self.branch).unwrap_or_default();

        let mut builder =
            ChangeBuilder::new(self.identity.clone(), self.signing_key.clone()).intent(self.intent);
        for tip in &tips.0 {
            builder = builder.dep(ChangeId(*tip));
        }
        for fc in self.files.into_values() {
            builder = builder.file(fc);
        }
        let change = builder.build()?;
        let id = repo.commit(change)?;
        repo.advance_branch(&self.branch, id)?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MosaicAgent;
    use tempfile::TempDir;

    fn fresh_agent() -> (TempDir, MosaicAgent) {
        let dir = TempDir::new().unwrap();
        let idn = Identity::human("dev@example.com", Some("Dev".into())).unwrap();
        let key = SigningKey::generate();
        let agent = MosaicAgent::init(dir.path(), idn, key).unwrap();
        (dir, agent)
    }

    #[test]
    fn session_round_trip() {
        let (_dir, agent) = fresh_agent();
        let mut s = agent.begin_session("add greeting").unwrap();
        s.stage_text("hello.txt", b"hello\n".to_vec());
        let id = s.commit().unwrap();

        let history = agent.history_on("main").unwrap();
        assert_eq!(history, vec![id]);
    }

    #[test]
    fn session_with_empty_intent_rejected() {
        let (_dir, agent) = fresh_agent();
        let r = agent.begin_session("");
        assert!(r.is_err());
    }

    #[test]
    fn session_with_no_staged_files_rejected() {
        let (_dir, agent) = fresh_agent();
        let s = agent.begin_session("nothing here").unwrap();
        assert!(s.commit().is_err());
    }

    #[test]
    fn edit_text_reads_writes_through_working_copy() {
        let (dir, agent) = fresh_agent();
        std::fs::write(dir.path().join("notes.md"), "first line\n").unwrap();

        let mut s = agent.begin_session("amend notes").unwrap();
        let after = s
            .edit_text("notes.md", |prior| format!("{prior}second line\n"))
            .unwrap();
        assert!(after.contains("first line"));
        assert!(after.contains("second line"));
        s.commit().unwrap();

        let on_disk = std::fs::read_to_string(dir.path().join("notes.md")).unwrap();
        assert_eq!(on_disk, after);
    }

    #[test]
    fn two_sequential_sessions_form_a_chain() {
        let (_dir, agent) = fresh_agent();
        let mut s1 = agent.begin_session("first").unwrap();
        s1.stage_text("a", b"1".to_vec());
        let id1 = s1.commit().unwrap();

        let mut s2 = agent.begin_session("second").unwrap();
        s2.stage_text("b", b"2".to_vec());
        let id2 = s2.commit().unwrap();

        assert_ne!(id1, id2);
        let history = agent.history_on("main").unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0], id1);
        assert_eq!(history[1], id2);
    }

    #[test]
    fn abort_does_not_persist_anything() {
        let (_dir, agent) = fresh_agent();
        let mut s = agent.begin_session("never mind").unwrap();
        s.stage_text("x", b"x".to_vec());
        s.abort();

        assert!(agent.history_on("main").unwrap().is_empty());
    }
}
