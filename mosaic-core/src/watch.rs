//! Subscriptions: "tell me when something lands on branch X".
//!
//! A lightweight, file-backed subscription registry the server consults
//! after every accepted push. Where webhooks fan out to *external* HTTP
//! endpoints, watches are *internal* records that a polling client (CLI,
//! IDE, agent) can query to learn what changed since it last looked —
//! without holding a long-lived connection.
//!
//! Each watch records a branch + the frontier the subscriber last saw.
//! `since()` returns the changes that landed after that frontier so a
//! client can do `mos watch poll <branch>` and get just the delta.

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::m1::change::ChangeId;
use crate::m1_dag::refs::Frontier;
use crate::repo::Repository;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Watches {
    /// branch name → the frontier the subscriber last acknowledged.
    pub seen: std::collections::BTreeMap<String, Vec<String>>,
}

impl Watches {
    fn path(repo_root: &Path) -> PathBuf {
        repo_root.join(".mosaic").join("watches.json")
    }

    pub fn load(repo_root: impl AsRef<Path>) -> Result<Self> {
        let path = Self::path(repo_root.as_ref());
        match fs::read_to_string(&path) {
            Ok(s) => {
                serde_json::from_str(&s).map_err(|e| Error::Serialization(e.to_string()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    pub fn save(&self, repo_root: impl AsRef<Path>) -> Result<()> {
        let dir = repo_root.as_ref().join(".mosaic");
        fs::create_dir_all(&dir)?;
        let raw = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        fs::write(Self::path(repo_root.as_ref()), raw)?;
        Ok(())
    }

    pub fn watched_branches(&self) -> Vec<String> {
        self.seen.keys().cloned().collect()
    }

    pub fn frontier_for(&self, branch: &str) -> Frontier {
        let mut f = Frontier::default();
        if let Some(hexes) = self.seen.get(branch) {
            for h in hexes {
                if let Ok(hash) = Hash::from_hex(h) {
                    f.0.insert(hash);
                }
            }
        }
        f
    }

    pub fn set_seen(&mut self, branch: &str, frontier: &Frontier) {
        self.seen.insert(
            branch.to_string(),
            frontier.0.iter().map(|h| h.to_hex()).collect(),
        );
    }
}

/// Start watching a branch from its current tip (so future polls show only
/// changes that land after now).
pub fn watch(repo: &Repository, branch: &str) -> Result<()> {
    let mut w = Watches::load(repo.root_parent())?;
    let current = repo.refs().get(branch).unwrap_or_default();
    w.set_seen(branch, &current);
    w.save(repo.root_parent())?;
    Ok(())
}

pub fn unwatch(repo: &Repository, branch: &str) -> Result<bool> {
    let mut w = Watches::load(repo.root_parent())?;
    let removed = w.seen.remove(branch).is_some();
    w.save(repo.root_parent())?;
    Ok(removed)
}

/// Changes that landed on `branch` since the subscriber last polled, in
/// topological order. Updates the seen-frontier to the current tip.
pub fn poll(repo: &Repository, branch: &str) -> Result<Vec<ChangeId>> {
    let mut w = Watches::load(repo.root_parent())?;
    let seen = w.frontier_for(branch);
    let current = repo.refs().get(branch).unwrap_or_default();

    let delta = crate::sync::missing_changes_for(repo, &seen, &current);
    w.set_seen(branch, &current);
    w.save(repo.root_parent())?;
    Ok(delta)
}

/// Like `poll` but does NOT advance the seen-frontier (a peek).
pub fn peek(repo: &Repository, branch: &str) -> Result<Vec<ChangeId>> {
    let w = Watches::load(repo.root_parent())?;
    let seen = w.frontier_for(branch);
    let current = repo.refs().get(branch).unwrap_or_default();
    Ok(crate::sync::missing_changes_for(repo, &seen, &current))
}

// Helper trait extension: the Repository exposes `root()` which is the
// `.mosaic` dir; watches live alongside it under the same parent.
trait RepoRootParent {
    fn root_parent(&self) -> PathBuf;
}

impl RepoRootParent for Repository {
    fn root_parent(&self) -> PathBuf {
        // `root()` is `<parent>/.mosaic`; we want `<parent>`.
        self.root()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.root().to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m1::change::{ChangeBuilder, FileChange, FileKind};
    use crate::m1::identity::Identity;
    use crate::m1::signing::SigningKey;
    use tempfile::TempDir;

    fn human() -> (Identity, SigningKey) {
        (
            Identity::human("dev@example.com", None).unwrap(),
            SigningKey::generate(),
        )
    }

    fn commit(repo: &mut Repository, intent: &str, parent: Option<ChangeId>) -> ChangeId {
        let (id, key) = human();
        let mut b = ChangeBuilder::new(id, key).intent(intent);
        if let Some(p) = parent {
            b = b.dep(p);
        }
        b = b.file(FileChange {
            path: format!("{intent}.txt"),
            kind: FileKind::Text,
            patch: intent.as_bytes().to_vec(),
            conflicts: Vec::new(),
        });
        let cid = repo.commit(b.build().unwrap()).unwrap();
        repo.advance_branch("main", cid).unwrap();
        cid
    }

    #[test]
    fn poll_returns_only_new_changes_since_watch() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let c1 = commit(&mut repo, "first", None);

        // Start watching at c1.
        watch(&repo, "main").unwrap();

        // No new changes yet.
        assert!(poll(&repo, "main").unwrap().is_empty());

        // Two more land.
        let c2 = commit(&mut repo, "second", Some(c1));
        let c3 = commit(&mut repo, "third", Some(c2));

        let delta = poll(&repo, "main").unwrap();
        let ids: BTreeSet<Hash> = delta.iter().map(|c| c.0).collect();
        assert!(ids.contains(&c2.0));
        assert!(ids.contains(&c3.0));
        assert!(!ids.contains(&c1.0));

        // After poll, frontier advanced — next poll is empty.
        assert!(poll(&repo, "main").unwrap().is_empty());
    }

    #[test]
    fn peek_does_not_advance_frontier() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let c1 = commit(&mut repo, "first", None);
        watch(&repo, "main").unwrap();
        let _c2 = commit(&mut repo, "second", Some(c1));

        let peek1 = peek(&repo, "main").unwrap();
        let peek2 = peek(&repo, "main").unwrap();
        assert_eq!(peek1.len(), 1);
        assert_eq!(peek2.len(), 1); // unchanged — peek doesn't advance
    }

    #[test]
    fn watch_from_empty_then_first_commit_shows_up() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        // Watch a branch before any commits.
        watch(&repo, "main").unwrap();
        let _c1 = commit(&mut repo, "first", None);
        let delta = poll(&repo, "main").unwrap();
        assert_eq!(delta.len(), 1);
    }

    #[test]
    fn unwatch_removes_subscription() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        watch(&repo, "main").unwrap();
        let w = Watches::load(dir.path()).unwrap();
        assert!(w.watched_branches().contains(&"main".to_string()));
        assert!(unwatch(&repo, "main").unwrap());
        let w2 = Watches::load(dir.path()).unwrap();
        assert!(w2.watched_branches().is_empty());
    }

    #[test]
    fn watches_round_trip_on_disk() {
        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let mut w = Watches::default();
        let mut f = Frontier::default();
        f.0.insert(Hash::of(b"x"));
        w.set_seen("main", &f);
        w.save(dir.path()).unwrap();
        let back = Watches::load(dir.path()).unwrap();
        assert_eq!(back.frontier_for("main").0.len(), 1);
    }
}
