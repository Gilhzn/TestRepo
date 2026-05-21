//! History DAG: per-change parents/children index and a content-addressed
//! `ChangeStore` for the serialized change bytes.
//!
//! Vector clocks live alongside the structural edges; the index is rebuilt
//! by replaying `register` calls over the changes loaded from the store.

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::m1_dag::vclock::VectorClock;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub trait ChangeStore {
    fn put_change(&self, id: Hash, bytes: &[u8]) -> Result<()>;
    fn get_change(&self, id: &Hash) -> Result<Vec<u8>>;
    fn has_change(&self, id: &Hash) -> Result<bool>;
}

#[derive(Default, Debug, Clone)]
pub struct DagIndex {
    parents: BTreeMap<Hash, Vec<Hash>>,
    children: BTreeMap<Hash, Vec<Hash>>,
    vclocks: BTreeMap<Hash, VectorClock>,
}

impl DagIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, id: &Hash) -> bool {
        self.parents.contains_key(id)
    }

    pub fn parents_of(&self, id: &Hash) -> Option<&[Hash]> {
        self.parents.get(id).map(|v| v.as_slice())
    }

    pub fn children_of(&self, id: &Hash) -> Option<&[Hash]> {
        self.children.get(id).map(|v| v.as_slice())
    }

    pub fn vclock_of(&self, id: &Hash) -> Option<&VectorClock> {
        self.vclocks.get(id)
    }

    pub fn len(&self) -> usize {
        self.parents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    pub fn register(&mut self, id: Hash, parents: Vec<Hash>, vclock: VectorClock) -> Result<()> {
        for p in &parents {
            if !self.parents.contains_key(p) {
                return Err(Error::UnknownParent(p.to_hex()));
            }
        }
        if self.parents.contains_key(&id) {
            return Ok(());
        }
        for p in &parents {
            let entry = self.children.entry(*p).or_default();
            if !entry.contains(&id) {
                entry.push(id);
            }
        }
        self.children.entry(id).or_default();
        self.parents.insert(id, parents);
        self.vclocks.insert(id, vclock);
        Ok(())
    }

    pub fn frontier(&self, set: &BTreeSet<Hash>) -> BTreeSet<Hash> {
        let mut out = BTreeSet::new();
        for id in set {
            let has_child_in_set = self
                .children
                .get(id)
                .map(|cs| cs.iter().any(|c| set.contains(c)))
                .unwrap_or(false);
            if !has_child_in_set {
                out.insert(*id);
            }
        }
        out
    }

    pub fn ancestors_of(&self, id: &Hash) -> BTreeSet<Hash> {
        let mut out = BTreeSet::new();
        let mut stack: Vec<Hash> = match self.parents.get(id) {
            Some(ps) => ps.clone(),
            None => return out,
        };
        while let Some(cur) = stack.pop() {
            if out.insert(cur) {
                if let Some(ps) = self.parents.get(&cur) {
                    stack.extend(ps.iter().copied());
                }
            }
        }
        out
    }

    pub fn is_ancestor(&self, a: &Hash, b: &Hash) -> bool {
        if a == b {
            return false;
        }
        self.ancestors_of(b).contains(a)
    }

    pub fn common_ancestors(&self, a: &Hash, b: &Hash) -> BTreeSet<Hash> {
        let aa = self.ancestors_of(a);
        let bb = self.ancestors_of(b);
        aa.intersection(&bb).copied().collect()
    }

    pub fn topological_order(&self, set: &BTreeSet<Hash>) -> Vec<Hash> {
        let mut indeg: BTreeMap<Hash, usize> = BTreeMap::new();
        for id in set {
            let mut d = 0usize;
            if let Some(ps) = self.parents.get(id) {
                for p in ps {
                    if set.contains(p) {
                        d += 1;
                    }
                }
            }
            indeg.insert(*id, d);
        }

        let mut ready: BTreeSet<Hash> = indeg
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(h, _)| *h)
            .collect();
        let mut order = Vec::with_capacity(set.len());

        while let Some(next) = ready.iter().next().copied() {
            ready.remove(&next);
            order.push(next);
            if let Some(children) = self.children.get(&next) {
                for c in children {
                    if let Some(d) = indeg.get_mut(c) {
                        if *d > 0 {
                            *d -= 1;
                            if *d == 0 {
                                ready.insert(*c);
                            }
                        }
                    }
                }
            }
        }
        order
    }

    pub fn heads(&self) -> BTreeSet<Hash> {
        let all: BTreeSet<Hash> = self.parents.keys().copied().collect();
        self.frontier(&all)
    }

    pub fn reachable_bfs(&self, roots: &BTreeSet<Hash>) -> BTreeSet<Hash> {
        let mut out = BTreeSet::new();
        let mut q: VecDeque<Hash> = roots.iter().copied().collect();
        while let Some(cur) = q.pop_front() {
            if out.insert(cur) {
                if let Some(ps) = self.parents.get(&cur) {
                    q.extend(ps.iter().copied());
                }
            }
        }
        out
    }
}

/// Filesystem-backed `ChangeStore`. Stores blobs at
/// `<root>/changes/<hex[..2]>/<hex[2..]>` as raw bytes (uncompressed — change
/// bodies are tiny vs. blob content and may be re-read by indexing code often).
pub struct FsChangeStore {
    root: PathBuf,
}

impl FsChangeStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let changes = root.join("changes");
        fs::create_dir_all(&changes)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join("changes").join(&hex[..2]).join(&hex[2..])
    }
}

