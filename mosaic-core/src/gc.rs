//! Garbage collection.
//!
//! Walks every branch's frontier transitively to compute the live change
//! set, derives the live CAS-blob set from those changes, and prunes
//! everything else.
//!
//! v1 is a two-pass mark + sweep on the local filesystem CAS:
//!
//!   `mos gc --dry-run`   reports what *would* be pruned
//!   `mos gc`             actually deletes orphans
//!
//! Safe by construction: we only delete objects with no reachable
//! reference. Lost a branch ref accidentally? Reachability shrinks
//! immediately — re-add the ref BEFORE running gc.

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::m1::change::ChangeId;
use crate::repo::{Repository, REPO_DIR};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

const OBJECTS: &str = "objects";
const CHANGES: &str = "changes";

#[derive(Debug, Default)]
pub struct GcReport {
    pub live_changes: usize,
    pub live_blobs: usize,
    pub pruned_changes: Vec<Hash>,
    pub pruned_blobs: Vec<Hash>,
    pub bytes_freed: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum GcMode {
    DryRun,
    Apply,
}

pub fn collect(repo: &Repository, mode: GcMode) -> Result<GcReport> {
    let live_changes = live_change_set(repo)?;
    let live_blobs = live_blob_set(repo, &live_changes)?;

    let mut report = GcReport {
        live_changes: live_changes.len(),
        live_blobs: live_blobs.len(),
        ..Default::default()
    };

    // Walk on-disk changes; prune anything not in live_changes.
    sweep_dir(
        &repo.root().join(CHANGES),
        &live_changes,
        &mut report.pruned_changes,
        &mut report.bytes_freed,
        mode,
    )?;

    // Walk on-disk blobs; prune anything not in live_blobs.
    sweep_dir(
        &repo.root().join(OBJECTS),
        &live_blobs,
        &mut report.pruned_blobs,
        &mut report.bytes_freed,
        mode,
    )?;

    Ok(report)
}

fn live_change_set(repo: &Repository) -> Result<BTreeSet<Hash>> {
    let mut live: BTreeSet<Hash> = BTreeSet::new();
    let mut queue: std::collections::VecDeque<Hash> = Default::default();

    for name in repo.refs().list()? {
        let f = repo.refs().get(&name)?;
        for tip in &f.0 {
            queue.push_back(*tip);
        }
    }

    while let Some(h) = queue.pop_front() {
        if !live.insert(h) {
            continue;
        }
        if let Some(parents) = repo.index().parents_of(&h) {
            for p in parents {
                queue.push_back(*p);
            }
        }
    }
    Ok(live)
}

fn live_blob_set(repo: &Repository, live_changes: &BTreeSet<Hash>) -> Result<BTreeSet<Hash>> {
    let mut live: BTreeSet<Hash> = BTreeSet::new();
    for h in live_changes {
        let change = repo.load_change(&ChangeId(*h))?;
        for file in &change.body {
            // FileChange.patch may itself be a manifest or raw bytes.
            // We hash exactly what's stored under the CAS contract.
            live.insert(Hash::of(&file.patch));
        }
    }
    Ok(live)
}

fn sweep_dir(
    dir: &std::path::Path,
    live: &BTreeSet<Hash>,
    pruned: &mut Vec<Hash>,
    bytes_freed: &mut u64,
    mode: GcMode,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for outer in fs::read_dir(dir).map_err(Error::Io)? {
        let outer = outer.map_err(Error::Io)?;
        if !outer.file_type().map_err(Error::Io)?.is_dir() {
            continue;
        }
        let prefix = outer
            .file_name()
            .to_string_lossy()
            .into_owned();
        for inner in fs::read_dir(outer.path()).map_err(Error::Io)? {
            let inner = inner.map_err(Error::Io)?;
            let rest = inner.file_name().to_string_lossy().into_owned();
            let hex = format!("{prefix}{rest}");
            let hash = match Hash::from_hex(&hex) {
                Ok(h) => h,
                Err(_) => continue,
            };
            if live.contains(&hash) {
                continue;
            }
            let path = inner.path();
            let size = path.metadata().map(|m| m.len()).unwrap_or(0);
            *bytes_freed += size;
            pruned.push(hash);
            if matches!(mode, GcMode::Apply) {
                let _ = fs::remove_file(&path);
            }
        }
    }
    Ok(())
}

pub fn repo_root(_repo: &Repository) -> PathBuf {
    PathBuf::from(REPO_DIR)
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
            Identity::human("dev@example.com", Some("Dev".into())).unwrap(),
            SigningKey::generate(),
        )
    }

    fn seed_change(repo: &mut Repository, intent: &str, parent: Option<ChangeId>) -> ChangeId {
        let (idn, key) = human();
        let mut builder = ChangeBuilder::new(idn, key).intent(intent);
        if let Some(p) = parent {
            builder = builder.dep(p);
        }
        builder = builder.file(FileChange {
            path: format!("{intent}.txt"),
            kind: FileKind::Text,
            patch: format!("body of {intent}").into_bytes(),
            conflicts: Vec::new(),
        });
        let id = repo.commit(builder.build().unwrap()).unwrap();
        repo.advance_branch("main", id).unwrap();
        id
    }

    #[test]
    fn dry_run_reports_zero_when_everything_reachable() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let _ = seed_change(&mut repo, "a", None);
        let report = collect(&repo, GcMode::DryRun).unwrap();
        assert_eq!(report.live_changes, 1);
        assert!(report.pruned_changes.is_empty());
    }

    #[test]
    fn orphaned_changes_pruned_in_apply_mode() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let _a = seed_change(&mut repo, "a", None);
        let _b = seed_change(&mut repo, "b", Some(_a));

        // Reset branch to point only at A, orphaning B.
        repo.refs()
            .put(
                "main",
                &crate::m1_dag::refs::Frontier(BTreeSet::from([_a.0])),
            )
            .unwrap();

        let dry = collect(&repo, GcMode::DryRun).unwrap();
        assert_eq!(dry.live_changes, 1);
        // B is orphaned → pruned set non-empty (B and B's blob).
        assert!(dry.pruned_changes.contains(&_b.0));

        let apply = collect(&repo, GcMode::Apply).unwrap();
        assert!(apply.bytes_freed > 0);

        // After apply, loading B fails.
        assert!(repo.load_change(&_b).is_err());
        // A still loads fine.
        let a_change = repo.load_change(&_a).unwrap();
        assert_eq!(a_change.intent.as_deref(), Some("a"));
    }

    #[test]
    fn dry_run_does_not_delete() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let _a = seed_change(&mut repo, "a", None);
        let _b = seed_change(&mut repo, "b", Some(_a));
        repo.refs()
            .put(
                "main",
                &crate::m1_dag::refs::Frontier(BTreeSet::from([_a.0])),
            )
            .unwrap();

        let dry = collect(&repo, GcMode::DryRun).unwrap();
        assert!(!dry.pruned_changes.is_empty());
        // B is STILL loadable because we only previewed.
        assert!(repo.load_change(&_b).is_ok());
    }

    #[test]
    fn keeps_all_branch_tips_alive() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let a = seed_change(&mut repo, "a", None);
        let b = seed_change(&mut repo, "b", Some(a));
        // Branch "feature" points at b. main still points at b too.
        repo.refs()
            .put(
                "feature",
                &crate::m1_dag::refs::Frontier(BTreeSet::from([b.0])),
            )
            .unwrap();

        // Move main back to a — but feature still references b.
        repo.refs()
            .put(
                "main",
                &crate::m1_dag::refs::Frontier(BTreeSet::from([a.0])),
            )
            .unwrap();

        let report = collect(&repo, GcMode::DryRun).unwrap();
        // Both a and b are live (feature keeps b alive).
        assert_eq!(report.live_changes, 2);
        assert!(report.pruned_changes.is_empty());
    }
}
