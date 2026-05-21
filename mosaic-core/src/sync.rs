//! Sync layer: bundles of changes that can travel between repositories.
//!
//! A bundle is a self-contained, signed, content-addressed envelope holding
//! some set of `Change`s plus their referenced blobs. The receiver verifies
//! every signature, walks parents to ensure causal closure, and registers
//! the changes in topological order. There is no notion of "rejected push" —
//! the receiver either accepts the bundle in full or rejects it for a
//! concrete reason (missing parent, bad signature, hash mismatch).

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::m1::change::{Change, ChangeId};
use crate::m1_dag::dag::ChangeStore;
use crate::m1_dag::refs::Frontier;
use crate::repo::Repository;
use crate::storage::Cas;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const BUNDLE_MAGIC: &[u8; 8] = b"MOSAICB1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub magic: [u8; 8],
    pub changes: Vec<Change>,
    pub blobs: BTreeMap<Hash, Vec<u8>>,
}

impl Bundle {
    pub fn new(changes: Vec<Change>, blobs: BTreeMap<Hash, Vec<u8>>) -> Self {
        Self {
            magic: *BUNDLE_MAGIC,
            changes,
            blobs,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        bincode::serialize(self).map_err(|e| Error::Serialization(e.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let b: Bundle = bincode::deserialize(bytes)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        if &b.magic != BUNDLE_MAGIC {
            return Err(Error::Serialization("bad bundle magic".into()));
        }
        Ok(b)
    }

    pub fn change_ids(&self) -> Vec<ChangeId> {
        self.changes.iter().map(Change::id).collect()
    }
}

/// Compute the change ids the receiver is missing, given a peer's known
/// frontier. Returns a topologically-ordered list starting from the oldest
/// shared boundary moving toward `tips`.
pub fn missing_changes_for(
    repo: &Repository,
    receiver_has: &Frontier,
    tips: &Frontier,
) -> Vec<ChangeId> {
    let mut already: BTreeSet<Hash> = BTreeSet::new();
    let mut queue: VecDeque<Hash> = receiver_has.0.iter().copied().collect();
    while let Some(h) = queue.pop_front() {
        if !already.insert(h) {
            continue;
        }
        if let Some(parents) = repo.index().parents_of(&h) {
            for p in parents {
                queue.push_back(*p);
            }
        }
    }

    let mut needed: BTreeSet<Hash> = BTreeSet::new();
    let mut queue: VecDeque<Hash> = tips.0.iter().copied().collect();
    while let Some(h) = queue.pop_front() {
        if already.contains(&h) || !needed.insert(h) {
            continue;
        }
        if let Some(parents) = repo.index().parents_of(&h) {
            for p in parents {
                if !already.contains(p) {
                    queue.push_back(*p);
                }
            }
        }
    }

    let ordered = repo.topo_order(&needed);
    ordered.into_iter().map(ChangeId).collect()
}

pub fn build_bundle(repo: &Repository, change_ids: &[ChangeId]) -> Result<Bundle> {
    let mut changes = Vec::with_capacity(change_ids.len());
    let mut blobs: BTreeMap<Hash, Vec<u8>> = BTreeMap::new();
    for id in change_ids {
        let change = repo.load_change(id)?;
        for file in &change.body {
            let hash = Hash::of(&file.patch);
            if repo.cas().has(&hash)? {
                let bytes = repo.cas().get(&hash)?;
                blobs.insert(hash, bytes);
            }
        }
        changes.push(change);
    }
    Ok(Bundle::new(changes, blobs))
}

#[derive(Debug, Clone)]
pub struct ApplyReport {
    pub applied: Vec<ChangeId>,
    pub skipped: Vec<ChangeId>,
}

pub fn apply_bundle(repo: &mut Repository, bundle: &Bundle) -> Result<ApplyReport> {
    if &bundle.magic != BUNDLE_MAGIC {
        return Err(Error::Serialization("bad bundle magic".into()));
    }

    let mut by_id: BTreeMap<ChangeId, Change> = BTreeMap::new();
    for change in &bundle.changes {
        change.verify()?;
        by_id.insert(change.id(), change.clone());
    }

    for (hash, bytes) in &bundle.blobs {
        let actual = Hash::of(bytes);
        if &actual != hash {
            return Err(Error::HashMismatch {
                expected: hash.to_hex(),
                actual: actual.to_hex(),
            });
        }
        repo.cas().put(bytes)?;
    }

    let mut remaining = by_id;
    let mut applied = Vec::new();
    let mut skipped = Vec::new();
    while !remaining.is_empty() {
        let ready: Vec<ChangeId> = remaining
            .iter()
            .filter(|(_, c)| {
                c.deps
                    .iter()
                    .all(|d| repo.index().contains(&d.0) || remaining.contains_key(d))
            })
            .filter(|(_, c)| {
                c.deps.iter().all(|d| repo.index().contains(&d.0))
            })
            .map(|(id, _)| *id)
            .collect();
        if ready.is_empty() {
            let pending: Vec<String> = remaining.keys().map(ChangeId::to_hex).collect();
            return Err(Error::UnknownParent(format!(
                "bundle has unresolvable deps; pending: {pending:?}"
            )));
        }
        for id in ready {
            let change = remaining.remove(&id).expect("ready key present");
            if repo.changes().has_change(&id.0)? {
                skipped.push(id);
                continue;
            }
            repo.commit(change)?;
            applied.push(id);
        }
    }

    Ok(ApplyReport { applied, skipped })
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

    fn fc(path: &str, body: &[u8]) -> FileChange {
        FileChange {
            path: path.into(),
            kind: FileKind::Text,
            patch: body.to_vec(),
            conflicts: Vec::new(),
        }
    }

    fn commit_one(
        repo: &mut Repository,
        id: &Identity,
        key: &SigningKey,
        intent: &str,
        parents: Vec<ChangeId>,
        files: Vec<FileChange>,
    ) -> ChangeId {
        let mut b = ChangeBuilder::new(id.clone(), key.clone()).intent(intent);
        for p in parents {
            b = b.dep(p);
        }
        for f in files {
            b = b.file(f);
        }
        repo.commit(b.build().unwrap()).unwrap()
    }

    #[test]
    fn bundle_round_trip_empty() {
        let b = Bundle::new(Vec::new(), BTreeMap::new());
        let bytes = b.encode().unwrap();
        let decoded = Bundle::decode(&bytes).unwrap();
        assert!(decoded.changes.is_empty());
        assert!(decoded.blobs.is_empty());
    }

    #[test]
    fn missing_changes_returns_topo_ordered_delta() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (idn, key) = human();

        let a = commit_one(&mut repo, &idn, &key, "a", vec![], vec![fc("a", b"a")]);
        let b = commit_one(&mut repo, &idn, &key, "b", vec![a], vec![fc("b", b"b")]);
        let c = commit_one(&mut repo, &idn, &key, "c", vec![b], vec![fc("c", b"c")]);

        let tips = Frontier(BTreeSet::from([c.0]));
        let receiver_has = Frontier(BTreeSet::from([a.0]));
        let needed = missing_changes_for(&repo, &receiver_has, &tips);
        let needed_hashes: Vec<Hash> = needed.iter().map(|c| c.0).collect();
        assert_eq!(needed_hashes, vec![b.0, c.0]);
    }

    #[test]
    fn bundle_transfers_changes_to_a_new_repo() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        let mut src = Repository::init(src_dir.path()).unwrap();
        let (idn, key) = human();
        let a = commit_one(&mut src, &idn, &key, "a", vec![], vec![fc("a", b"hello")]);
        let b = commit_one(&mut src, &idn, &key, "b", vec![a], vec![fc("b", b"world")]);

        let bundle = build_bundle(&src, &[a, b]).unwrap();
        let wire = bundle.encode().unwrap();

        let mut dst = Repository::init(dst_dir.path()).unwrap();
        let received = Bundle::decode(&wire).unwrap();
        let report = apply_bundle(&mut dst, &received).unwrap();

        assert_eq!(report.applied.len(), 2);
        assert!(report.skipped.is_empty());
        assert!(dst.index().contains(&a.0));
        assert!(dst.index().contains(&b.0));

        let loaded = dst.load_change(&b).unwrap();
        assert_eq!(loaded.intent.as_deref(), Some("b"));
    }

    #[test]
    fn second_apply_is_idempotent() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        let mut src = Repository::init(src_dir.path()).unwrap();
        let (idn, key) = human();
        let a = commit_one(&mut src, &idn, &key, "only", vec![], vec![fc("a", b"x")]);

        let bundle = build_bundle(&src, &[a]).unwrap();
        let mut dst = Repository::init(dst_dir.path()).unwrap();
        let r1 = apply_bundle(&mut dst, &bundle).unwrap();
        let r2 = apply_bundle(&mut dst, &bundle).unwrap();
        assert_eq!(r1.applied.len(), 1);
        assert_eq!(r2.applied.len(), 0);
        assert_eq!(r2.skipped.len(), 1);
    }