impl ChangeStore for FsChangeStore {
    fn put_change(&self, id: Hash, bytes: &[u8]) -> Result<()> {
        let path = self.path_for(&id);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn get_change(&self, id: &Hash) -> Result<Vec<u8>> {
        let path = self.path_for(id);
        let mut f = fs::File::open(&path)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn has_change(&self, id: &Hash) -> Result<bool> {
        Ok(self.path_for(id).exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn mock_change_id(seed: u8) -> Hash {
        Hash::of(&[seed; 32])
    }

    fn vc(entries: &[(&str, u64)]) -> VectorClock {
        let mut v = VectorClock::new();
        for (k, n) in entries {
            for _ in 0..*n {
                v.increment(*k);
            }
        }
        v
    }

    #[test]
    fn linear_chain_frontier_and_ancestors() {
        let mut idx = DagIndex::new();
        let a = mock_change_id(1);
        let b = mock_change_id(2);
        let c = mock_change_id(3);
        idx.register(a, vec![], vc(&[("a", 1)])).unwrap();
        idx.register(b, vec![a], vc(&[("a", 2)])).unwrap();
        idx.register(c, vec![b], vc(&[("a", 3)])).unwrap();

        let set: BTreeSet<Hash> = [a, b, c].into_iter().collect();
        let front = idx.frontier(&set);
        assert_eq!(front, [c].into_iter().collect());

        let anc = idx.ancestors_of(&c);
        assert_eq!(anc, [a, b].into_iter().collect());

        assert!(idx.is_ancestor(&a, &c));
        assert!(idx.is_ancestor(&b, &c));
        assert!(!idx.is_ancestor(&c, &a));
        assert!(!idx.is_ancestor(&a, &a));
    }

    #[test]
    fn diamond_common_ancestors_and_topo() {
        let mut idx = DagIndex::new();
        let a = mock_change_id(1);
        let b = mock_change_id(2);
        let c = mock_change_id(3);
        let d = mock_change_id(4);
        idx.register(a, vec![], vc(&[("x", 1)])).unwrap();
        idx.register(b, vec![a], vc(&[("x", 2)])).unwrap();
        idx.register(c, vec![a], vc(&[("y", 1)])).unwrap();
        idx.register(d, vec![b, c], vc(&[("x", 2), ("y", 1)])).unwrap();

        let common = idx.common_ancestors(&b, &c);
        assert_eq!(common, [a].into_iter().collect());

        let set: BTreeSet<Hash> = [a, b, c, d].into_iter().collect();
        let order = idx.topological_order(&set);
        assert_eq!(order.len(), 4);
        let pos = |h: &Hash| order.iter().position(|x| x == h).unwrap();
        assert!(pos(&a) < pos(&b));
        assert!(pos(&a) < pos(&c));
        assert!(pos(&b) < pos(&d));
        assert!(pos(&c) < pos(&d));
    }

    #[test]
    fn concurrent_branches_have_two_heads() {
        let mut idx = DagIndex::new();
        let root = mock_change_id(0);
        let left = mock_change_id(1);
        let right = mock_change_id(2);
        idx.register(root, vec![], vc(&[("r", 1)])).unwrap();
        idx.register(left, vec![root], vc(&[("a", 1), ("r", 1)]))
            .unwrap();
        idx.register(right, vec![root], vc(&[("b", 1), ("r", 1)]))
            .unwrap();

        let set: BTreeSet<Hash> = [root, left, right].into_iter().collect();
        let front = idx.frontier(&set);
        assert_eq!(front, [left, right].into_iter().collect());

        let common = idx.common_ancestors(&left, &right);
        assert_eq!(common, [root].into_iter().collect());
    }

    #[test]
    fn register_fails_when_parent_unknown() {
        let mut idx = DagIndex::new();
        let ghost = mock_change_id(99);
        let new = mock_change_id(1);
        let err = idx.register(new, vec![ghost], vc(&[("a", 1)]));
        assert!(matches!(err, Err(Error::UnknownParent(_))));
    }

    #[test]
    fn topo_is_deterministic_for_independent_nodes() {
        let mut idx = DagIndex::new();
        let a = mock_change_id(1);
        let b = mock_change_id(2);
        let c = mock_change_id(3);
        idx.register(a, vec![], vc(&[])).unwrap();
        idx.register(b, vec![], vc(&[])).unwrap();
        idx.register(c, vec![], vc(&[])).unwrap();
        let set: BTreeSet<Hash> = [a, b, c].into_iter().collect();

        let o1 = idx.topological_order(&set);
        let o2 = idx.topological_order(&set);
        assert_eq!(o1, o2);
        // Tie-break is BTreeSet order: ascending by hash.
        let mut sorted = vec![a, b, c];
        sorted.sort();
        assert_eq!(o1, sorted);
    }

    #[test]
    fn fs_change_store_round_trip() {
        let dir = TempDir::new().unwrap();
        let store = FsChangeStore::open(dir.path()).unwrap();
        let id = mock_change_id(7);
        let bytes = b"serialized change payload";
        assert!(!store.has_change(&id).unwrap());
        store.put_change(id, bytes).unwrap();
        assert!(store.has_change(&id).unwrap());
        let got = store.get_change(&id).unwrap();
        assert_eq!(got, bytes);
    }

    #[test]
    fn fs_change_store_put_idempotent() {
        let dir = TempDir::new().unwrap();
        let store = FsChangeStore::open(dir.path()).unwrap();
        let id = mock_change_id(8);
        store.put_change(id, b"first").unwrap();
        store.put_change(id, b"second-ignored").unwrap();
        assert_eq!(store.get_change(&id).unwrap(), b"first");
    }

    #[test]
    fn fs_change_store_missing_errors() {
        let dir = TempDir::new().unwrap();
        let store = FsChangeStore::open(dir.path()).unwrap();
        let ghost = mock_change_id(123);
        assert!(!store.has_change(&ghost).unwrap());
        assert!(store.get_change(&ghost).is_err());
    }
}
