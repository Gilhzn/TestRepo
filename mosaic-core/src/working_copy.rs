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

/// A file whose content diverged across frontier tips and was auto-merged
/// during snapshot/checkout (instead of one tip silently overwriting another).
#[derive(Debug, Clone)]
pub struct MergeNote {
    pub path: String,
    /// Line-level structured conflicts carried by the union merge (0 = clean).
    pub conflicts: usize,
    /// True when the file couldn't be text-merged (binary / non-UTF-8) and one
    /// tip's bytes were kept verbatim.
    pub binary: bool,
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

    /// Build a map of path -> content for `branch` — the "branch tip" against
    /// which we diff and that `checkout` materializes.
    ///
    /// When the frontier has a single tip this is a plain linear
    /// materialization. When it has **multiple tips** (parallel work that
    /// hasn't been linearized), files that diverge across tips are 3-way merged
    /// rather than last-writer-wins, so no tip's work is silently dropped.
    pub fn branch_snapshot(&self, branch: &str) -> Result<BTreeMap<String, Vec<u8>>> {
        Ok(self.branch_snapshot_merged(branch)?.0)
    }

    /// Like [`branch_snapshot`](Self::branch_snapshot), but also returns a note
    /// for every path that diverged across frontier tips and had to be merged.
    pub fn branch_snapshot_merged(
        &self,
        branch: &str,
    ) -> Result<(BTreeMap<String, Vec<u8>>, Vec<MergeNote>)> {
        let frontier = match self.repo.refs().get(branch) {
            Ok(f) => f,
            Err(Error::RefNotFound(_)) => return Ok((BTreeMap::new(), Vec::new())),
            Err(e) => return Err(e),
        };
        let tips: Vec<crate::Hash> = frontier.0.iter().copied().collect();

        // Single tip (or empty): linear materialization, no divergence possible.
        if tips.len() <= 1 {
            return Ok((self.snapshot_over(&self.ancestor_set(&tips))?, Vec::new()));
        }

        // Multi-tip: materialize each tip independently, then merge per path so
        // concurrent same-file edits combine instead of overwriting each other.
        let tip_ancestors: Vec<BTreeSet<crate::Hash>> = tips
            .iter()
            .map(|t| self.ancestor_set(std::slice::from_ref(t)))
            .collect();
        let tip_snaps: Vec<BTreeMap<String, Vec<u8>>> = tip_ancestors
            .iter()
            .map(|a| self.snapshot_over(a))
            .collect::<Result<_>>()?;

        // Merge base = changes common to every tip.
        let mut base_set = tip_ancestors[0].clone();
        for a in &tip_ancestors[1..] {
            base_set.retain(|h| a.contains(h));
        }
        let base_snap = self.snapshot_over(&base_set)?;

        let mut all_paths: BTreeSet<String> = BTreeSet::new();
        for s in &tip_snaps {
            all_paths.extend(s.keys().cloned());
        }

        let mut out: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut notes: Vec<MergeNote> = Vec::new();
        for path in all_paths {
            let versions: Vec<&Vec<u8>> = tip_snaps.iter().filter_map(|s| s.get(&path)).collect();
            if versions.is_empty() {
                continue;
            }
            if versions.iter().all(|v| *v == versions[0]) {
                out.insert(path, versions[0].clone());
                continue;
            }
            let base = base_snap.get(&path).cloned().unwrap_or_default();
            let (content, note) = self.merge_versions(&path, &base, &versions);
            out.insert(path, content);
            notes.push(note);
        }
        Ok((out, notes))
    }

    /// All changes reachable from `tips` (inclusive).
    fn ancestor_set(&self, tips: &[crate::Hash]) -> BTreeSet<crate::Hash> {
        let mut ancestors: BTreeSet<crate::Hash> = BTreeSet::new();
        let mut queue: std::collections::VecDeque<crate::Hash> = tips.iter().copied().collect();
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
        ancestors
    }

