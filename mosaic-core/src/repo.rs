//! Repository facade: ties storage, change-DAG, refs, and identity into one
//! API that the CLI and SDK can drive.
//!
//! On-disk layout:
//! ```text
//! <root>/
//!   objects/        BLAKE3-keyed CAS for blobs (manifests, chunks, change bodies)
//!   changes/        raw change bytes keyed by hash (same scheme as objects)
//!   refs/           one file per branch, contents = newline-separated frontier
//!   identity/       local signing key + identity metadata
//! ```

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::m1::change::{Change, ChangeId};
use crate::m1::identity::Identity;
use crate::m1::signing::SigningKey;
use crate::m1_dag::dag::{ChangeStore, DagIndex};
use crate::m1_dag::refs::{Frontier, RefStore};
use crate::m1_dag::vclock::VectorClock;
use crate::storage::FsCas;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const REPO_DIR: &str = ".mosaic";
const OBJECTS: &str = "objects";
const CHANGES: &str = "changes";
const IDENTITY: &str = "identity";
const HEAD_REF: &str = "main";

pub struct Repository {
    root: PathBuf,
    cas: FsCas,
    changes: FsChangeStore,
    refs: RefStore,
    index: DagIndex,
}

impl Repository {
    pub fn init(parent: impl AsRef<Path>) -> Result<Self> {
        let root = parent.as_ref().join(REPO_DIR);
        if root.exists() {
            return Err(Error::Serialization(format!(
                "{REPO_DIR} already exists at {}",
                parent.as_ref().display()
            )));
        }
        fs::create_dir_all(root.join(OBJECTS))?;
        fs::create_dir_all(root.join(CHANGES))?;
        fs::create_dir_all(root.join(IDENTITY))?;
        Self::open(parent)
    }

