//! Branch refs as named frontiers (jj-style).
//!
//! A branch is not a chain of commits; it is a *set* of change ids — the
//! tips that no further change in the branch has as a parent. Two peers
//! holding the same set of changes see the same branch state regardless
//! of the order changes arrived.

use crate::error::{Error, Result};
use crate::hash::Hash;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frontier(pub BTreeSet<Hash>);

impl Frontier {
    pub fn new() -> Self {
        Self(BTreeSet::new())
    }

    pub fn from_iter<I: IntoIterator<Item = Hash>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }

    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(self.0.len() * 65);
        for h in &self.0 {
            out.push_str(&h.to_hex());
            out.push('\n');
        }
        out
    }

    pub fn decode(s: &str) -> Result<Self> {
        let mut set = BTreeSet::new();
        for line in s.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let h = Hash::from_hex(trimmed)
                .map_err(|_| Error::InvalidFrontierLine(trimmed.to_string()))?;
            set.insert(h);
        }
        Ok(Self(set))
    }
}

pub struct RefStore {
    root: PathBuf,
}

impl RefStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let root = path.into().join("refs");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn put(&self, name: &str, frontier: &Frontier) -> Result<()> {
        validate_name(name)?;
        let path = self.root.join(name);
        let tmp = self.root.join(format!(".{name}.tmp"));
        fs::write(&tmp, frontier.encode())?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<Frontier> {
        validate_name(name)?;
        let path = self.root.join(name);
        match fs::read_to_string(&path) {
            Ok(s) => Frontier::decode(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::RefNotFound(name.to_string()))
            }
            Err(e) => Err(Error::Io(e)),
        }
    }

    pub fn has(&self, name: &str) -> Result<bool> {
        validate_name(name)?;
        Ok(self.root.join(name).exists())
    }

    pub fn list(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let name = match file_name.to_str() {
                Some(s) => s,
                None => continue,
            };
            if name.starts_with('.') {
                continue;
            }
            if validate_name(name).is_err() {
                continue;
            }
            names.push(name.to_string());
        }
        names.sort();
        Ok(names)
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let path = self.root.join(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::RefNotFound(name.to_string()))
            }
            Err(e) => Err(Error::Io(e)),
        }
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidRefName("empty".into()));
    }
    if name.starts_with('.') {
        return Err(Error::InvalidRefName(name.into()));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(Error::InvalidRefName(name.into()));
    }
    for c in name.chars() {
        if !(c.is_ascii_graphic() || c == '-' || c == '_') {
            return Err(Error::InvalidRefName(name.into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn mk_hash(seed: u8) -> Hash {
        Hash::of(&[seed; 8])
    }

    #[test]
    fn frontier_round_trip() {
        let f = Frontier::from_iter([mk_hash(1), mk_hash(2), mk_hash(3)]);
        let encoded = f.encode();
        let decoded = Frontier::decode(&encoded).unwrap();
        assert_eq!(f, decoded);
    }

    #[test]
    fn frontier_decode_rejects_garbage() {
        assert!(matches!(
            Frontier::decode("not-a-hash\n"),
            Err(Error::InvalidFrontierLine(_))
        ));
    }

    #[test]
    fn put_then_get() {
        let dir = TempDir::new().unwrap();
        let store = RefStore::open(dir.path()).unwrap();
        let f = Frontier::from_iter([mk_hash(7)]);

        store.put("main", &f).unwrap();
        assert_eq!(store.get("main").unwrap(), f);
        assert!(store.has("main").unwrap());
    }

    #[test]
    fn list_returns_alphabetical() {
        let dir = TempDir::new().unwrap();
        let store = RefStore::open(dir.path()).unwrap();
        store.put("zebra", &Frontier::default()).unwrap();
        store.put("alpha", &Frontier::default()).unwrap();
        store.put("middle", &Frontier::default()).unwrap();

        assert_eq!(store.list().unwrap(), vec!["alpha", "middle", "zebra"]);
    }

    #[test]
    fn list_skips_tmp_files() {
        let dir = TempDir::new().unwrap();
        let store = RefStore::open(dir.path()).unwrap();
        store.put("main", &Frontier::default()).unwrap();
        fs::write(store.root().join(".main.tmp"), "junk").unwrap();

        assert_eq!(store.list().unwrap(), vec!["main"]);
    }

    #[test]
    fn delete_then_get_errors() {
        let dir = TempDir::new().unwrap();
        let store = RefStore::open(dir.path()).unwrap();
        store.put("feature", &Frontier::default()).unwrap();

        store.delete("feature").unwrap();
        assert!(matches!(
            store.get("feature"),
            Err(Error::RefNotFound(_))
        ));
        assert!(matches!(
            store.delete("feature"),
            Err(Error::RefNotFound(_))
        ));
    }

    #[test]
    fn invalid_names_rejected() {
        let dir = TempDir::new().unwrap();
        let store = RefStore::open(dir.path()).unwrap();
        let f = Frontier::default();

        for bad in &["", "..", "foo/bar", "foo\\bar", ".hidden", "name with space", "tab\there"] {
            assert!(
                store.put(bad, &f).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn put_is_atomic_via_tmp_then_rename() {
        let dir = TempDir::new().unwrap();
        let store = RefStore::open(dir.path()).unwrap();
        let f1 = Frontier::from_iter([mk_hash(1)]);
        let f2 = Frontier::from_iter([mk_hash(2)]);

        store.put("main", &f1).unwrap();
        store.put("main", &f2).unwrap();
        assert_eq!(store.get("main").unwrap(), f2);

        let tmp_visible = fs::read_dir(store.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!tmp_visible, "tmp files should not linger");
    }
}