    /// Materialize a set of changes (last-writer-wins over topo order). Correct
    /// for a single linear lineage; multi-tip divergence is handled by the
    /// caller via [`merge_versions`](Self::merge_versions).
    fn snapshot_over(&self, set: &BTreeSet<crate::Hash>) -> Result<BTreeMap<String, Vec<u8>>> {
        let ordered = self.repo.topo_order(set);
        let mut snapshot: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for h in ordered {
            let change = self.repo.load_change(&ChangeId(h))?;
            for file in change.body {
                snapshot.insert(file.path, file.patch);
            }
        }
        Ok(snapshot)
    }

    /// 3-way merge the divergent `versions` of one path against `base`, folding
    /// pairwise. Text files are unioned through the patch+semantic engine (both
    /// sides kept; conflicts counted but never dropped). Binary / non-UTF-8
    /// files can't be text-merged, so the first tip's bytes are kept and the
    /// note is flagged binary for the caller to surface.
    fn merge_versions(
        &self,
        path: &str,
        base: &[u8],
        versions: &[&Vec<u8>],
    ) -> (Vec<u8>, MergeNote) {
        let utf8_ok = std::str::from_utf8(base).is_ok()
            && versions.iter().all(|v| std::str::from_utf8(v).is_ok());
        if !utf8_ok {
            return (
                versions[0].to_vec(),
                MergeNote {
                    path: path.to_string(),
                    conflicts: 0,
                    binary: true,
                },
            );
        }
        let lang = crate::ast::Lang::from_path(path);
        let creator = crate::Hash::of(format!("checkout-merge:{path}").as_bytes());
        let base_s = String::from_utf8_lossy(base).into_owned();
        let mut acc = String::from_utf8_lossy(versions[0]).into_owned();
        let mut conflicts = 0usize;
        for v in &versions[1..] {
            let theirs = String::from_utf8_lossy(v).into_owned();
            match crate::merge_strategies::merge_text_file(&creator, lang, &base_s, &acc, &theirs) {
                Ok(fm) => {
                    conflicts += fm.patch_conflicts.len();
                    acc = fm.merged_lines.join("\n");
                    if !acc.is_empty() {
                        acc.push('\n');
                    }
                }
                Err(_) => conflicts += 1,
            }
        }
        (
            acc.into_bytes(),
            MergeNote {
                path: path.to_string(),
                conflicts,
                binary: false,
            },
        )
    }