    pub fn open(parent: impl AsRef<Path>) -> Result<Self> {
        let root = parent.as_ref().join(REPO_DIR);
        if !root.exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no {REPO_DIR}/ at {}", parent.as_ref().display()),
            )));
        }
        let cas = FsCas::open(root.join(OBJECTS))?;
        let changes = FsChangeStore::open(root.join(CHANGES))?;
        let refs = RefStore::open(&root)?;
        let mut index = DagIndex::new();
        load_index(&changes, &mut index)?;
        Ok(Self {
            root,
            cas,
            changes,
            refs,
            index,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cas(&self) -> &FsCas {
        &self.cas
    }

    pub fn changes(&self) -> &FsChangeStore {
        &self.changes
    }

    pub fn refs(&self) -> &RefStore {
        &self.refs
    }

    pub fn index(&self) -> &DagIndex {
        &self.index
    }

    pub fn commit(&mut self, change: Change) -> Result<ChangeId> {
        change.verify()?;
        for dep in &change.deps {
            if !self.index.contains(&dep.0) {
                return Err(Error::UnknownParent(dep.to_hex()));
            }
        }
        let id = change.id();
        let bytes = bincode::serialize(&change)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        self.changes.put_change(id.0, &bytes)?;
        let mut vclock = self.merged_parent_vclock(&change.deps);
        vclock.increment(change.author.id());
        let parents: Vec<Hash> = change.deps.iter().map(|c| c.0).collect();
        self.index.register(id.0, parents, vclock)?;
        Ok(id)
    }

    pub fn load_change(&self, id: &ChangeId) -> Result<Change> {
        let bytes = self.changes.get_change(&id.0)?;
        let change: Change = bincode::deserialize(&bytes)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        change.verify()?;
        Ok(change)
    }

    pub fn head(&self) -> Result<Frontier> {
        match self.refs.get(HEAD_REF) {
            Ok(f) => Ok(f),
            Err(Error::RefNotFound(_)) => Ok(Frontier::default()),
            Err(e) => Err(e),
        }
    }

    pub fn set_head(&self, frontier: &Frontier) -> Result<()> {
        self.refs.put(HEAD_REF, frontier)
    }

    pub fn advance_branch(&self, name: &str, new_tip: ChangeId) -> Result<Frontier> {
        let mut frontier = match self.refs.get(name) {
            Ok(f) => f,
            Err(Error::RefNotFound(_)) => Frontier::default(),
            Err(e) => return Err(e),
        };
        let ancestors = self.index.ancestors_of(&new_tip.0);
        frontier.0.retain(|h| !ancestors.contains(h));
        frontier.0.insert(new_tip.0);
        self.refs.put(name, &frontier)?;
        Ok(frontier)
    }

    pub fn topo_order(&self, set: &BTreeSet<Hash>) -> Vec<Hash> {
        self.index.topological_order(set)
    }

    pub fn all_change_ids(&self) -> Result<BTreeSet<Hash>> {
        Ok(self.changes.iter_ids()?.into_iter().collect())
    }

    fn merged_parent_vclock(&self, deps: &[ChangeId]) -> VectorClock {
        let mut clock = VectorClock::new();
        for dep in deps {
            if let Some(pv) = self.index.vclock_of(&dep.0) {
                clock.merge(pv);
            }
        }
        clock
    }

    pub fn save_identity(&self, identity: &Identity, signing_key: &SigningKey) -> Result<()> {
        let dir = self.root.join(IDENTITY);
        let id_bytes = bincode::serialize(identity)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        fs::write(dir.join("identity.bin"), id_bytes)?;
        fs::write(dir.join("key.secret"), signing_key.to_bytes())?;
        fs::write(dir.join("key.public"), signing_key.verifying_key().to_bytes())?;
        Ok(())
    }

    pub fn load_identity(&self) -> Result<(Identity, SigningKey)> {
        let dir = self.root.join(IDENTITY);
        let id_bytes = fs::read(dir.join("identity.bin"))?;
        let identity: Identity = bincode::deserialize(&id_bytes)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        let secret = fs::read(dir.join("key.secret"))?;
        let secret: [u8; crate::m1::signing::SIGNING_KEY_LEN] =
            secret.as_slice().try_into().map_err(|_| Error::InvalidKeyLength {
                expected: crate::m1::signing::SIGNING_KEY_LEN,
                actual: secret.len(),
            })?;
        let key = SigningKey::from_bytes(&secret);
        Ok((identity, key))
    }

    pub fn has_identity(&self) -> bool {
        self.root.join(IDENTITY).join("key.secret").exists()
    }
}

pub struct FsChangeStore {
    root: PathBuf,
}

impl FsChangeStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let root = path.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, id: &Hash) -> PathBuf {
        let hex = id.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    pub fn iter_ids(&self) -> Result<Vec<Hash>> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        for outer in fs::read_dir(&self.root)? {
            let outer = outer?;
            if !outer.file_type()?.is_dir() {
                continue;
            }
            let prefix = outer
                .file_name()
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::Serialization("non-utf8 dir name".into()))?;
            for inner in fs::read_dir(outer.path())? {
                let inner = inner?;
                let rest = inner
                    .file_name()
                    .to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| Error::Serialization("non-utf8 file name".into()))?;
                let hex = format!("{prefix}{rest}");
                if let Ok(h) = Hash::from_hex(&hex) {
                    out.push(h);
                }
            }
        }
        out.sort();
        Ok(out)
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
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn get_change(&self, id: &Hash) -> Result<Vec<u8>> {
        let path = self.path_for(id);
        let mut buf = Vec::new();
        fs::File::open(&path)?.read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn has_change(&self, id: &Hash) -> Result<bool> {
        Ok(self.path_for(id).exists())
    }
}

