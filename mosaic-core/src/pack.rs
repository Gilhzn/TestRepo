//! Packfiles — a single compressed archive of many changes + blobs.
//!
//! Where a `Bundle` (see `sync`) is the wire format for an incremental
//! delta, a packfile is the *archival* format: pack an entire repository's
//! change history and referenced blobs into one zstd-level-19 file for cold
//! storage, offline transfer (sneakernet a monorepo), or backup. A small
//! index maps each object hash to its (kind, offset, length) in the
//! decompressed data blob.

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::m1::change::ChangeId;
use crate::m1_dag::dag::ChangeStore;
use crate::repo::Repository;
use crate::storage::Cas;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PACK_MAGIC: &[u8; 8] = b"MOSAICP1";
const ZSTD_ARCHIVE_LEVEL: i32 = 19;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjKind {
    Change,
    Blob,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexEntry {
    pub kind: ObjKind,
    pub offset: u64,
    pub len: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Packfile {
    pub magic: [u8; 8],
    pub index: BTreeMap<Hash, IndexEntry>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    pub changes: usize,
    pub blobs: usize,
    pub skipped: usize,
}

impl Packfile {
    pub fn change_count(&self) -> usize {
        self.index
            .values()
            .filter(|e| e.kind == ObjKind::Change)
            .count()
    }

    pub fn blob_count(&self) -> usize {
        self.index
            .values()
            .filter(|e| e.kind == ObjKind::Blob)
            .count()
    }

    /// Serialize + zstd-19 compress the whole packfile.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let raw =
            bincode::serialize(self).map_err(|e| Error::Serialization(e.to_string()))?;
        zstd::encode_all(raw.as_slice(), ZSTD_ARCHIVE_LEVEL)
            .map_err(|e| Error::Serialization(format!("zstd: {e}")))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let raw = zstd::decode_all(bytes)
            .map_err(|e| Error::Serialization(format!("zstd decode: {e}")))?;
        let pack: Packfile = bincode::deserialize(&raw)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        if &pack.magic != PACK_MAGIC {
            return Err(Error::Serialization("bad packfile magic".into()));
        }
        Ok(pack)
    }

    fn slice(&self, e: &IndexEntry) -> &[u8] {
        &self.data[e.offset as usize..(e.offset + e.len) as usize]
    }
}

/// Pack every change in the repo plus every blob those changes reference.
pub fn pack_repo(repo: &Repository) -> Result<Packfile> {
    let ids: Vec<ChangeId> = repo
        .all_change_ids()?
        .into_iter()
        .map(ChangeId)
        .collect();
    pack_changes(repo, &ids)
}

/// Pack a specific set of changes (+ their referenced blobs).
pub fn pack_changes(repo: &Repository, change_ids: &[ChangeId]) -> Result<Packfile> {
    let mut data: Vec<u8> = Vec::new();
    let mut index: BTreeMap<Hash, IndexEntry> = BTreeMap::new();

    for id in change_ids {
        let change = repo.load_change(id)?;
        let bytes = bincode::serialize(&change)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        let offset = data.len() as u64;
        data.extend_from_slice(&bytes);
        index.insert(
            id.0,
            IndexEntry {
                kind: ObjKind::Change,
                offset,
                len: bytes.len() as u64,
            },
        );

        // Pack referenced blobs.
        for file in &change.body {
            let bh = Hash::of(&file.patch);
            if index.contains_key(&bh) {
                continue;
            }
            if repo.cas().has(&bh)? {
                let blob = repo.cas().get(&bh)?;
                let offset = data.len() as u64;
                data.extend_from_slice(&blob);
                index.insert(
                    bh,
                    IndexEntry {
                        kind: ObjKind::Blob,
                        offset,
                        len: blob.len() as u64,
                    },
                );
            }
        }
    }

    Ok(Packfile {
        magic: *PACK_MAGIC,
        index,
        data,
    })
}

/// Restore a packfile into a repo: write blobs to the CAS, register changes
/// parent-first (verifying signatures). Idempotent — already-present changes
/// are skipped.
pub fn restore_into(repo: &mut Repository, pack: &Packfile) -> Result<RestoreReport> {
    let mut report = RestoreReport::default();

    // Blobs first.
    for (_h, e) in pack.index.iter().filter(|(_, e)| e.kind == ObjKind::Blob) {
        repo.cas().put(pack.slice(e))?;
        report.blobs += 1;
    }

    // Changes, parent-first.
    let mut pending: BTreeMap<ChangeId, crate::m1::change::Change> = BTreeMap::new();
    for (h, e) in pack.index.iter().filter(|(_, e)| e.kind == ObjKind::Change) {
        let change: crate::m1::change::Change = bincode::deserialize(pack.slice(e))
            .map_err(|e| Error::Serialization(e.to_string()))?;
        change.verify()?;
        pending.insert(ChangeId(*h), change);
    }

    while !pending.is_empty() {
        let ready: Vec<ChangeId> = pending
            .iter()
            .filter(|(_, c)| c.deps.iter().all(|d| repo.index().contains(&d.0)))
            .map(|(id, _)| *id)
            .collect();
        if ready.is_empty() {
            return Err(Error::UnknownParent(
                "packfile has changes with unresolvable parents".into(),
            ));
        }
        for id in ready {
            let change = pending.remove(&id).expect("ready key present");
            if repo.changes().has_change(&id.0)? {
                report.skipped += 1;
                continue;
            }
            repo.commit(change)?;
            report.changes += 1;
        }
    }

    Ok(report)
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
        let (idn, key) = human();
        let mut b = ChangeBuilder::new(idn, key).intent(intent);
        if let Some(p) = parent {
            b = b.dep(p);
        }
        b = b.file(FileChange {
            path: format!("{intent}.txt"),
            kind: FileKind::Text,
            patch: format!("content of {intent}").into_bytes(),
            conflicts: Vec::new(),
        });
        let id = repo.commit(b.build().unwrap()).unwrap();
        repo.advance_branch("main", id).unwrap();
        id
    }

    #[test]
    fn pack_then_restore_round_trip() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let mut repo = Repository::init(src.path()).unwrap();
        let a = commit(&mut repo, "a", None);
        let b = commit(&mut repo, "b", Some(a));
        let _c = commit(&mut repo, "c", Some(b));

        let pack = pack_repo(&repo).unwrap();
        assert_eq!(pack.change_count(), 3);

        let mut dst_repo = Repository::init(dst.path()).unwrap();
        let report = restore_into(&mut dst_repo, &pack).unwrap();
        assert_eq!(report.changes, 3);
        assert!(dst_repo.changes().has_change(&a.0).unwrap());
        assert!(dst_repo.changes().has_change(&_c.0).unwrap());
    }

    #[test]
    fn encode_decode_round_trip() {
        let src = TempDir::new().unwrap();
        let mut repo = Repository::init(src.path()).unwrap();
        let _a = commit(&mut repo, "only", None);
        let pack = pack_repo(&repo).unwrap();
        let bytes = pack.encode().unwrap();
        let back = Packfile::decode(&bytes).unwrap();
        assert_eq!(back.change_count(), 1);
        assert_eq!(back.index.len(), pack.index.len());
    }

    #[test]
    fn restore_is_idempotent() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let mut repo = Repository::init(src.path()).unwrap();
        commit(&mut repo, "x", None);
        let pack = pack_repo(&repo).unwrap();

        let mut dst_repo = Repository::init(dst.path()).unwrap();
        let r1 = restore_into(&mut dst_repo, &pack).unwrap();
        let r2 = restore_into(&mut dst_repo, &pack).unwrap();
        assert_eq!(r1.changes, 1);
        assert_eq!(r2.changes, 0);
        assert_eq!(r2.skipped, 1);
    }

    #[test]
    fn bad_magic_rejected() {
        let src = TempDir::new().unwrap();
        let mut repo = Repository::init(src.path()).unwrap();
        commit(&mut repo, "x", None);
        let mut pack = pack_repo(&repo).unwrap();
        pack.magic = *b"NOTPACK!";
        let bytes = pack.encode().unwrap();
        assert!(Packfile::decode(&bytes).is_err());
    }

    #[test]
    fn encoded_pack_is_compressed() {
        let src = TempDir::new().unwrap();
        let mut repo = Repository::init(src.path()).unwrap();
        // Many similar changes compress well.
        let mut parent = None;
        for i in 0..20 {
            parent = Some(commit(&mut repo, &format!("change number {i}"), parent));
        }
        let pack = pack_repo(&repo).unwrap();
        let encoded = pack.encode().unwrap();
        let raw = bincode::serialize(&pack).unwrap();
        assert!(
            encoded.len() < raw.len(),
            "zstd-19 should shrink the archive ({} >= {})",
            encoded.len(),
            raw.len()
        );
    }
}
