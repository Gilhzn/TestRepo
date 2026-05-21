//! Working-copy ergonomics for the `mos` CLI.
//!
//! Solves the "every developer's muscle memory" problem. With this module:
//!
//!   mos add .                    # stage every modified file under cwd
//!   mos add src/foo.rs           # stage one
//!   mos status                   # show working tree vs branch tip
//!   mos diff                     # show line-level diff for staged + modified
//!   mos commit -i "intent"       # commits all staged files (no -f needed)
//!   mos unstage src/foo.rs
//!   mos restore src/foo.rs       # discard working-tree changes
//!
//! Tracked state lives at `.mosaic/index.json`: a sorted list of relative
//! paths that have been explicitly staged. Files NOT in the index but
//! that differ from the branch tip are "unstaged modifications" and show
//! up in status but are not committed unless added.

use crate::error::{Error, Result};
use crate::m1::change::{ChangeId, FileChange, FileKind};
use crate::repo::{Repository, REPO_DIR};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const INDEX_FILE: &str = "index.json";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StagedIndex {
    pub paths: BTreeSet<String>,
}

impl StagedIndex {
    pub fn load(repo_root: &Path) -> Result<Self> {
        let path = repo_root.join(REPO_DIR).join(INDEX_FILE);
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s)
                .map_err(|e| Error::Serialization(format!("index.json: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    pub fn save(&self, repo_root: &Path) -> Result<()> {
        let dir = repo_root.join(REPO_DIR);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(INDEX_FILE);
        let raw = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        std::fs::write(path, raw)?;
        Ok(())
    }

    pub fn stage(&mut self, path: String) {
        self.paths.insert(path);
    }

    pub fn unstage(&mut self, path: &str) -> bool {
        self.paths.remove(path)
    }

    pub fn clear(&mut self) {
        self.paths.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    Untracked,
    Modified,
    Unmodified,
    Removed,
}

#[derive(Debug, Clone)]
pub struct StatusEntry {
    pub path: String,
    pub state: FileState,
    pub staged: bool,
}

pub struct WorkingCopy<'a> {
    repo: &'a Repository,
    root: PathBuf,
}

impl<'a> WorkingCopy<'a> {
    pub fn open(repo: &'a Repository, root: impl Into<PathBuf>) -> Self {
        Self {
            repo,
            root: root.into(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Build a map of path -> content from the latest reachable change set
    /// of `branch` — the "branch tip" against which we diff.
    pub fn branch_snapshot(&self, branch: &str) -> Result<BTreeMap<String, Vec<u8>>> {
        let frontier = match self.repo.refs().get(branch) {
            Ok(f) => f,
            Err(Error::RefNotFound(_)) => return Ok(BTreeMap::new()),
            Err(e) => return Err(e),
        };

        let mut ancestors: BTreeSet<crate::Hash> = BTreeSet::new();
        let mut queue: std::collections::VecDeque<crate::Hash> =
            frontier.0.iter().copied().collect();
        while let Some(h) = queue.pop_front() {
            if !ancestors.insert(h) {
                continue;
            }
            if let Some(ps) = self.repo.index().parents_of(&h) {
                for p in ps {
                    queue.push_back(*p);
                }
            }
        }

        let ordered = self.repo.topo_order(&ancestors);
        let mut snapshot: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for h in ordered {
            let change = self.repo.load_change(&ChangeId(h))?;
            for file in change.body {
                snapshot.insert(file.path, file.patch);
            }
        }
        Ok(snapshot)
    }

    /// Walk every file under `root` (skipping `.mosaic/` and a small set of
    /// well-known artifact dirs) and return paths relative to root.
    pub fn working_tree_files(&self) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        let root = self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
        walk(&root, &root, &mut out)?;
        out.sort();
        Ok(out)
    }

    /// Compute one StatusEntry per path that's either in the snapshot, in
    /// the working tree, or in the staged index.
    pub fn status(&self, branch: &str, index: &StagedIndex) -> Result<Vec<StatusEntry>> {
        let snapshot = self.branch_snapshot(branch)?;
        let working = self.working_tree_files()?;

        let mut all_paths: BTreeSet<String> = BTreeSet::new();
        for p in snapshot.keys() {
            all_paths.insert(p.clone());
        }
        for p in &working {
            all_paths.insert(p.clone());
        }
        for p in &index.paths {
            all_paths.insert(p.clone());
        }

        let mut entries = Vec::new();
        for path in all_paths {
            let in_snapshot = snapshot.contains_key(&path);
            let in_wd = working.contains(&path);
            let state = if !in_wd && in_snapshot {
                FileState::Removed
            } else if in_wd && !in_snapshot {
                FileState::Untracked
            } else if in_wd && in_snapshot {
                let wd_bytes = std::fs::read(self.root.join(&path)).unwrap_or_default();
                if wd_bytes == snapshot[&path] {
                    FileState::Unmodified
                } else {
                    FileState::Modified
                }
            } else {
                FileState::Unmodified
            };
            let staged = index.paths.contains(&path);
            entries.push(StatusEntry {
                path,
                state,
                staged,
            });
        }
        // Filter out boring entries (Unmodified, not staged).
        entries.retain(|e| !(matches!(e.state, FileState::Unmodified) && !e.staged));
        Ok(entries)
    }

    /// Stage every path that's currently modified OR untracked relative to
    /// the branch tip. Mirrors `git add .`.
    pub fn stage_all_modified(
        &self,
        branch: &str,
        index: &mut StagedIndex,
    ) -> Result<usize> {
        let entries = self.status(branch, index)?;
        let mut added = 0;
        for e in entries {
            if matches!(e.state, FileState::Modified | FileState::Untracked)
                && !e.staged
            {
                index.stage(e.path);
                added += 1;
            }
        }
        Ok(added)
    }

    /// Build FileChanges for every staged path, ready to attach to a Change.
    pub fn build_staged_file_changes(
        &self,
        index: &StagedIndex,
    ) -> Result<Vec<FileChange>> {
        let mut out = Vec::new();
        for path in &index.paths {
            let abs = self.root.join(path);
            let bytes = std::fs::read(&abs).map_err(Error::Io)?;
            let kind = if std::str::from_utf8(&bytes).is_ok() {
                FileKind::Text
            } else {
                FileKind::Binary
            };
            out.push(FileChange {
                path: path.clone(),
                kind,
                patch: bytes,
                conflicts: Vec::new(),
            });
        }
        Ok(out)
    }

    /// Restore a file to its branch-tip state. Returns true if the file
    /// was rewritten, false if there was nothing to do.
    pub fn restore(&self, branch: &str, path: &str) -> Result<bool> {
        let snapshot = self.branch_snapshot(branch)?;
        let bytes = match snapshot.get(path) {
            Some(b) => b,
            None => return Ok(false),
        };
        let abs = self.root.join(path);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(abs, bytes)?;
        Ok(true)
    }

    /// Unified line-diff text for a single path between branch tip and
    /// working tree. Empty string when no difference.
    pub fn diff_text(&self, branch: &str, path: &str) -> Result<String> {
        let snapshot = self.branch_snapshot(branch)?;
        let before = snapshot
            .get(path)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        let after_bytes = std::fs::read(self.root.join(path)).unwrap_or_default();
        let after = String::from_utf8_lossy(&after_bytes).into_owned();
        if before == after {
            return Ok(String::new());
        }
        use similar::{ChangeTag, TextDiff};
        let diff = TextDiff::from_lines(&before, &after);
        let mut out = String::new();
        for change in diff.iter_all_changes() {
            let sign = match change.tag() {
                ChangeTag::Equal => " ",
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
            };
            out.push_str(sign);
            out.push_str(change.value().trim_end_matches('\n'));
            out.push('\n');
        }
        Ok(out)
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if matches!(
            name_str.as_ref(),
            REPO_DIR | "target" | "node_modules" | "dist" | ".git" | "__pycache__"
        ) {
            continue;
        }
        if entry.file_type().map_err(Error::Io)?.is_dir() {
            walk(root, &path, out)?;
        } else {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m1::change::ChangeBuilder;
    use crate::m1::identity::Identity;
    use crate::m1::signing::SigningKey;
    use tempfile::TempDir;

    fn human() -> (Identity, SigningKey) {
        (
            Identity::human("dev@example.com", Some("Dev".into())).unwrap(),
            SigningKey::generate(),
        )
    }

    fn seed_repo(dir: &Path, files: &[(&str, &[u8])]) {
        let mut repo = Repository::init(dir).unwrap();
        let (idn, key) = human();
        let mut builder = ChangeBuilder::new(idn, key).intent("seed");
        for (path, bytes) in files {
            if let Some(parent) = std::path::Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir.join(parent)).unwrap();
                }
            }
            std::fs::write(dir.join(path), bytes).unwrap();
            builder = builder.file(FileChange {
                path: (*path).to_string(),
                kind: FileKind::Text,
                patch: bytes.to_vec(),
                conflicts: Vec::new(),
            });
        }
        let id = repo.commit(builder.build().unwrap()).unwrap();
        repo.advance_branch("main", id).unwrap();
    }

    #[test]
    fn index_round_trip_on_disk() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(REPO_DIR)).unwrap();
        let mut idx = StagedIndex::default();
        idx.stage("a.txt".into());
        idx.stage("src/b.rs".into());
        idx.save(dir.path()).unwrap();

        let reloaded = StagedIndex::load(dir.path()).unwrap();
        assert_eq!(reloaded.paths.len(), 2);
        assert!(reloaded.paths.contains("a.txt"));
        assert!(reloaded.paths.contains("src/b.rs"));
    }

    #[test]
    fn missing_index_loads_empty() {
        let dir = TempDir::new().unwrap();
        let idx = StagedIndex::load(dir.path()).unwrap();
        assert!(idx.paths.is_empty());
    }

    #[test]
    fn status_marks_untracked_modified_and_unmodified() {
        let dir = TempDir::new().unwrap();
        seed_repo(dir.path(), &[("kept.txt", b"original"), ("touched.txt", b"v1")]);

        // After seed: both files exist, both match snapshot.
        std::fs::write(dir.path().join("touched.txt"), b"v2-modified").unwrap();
        std::fs::write(dir.path().join("new.txt"), b"hello").unwrap();

        let repo = Repository::open(dir.path()).unwrap();
        let wc = WorkingCopy::open(&repo, dir.path());
        let index = StagedIndex::default();
        let status = wc.status("main", &index).unwrap();

        let by_path: std::collections::HashMap<&str, &StatusEntry> =
            status.iter().map(|e| (e.path.as_str(), e)).collect();
        assert!(matches!(
            by_path.get("touched.txt").unwrap().state,
            FileState::Modified
        ));
        assert!(matches!(
            by_path.get("new.txt").unwrap().state,
            FileState::Untracked
        ));
        // kept.txt is unmodified → filtered out.
        assert!(!by_path.contains_key("kept.txt"));
    }

    #[test]
    fn stage_all_modified_picks_up_changes_and_untracked() {
        let dir = TempDir::new().unwrap();
        seed_repo(dir.path(), &[("a.txt", b"a")]);
        std::fs::write(dir.path().join("a.txt"), b"a-modified").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"b-new").unwrap();

        let repo = Repository::open(dir.path()).unwrap();
        let wc = WorkingCopy::open(&repo, dir.path());
        let mut index = StagedIndex::default();
        let added = wc.stage_all_modified("main", &mut index).unwrap();
        assert_eq!(added, 2);
        assert!(index.paths.contains("a.txt"));
        assert!(index.paths.contains("b.txt"));
    }