    /// Materialize a branch's files into the working tree, honoring an
    /// optional sparse profile. Returns `(written paths, merge notes)`. When
    /// the branch frontier has multiple tips, divergent files are auto-merged
    /// (see [`branch_snapshot_merged`](Self::branch_snapshot_merged)) and the
    /// notes report which paths were merged and whether they carry conflicts.
    /// Files excluded by the sparse profile are skipped (not written to disk).
    pub fn checkout(
        &self,
        branch: &str,
        sparse: &crate::sparse::SparseProfile,
    ) -> Result<(Vec<String>, Vec<MergeNote>)> {
        let (snapshot, notes) = self.branch_snapshot_merged(branch)?;
        let mut written = Vec::new();
        for (path, bytes) in &snapshot {
            if !sparse.includes(path) {
                continue;
            }
            let abs = self.root.join(path);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).map_err(Error::Io)?;
            }
            std::fs::write(&abs, bytes).map_err(Error::Io)?;
            written.push(path.clone());
        }
        written.sort();
        // Only report notes for paths that were actually written.
        let notes = notes
            .into_iter()
            .filter(|n| sparse.includes(&n.path))
            .collect();
        Ok((written, notes))
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
    fn checkout_writes_branch_files() {
        let dir = TempDir::new().unwrap();
        seed_repo(dir.path(), &[("src/main.rs", b"fn main(){}"), ("README.md", b"hi")]);
        // Wipe the working tree, then check out.
        std::fs::remove_file(dir.path().join("src/main.rs")).unwrap();
        std::fs::remove_file(dir.path().join("README.md")).unwrap();

        let repo = Repository::open(dir.path()).unwrap();
        let wc = WorkingCopy::open(&repo, dir.path());
        let (written, _notes) = wc
            .checkout("main", &crate::sparse::SparseProfile::default())
            .unwrap();
        assert_eq!(written.len(), 2);
        assert_eq!(
            std::fs::read(dir.path().join("src/main.rs")).unwrap(),
            b"fn main(){}"
        );
    }

    #[test]
    fn checkout_merges_concurrent_same_file_edits_across_tips() {
        // Regression for the dogfooding finding: two agents editing the same
        // file on a multi-tip frontier must BOTH survive checkout — the old
        // last-writer-wins silently dropped one.
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (idn, key) = human();

        let base = repo
            .commit(
                ChangeBuilder::new(idn.clone(), key.clone())
                    .intent("base")
                    .file(FileChange {
                        path: "app.py".into(),
                        kind: FileKind::Text,
                        patch: b"def main():\n    pass\n".to_vec(),
                        conflicts: Vec::new(),
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
        repo.advance_branch("main", base).unwrap();

        // Agent A: add an `add()` call. Agent B: add a `list()` call. Both
        // descend from `base` (concurrent), both edit app.py.
        let a = repo
            .commit(
                ChangeBuilder::new(idn.clone(), key.clone())
                    .intent("agent-a: add")
                    .dep(base)
                    .file(FileChange {
                        path: "app.py".into(),
                        kind: FileKind::Text,
                        patch: b"def main():\n    add()\n    pass\n".to_vec(),
                        conflicts: Vec::new(),
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
        let b = repo
            .commit(
                ChangeBuilder::new(idn, key)
                    .intent("agent-b: list")
                    .dep(base)
                    .file(FileChange {
                        path: "app.py".into(),
                        kind: FileKind::Text,
                        patch: b"def main():\n    list()\n    pass\n".to_vec(),
                        conflicts: Vec::new(),
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
        repo.advance_branch("main", a).unwrap();
        repo.advance_branch("main", b).unwrap();
        // main is now a 2-tip frontier.
        assert_eq!(repo.refs().get("main").unwrap().0.len(), 2);

        let wc = WorkingCopy::open(&repo, dir.path());
        let (_written, notes) = wc
            .checkout("main", &crate::sparse::SparseProfile::default())
            .unwrap();
        let merged = std::fs::read_to_string(dir.path().join("app.py")).unwrap();
        // Neither agent's work was dropped.
        assert!(merged.contains("add()"), "agent-a's edit was dropped:\n{merged}");
        assert!(merged.contains("list()"), "agent-b's edit was dropped:\n{merged}");
        // The divergence was reported, not silent.
        assert!(notes.iter().any(|n| n.path == "app.py"));
    }

    #[test]
    fn sparse_checkout_skips_excluded_paths() {
        let dir = TempDir::new().unwrap();
        seed_repo(
            dir.path(),
            &[
                ("payments/api.rs", b"pay"),
                ("billing/api.rs", b"bill"),
                ("README.md", b"hi"),
            ],
        );
        for p in ["payments/api.rs", "billing/api.rs", "README.md"] {
            std::fs::remove_file(dir.path().join(p)).unwrap();
        }
        let repo = Repository::open(dir.path()).unwrap();
        let wc = WorkingCopy::open(&repo, dir.path());
        let profile = crate::sparse::SparseProfile {
            include: vec!["payments/**".into()],
            exclude: vec![],
        };
        let (written, _notes) = wc.checkout("main", &profile).unwrap();
        assert_eq!(written, vec!["payments/api.rs".to_string()]);
        assert!(dir.path().join("payments/api.rs").exists());
        assert!(!dir.path().join("billing/api.rs").exists());
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