    #[test]
    fn apply_rejects_dangling_parent() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        let mut src = Repository::init(src_dir.path()).unwrap();
        let (idn, key) = human();
        let a = commit_one(&mut src, &idn, &key, "a", vec![], vec![fc("a", b"x")]);
        let b = commit_one(&mut src, &idn, &key, "b", vec![a], vec![fc("b", b"y")]);

        // Only ship `b`; `a` is missing from the bundle and from the destination.
        let bundle = build_bundle(&src, &[b]).unwrap();
        let mut dst = Repository::init(dst_dir.path()).unwrap();
        assert!(matches!(
            apply_bundle(&mut dst, &bundle),
            Err(Error::UnknownParent(_))
        ));
    }

    #[test]
    fn apply_rejects_tampered_change() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        let mut src = Repository::init(src_dir.path()).unwrap();
        let (idn, key) = human();
        let a = commit_one(&mut src, &idn, &key, "a", vec![], vec![fc("a", b"x")]);
        let _ = a;

        let mut bundle = build_bundle(&src, &[a]).unwrap();
        bundle.changes[0].intent = Some("evil rewrite".into());

        let mut dst = Repository::init(dst_dir.path()).unwrap();
        assert!(apply_bundle(&mut dst, &bundle).is_err());
    }

    #[test]
    fn bad_magic_rejected() {
        let mut bundle = Bundle::new(Vec::new(), BTreeMap::new());
        bundle.magic = *b"NOTABUND";
        let bytes = bincode::serialize(&bundle).unwrap();
        assert!(Bundle::decode(&bytes).is_err());
    }
}