    #[test]
    fn build_staged_file_changes_loads_bytes() {
        let dir = TempDir::new().unwrap();
        seed_repo(dir.path(), &[]);
        std::fs::write(dir.path().join("greet.txt"), b"hello world").unwrap();

        let repo = Repository::open(dir.path()).unwrap();
        let wc = WorkingCopy::open(&repo, dir.path());
        let mut index = StagedIndex::default();
        index.stage("greet.txt".into());

        let files = wc.build_staged_file_changes(&index).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "greet.txt");
        assert_eq!(files[0].patch, b"hello world");
        assert!(matches!(files[0].kind, FileKind::Text));
    }

    #[test]
    fn restore_resets_a_modified_file() {
        let dir = TempDir::new().unwrap();
        seed_repo(dir.path(), &[("file.txt", b"original")]);
        std::fs::write(dir.path().join("file.txt"), b"tampered").unwrap();

        let repo = Repository::open(dir.path()).unwrap();
        let wc = WorkingCopy::open(&repo, dir.path());
        let ok = wc.restore("main", "file.txt").unwrap();
        assert!(ok);
        let back = std::fs::read(dir.path().join("file.txt")).unwrap();
        assert_eq!(back, b"original");
    }

    #[test]
    fn diff_text_shows_unified_diff() {
        let dir = TempDir::new().unwrap();
        seed_repo(
            dir.path(),
            &[("notes.md", b"alpha\nbeta\ngamma\n")],
        );
        std::fs::write(dir.path().join("notes.md"), b"alpha\nBETA\ngamma\ndelta\n").unwrap();

        let repo = Repository::open(dir.path()).unwrap();
        let wc = WorkingCopy::open(&repo, dir.path());
        let diff = wc.diff_text("main", "notes.md").unwrap();
        assert!(diff.contains("-beta"));
        assert!(diff.contains("+BETA"));
        assert!(diff.contains("+delta"));
    }

    #[test]
    fn skips_meta_directories() {
        let dir = TempDir::new().unwrap();
        seed_repo(dir.path(), &[]);
        std::fs::create_dir_all(dir.path().join("target/some-build")).unwrap();
        std::fs::write(dir.path().join("target/some-build/junk"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        std::fs::write(dir.path().join("node_modules/pkg/x"), b"x").unwrap();
        std::fs::write(dir.path().join("real.txt"), b"y").unwrap();

        let repo = Repository::open(dir.path()).unwrap();
        let wc = WorkingCopy::open(&repo, dir.path());
        let files = wc.working_tree_files().unwrap();
        assert!(files.contains(&"real.txt".to_string()));
        assert!(!files.iter().any(|p| p.starts_with("target/")));
        assert!(!files.iter().any(|p| p.starts_with("node_modules/")));
    }
}