fn load_index(store: &FsChangeStore, index: &mut DagIndex) -> Result<()> {
    let mut by_id: BTreeMap<Hash, Change> = BTreeMap::new();
    for id in store.iter_ids()? {
        let bytes = store.get_change(&id)?;
        let change: Change = bincode::deserialize(&bytes)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        by_id.insert(id, change);
    }
    let mut remaining: BTreeMap<Hash, Change> = by_id;
    while !remaining.is_empty() {
        let ready: Vec<Hash> = remaining
            .iter()
            .filter(|(_, c)| c.deps.iter().all(|d| index.contains(&d.0)))
            .map(|(h, _)| *h)
            .collect();
        if ready.is_empty() {
            return Err(Error::Serialization(
                "change store has cyclic or unresolvable deps".into(),
            ));
        }
        for id in ready {
            let change = remaining.remove(&id).expect("ready key present");
            let parents: Vec<Hash> = change.deps.iter().map(|c| c.0).collect();
            let mut vclock = VectorClock::new();
            for dep in &change.deps {
                if let Some(pv) = index.vclock_of(&dep.0) {
                    vclock.merge(pv);
                }
            }
            vclock.increment(change.author.id());
            index.register(id, parents, vclock)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m1::change::{ChangeBuilder, FileChange, FileKind};
    use crate::m1::identity::Identity;
    use crate::m1::signing::SigningKey;
    use tempfile::TempDir;

    fn human() -> (Identity, SigningKey) {
        let id = Identity::human("eyal@example.com", Some("Eyal".into())).unwrap();
        (id, SigningKey::generate())
    }

    fn empty_file_change(path: &str) -> FileChange {
        FileChange {
            path: path.into(),
            kind: FileKind::Text,
            patch: Vec::new(),
            conflicts: Vec::new(),
        }
    }

    #[test]
    fn init_creates_layout() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        assert!(repo.root().exists());
        assert!(repo.root().join(OBJECTS).exists());
        assert!(repo.root().join(CHANGES).exists());
    }

    #[test]
    fn commit_round_trip_through_persistence() {
        let dir = TempDir::new().unwrap();
        let id_h = {
            let mut repo = Repository::init(dir.path()).unwrap();
            let (identity, key) = human();
            let change = ChangeBuilder::new(identity, key)
                .intent("first")
                .file(empty_file_change("readme.md"))
                .build()
                .unwrap();
            let h = repo.commit(change).unwrap();
            assert_eq!(repo.index().len(), 1);
            h
        };

        let repo = Repository::open(dir.path()).unwrap();
        assert_eq!(repo.index().len(), 1);
        let change = repo.load_change(&id_h).unwrap();
        assert_eq!(change.intent.as_deref(), Some("first"));
    }

    #[test]
    fn unknown_parent_rejected() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (identity, key) = human();
        let bogus = ChangeId(Hash::of(b"not-a-real-change"));
        let change = ChangeBuilder::new(identity, key)
            .intent("bad")
            .dep(bogus)
            .file(empty_file_change("x"))
            .build()
            .unwrap();
        assert!(matches!(
            repo.commit(change),
            Err(Error::UnknownParent(_))
        ));
    }

    #[test]
    fn advance_branch_drops_ancestors() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (identity, key) = human();

        let a = ChangeBuilder::new(identity.clone(), key.clone())
            .intent("a")
            .file(empty_file_change("a"))
            .build()
            .unwrap();
        let a_id = repo.commit(a).unwrap();

        let b = ChangeBuilder::new(identity.clone(), key.clone())
            .intent("b")
            .dep(a_id)
            .file(empty_file_change("b"))
            .build()
            .unwrap();
        let b_id = repo.commit(b).unwrap();

        repo.advance_branch("feature", a_id).unwrap();
        let f = repo.advance_branch("feature", b_id).unwrap();
        assert_eq!(f.0.len(), 1);
        assert!(f.0.contains(&b_id.0));
    }

    #[test]
    fn identity_persisted_across_reopen() {
        let dir = TempDir::new().unwrap();
        let (identity, key) = human();
        {
            let repo = Repository::init(dir.path()).unwrap();
            repo.save_identity(&identity, &key).unwrap();
        }
        let repo = Repository::open(dir.path()).unwrap();
        let (loaded_id, loaded_key) = repo.load_identity().unwrap();
        assert_eq!(loaded_id, identity);
        assert_eq!(
            loaded_key.verifying_key().to_bytes(),
            key.verifying_key().to_bytes()
        );
    }

    #[test]
    fn parallel_commits_share_root_and_merge_vclocks() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (h_id, h_key) = human();
        let agent_id = Identity::agent(
            "agent-a",
            "sess-1",
            Identity::human("eyal@example.com", None).unwrap(),
        )
        .unwrap();
        let agent_key = SigningKey::generate();

        let root = ChangeBuilder::new(h_id.clone(), h_key.clone())
            .intent("root")
            .file(empty_file_change("readme.md"))
            .build()
            .unwrap();
        let root_id = repo.commit(root).unwrap();

        let human_branch = ChangeBuilder::new(h_id.clone(), h_key.clone())
            .intent("human work")
            .dep(root_id)
            .file(empty_file_change("h"))
            .build()
            .unwrap();
        let h_change_id = repo.commit(human_branch).unwrap();

        let agent_branch = ChangeBuilder::new(agent_id.clone(), agent_key)
            .intent("agent work")
            .dep(root_id)
            .file(empty_file_change("a"))
            .build()
            .unwrap();
        let a_change_id = repo.commit(agent_branch).unwrap();

        let common = repo
            .index()
            .common_ancestors(&h_change_id.0, &a_change_id.0);
        assert_eq!(common.len(), 1);
        assert!(common.contains(&root_id.0));
    }
}
