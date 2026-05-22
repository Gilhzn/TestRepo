//! Stash: shelve work-in-progress without committing.
//!
//! `mos stash` captures the working-tree modifications (relative to the
//! branch tip), saves them as a named entry under `.mosaic/stash/`, and
//! resets the touched files back to the tip — giving you a clean tree to
//! switch tasks. `mos stash pop` writes a saved entry back into the
//! working tree.
//!
//! Stash entries are plain JSON (not signed — they're local scratch, never
//! synced). Each entry is content-addressed by a BLAKE3 of its payload so
//! ids are stable + collision-free.

use crate::error::{Error, Result};
use crate::hash::{Hash, Hasher};
use crate::m1::change::{FileChange, Tai64N};
use crate::repo::Repository;
use crate::working_copy::{FileState, StagedIndex, WorkingCopy};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StashEntry {
    pub id: String,
    pub ts: Tai64N,
    pub message: String,
    pub branch: String,
    pub files: Vec<FileChange>,
}

impl StashEntry {
    fn compute_id(message: &str, files: &[FileChange]) -> String {
        let mut h = Hasher::new();
        h.update(b"mosaic.stash.v1");
        h.update(message.as_bytes());
        for f in files {
            h.update(f.path.as_bytes());
            h.update(&f.patch);
        }
        // Mix in time so two identical stashes are still distinct.
        let now = Tai64N::now();
        h.update(&now.0.to_le_bytes());
        h.update(&now.1.to_le_bytes());
        h.finalize().to_hex()[..16].to_string()
    }
}

fn stash_dir(repo_parent: &Path) -> PathBuf {
    repo_parent.join(".mosaic").join("stash")
}

fn repo_parent(repo: &Repository) -> PathBuf {
    repo.root()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo.root().to_path_buf())
}

/// Capture working-tree modifications + untracked staged files relative to
/// `branch`, save them as a stash entry, and reset those files to the tip.
/// Returns the new entry's id (or None if there was nothing to stash).
pub fn push(repo: &Repository, branch: &str, message: &str) -> Result<Option<String>> {
    let parent = repo_parent(repo);
    let wc = WorkingCopy::open(repo, &parent);
    let index = StagedIndex::load(&parent)?;
    let status = wc.status(branch, &index)?;

    // Collect modified + untracked files' current content.
    let mut files = Vec::new();
    for entry in &status {
        if matches!(entry.state, FileState::Modified | FileState::Untracked) {
            let abs = parent.join(&entry.path);
            if let Ok(bytes) = fs::read(&abs) {
                let kind = if std::str::from_utf8(&bytes).is_ok() {
                    crate::m1::change::FileKind::Text
                } else {
                    crate::m1::change::FileKind::Binary
                };
                files.push(FileChange {
                    path: entry.path.clone(),
                    kind,
                    patch: bytes,
                    conflicts: Vec::new(),
                });
            }
        }
    }
    if files.is_empty() {
        return Ok(None);
    }

    let entry = StashEntry {
        id: StashEntry::compute_id(message, &files),
        ts: Tai64N::now(),
        message: message.to_string(),
        branch: branch.to_string(),
        files: files.clone(),
    };

    let dir = stash_dir(&parent);
    fs::create_dir_all(&dir)?;
    let raw =
        serde_json::to_string(&entry).map_err(|e| Error::Serialization(e.to_string()))?;
    fs::write(dir.join(format!("{}.json", entry.id)), raw)?;

    // Reset touched files to the branch tip (or delete if untracked there).
    let snapshot = wc.branch_snapshot(branch)?;
    for f in &files {
        let abs = parent.join(&f.path);
        match snapshot.get(&f.path) {
            Some(tip_bytes) => {
                let _ = fs::write(&abs, tip_bytes);
            }
            None => {
                // Untracked file — remove it from the working tree.
                let _ = fs::remove_file(&abs);
            }
        }
    }

    Ok(Some(entry.id))
}

pub fn list(repo: &Repository) -> Result<Vec<StashEntry>> {
    let parent = repo_parent(repo);
    let dir = stash_dir(&parent);
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for e in fs::read_dir(&dir)? {
        let e = e?;
        if e.path().extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(e.path())?;
        if let Ok(entry) = serde_json::from_str::<StashEntry>(&raw) {
            out.push(entry);
        }
    }
    out.sort_by_key(|e| (e.ts.0, e.ts.1));
    Ok(out)
}

pub fn get(repo: &Repository, id: &str) -> Result<StashEntry> {
    let parent = repo_parent(repo);
    let path = stash_dir(&parent).join(format!("{id}.json"));
    let raw = fs::read_to_string(&path)
        .map_err(|_| Error::Serialization(format!("no stash entry {id}")))?;
    serde_json::from_str(&raw).map_err(|e| Error::Serialization(e.to_string()))
}

/// Write a stash entry's files back into the working tree. Does NOT delete
/// the entry (use `drop` for that, or `pop` = apply + drop).
pub fn apply(repo: &Repository, id: &str) -> Result<usize> {
    let parent = repo_parent(repo);
    let entry = get(repo, id)?;
    for f in &entry.files {
        let abs = parent.join(&f.path);
        if let Some(p) = abs.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(&abs, &f.patch)?;
    }
    Ok(entry.files.len())
}

pub fn drop(repo: &Repository, id: &str) -> Result<()> {
    let parent = repo_parent(repo);
    let path = stash_dir(&parent).join(format!("{id}.json"));
    fs::remove_file(&path)
        .map_err(|_| Error::Serialization(format!("no stash entry {id}")))?;
    Ok(())
}

/// apply + drop.
pub fn pop(repo: &Repository, id: &str) -> Result<usize> {
    let n = apply(repo, id)?;
    drop(repo, id)?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m1::change::{ChangeBuilder, FileKind};
    use crate::m1::identity::Identity;
    use crate::m1::signing::SigningKey;
    use tempfile::TempDir;

    fn seed(dir: &Path) {
        let mut repo = Repository::init(dir).unwrap();
        let idn = Identity::human("dev@example.com", None).unwrap();
        let key = SigningKey::generate();
        std::fs::write(dir.join("tracked.txt"), b"original\n").unwrap();
        let id = repo
            .commit(
                ChangeBuilder::new(idn, key)
                    .intent("seed")
                    .file(FileChange {
                        path: "tracked.txt".into(),
                        kind: FileKind::Text,
                        patch: b"original\n".to_vec(),
                        conflicts: Vec::new(),
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
        repo.advance_branch("main", id).unwrap();
    }

    #[test]
    fn push_shelves_and_resets_modified_file() {
        let dir = TempDir::new().unwrap();
        seed(dir.path());
        std::fs::write(dir.path().join("tracked.txt"), b"WIP changes\n").unwrap();

        let repo = Repository::open(dir.path()).unwrap();
        let id = push(&repo, "main", "wip").unwrap().unwrap();

        // Working tree reset to tip.
        let on_disk = std::fs::read(dir.path().join("tracked.txt")).unwrap();
        assert_eq!(on_disk, b"original\n");

        // Entry exists.
        assert_eq!(list(&repo).unwrap().len(), 1);

        // Pop restores the WIP.
        pop(&repo, &id).unwrap();
        let restored = std::fs::read(dir.path().join("tracked.txt")).unwrap();
        assert_eq!(restored, b"WIP changes\n");
        assert!(list(&repo).unwrap().is_empty());
    }

    #[test]
    fn push_shelves_untracked_and_removes_it() {
        let dir = TempDir::new().unwrap();
        seed(dir.path());
        std::fs::write(dir.path().join("scratch.txt"), b"new file\n").unwrap();

        let repo = Repository::open(dir.path()).unwrap();
        let id = push(&repo, "main", "untracked").unwrap().unwrap();
        // Untracked file removed from working tree on stash.
        assert!(!dir.path().join("scratch.txt").exists());

        apply(&repo, &id).unwrap();
        assert!(dir.path().join("scratch.txt").exists());
    }

    #[test]
    fn push_with_nothing_to_stash_returns_none() {
        let dir = TempDir::new().unwrap();
        seed(dir.path());
        let repo = Repository::open(dir.path()).unwrap();
        assert!(push(&repo, "main", "empty").unwrap().is_none());
    }

    #[test]
    fn apply_keeps_entry_pop_removes_it() {
        let dir = TempDir::new().unwrap();
        seed(dir.path());
        std::fs::write(dir.path().join("tracked.txt"), b"x\n").unwrap();
        let repo = Repository::open(dir.path()).unwrap();
        let id = push(&repo, "main", "m").unwrap().unwrap();

        apply(&repo, &id).unwrap();
        assert_eq!(list(&repo).unwrap().len(), 1); // still there
        drop(&repo, &id).unwrap();
        assert!(list(&repo).unwrap().is_empty());
    }

    #[test]
    fn drop_unknown_errors() {
        let dir = TempDir::new().unwrap();
        seed(dir.path());
        let repo = Repository::open(dir.path()).unwrap();
        assert!(drop(&repo, "nope").is_err());
    }
}
