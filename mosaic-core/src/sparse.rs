//! Sparse profiles — work with only a subset of a large repo.
//!
//! In a monorepo you rarely want every file on disk. A sparse profile is a
//! set of include/exclude glob patterns; tools that materialize a working
//! tree (the CLI checkout, the FUSE mount, `mos status`) consult it to skip
//! paths the developer doesn't care about. The full history still lives in
//! the DAG — sparseness only affects what gets written to / shown in the
//! working tree.
//!
//! Stored at `<repo>/.mosaic/sparse.json`. An empty/missing profile means
//! "everything" (the common case), so this is zero-cost until opted in.
//!
//! Glob semantics: `*` matches within one path segment, `**` matches across
//! segments. A path is included iff it matches any `include` (or there are
//! no includes) AND matches no `exclude`.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SparseProfile {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl SparseProfile {
    pub fn load(repo_root: impl AsRef<Path>) -> Result<Self> {
        let path = repo_root.as_ref().join(".mosaic").join("sparse.json");
        match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s)
                .map_err(|e| Error::Serialization(format!("sparse.json: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    pub fn save(&self, repo_root: impl AsRef<Path>) -> Result<()> {
        let dir = repo_root.as_ref().join(".mosaic");
        fs::create_dir_all(&dir)?;
        let raw = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        fs::write(dir.join("sparse.json"), raw)?;
        Ok(())
    }

    /// True when this profile constrains anything (otherwise = "everything").
    pub fn is_active(&self) -> bool {
        !self.include.is_empty() || !self.exclude.is_empty()
    }

    /// Whether `path` should be materialized under this profile.
    pub fn includes(&self, path: &str) -> bool {
        if !self.exclude.is_empty() && self.exclude.iter().any(|p| glob_match(p, path)) {
            return false;
        }
        if self.include.is_empty() {
            return true;
        }
        self.include.iter().any(|p| glob_match(p, path))
    }

    /// Filter an iterator of paths down to those the profile includes.
    pub fn filter<'a, I: IntoIterator<Item = &'a String>>(
        &self,
        paths: I,
    ) -> Vec<String> {
        paths
            .into_iter()
            .filter(|p| self.includes(p))
            .cloned()
            .collect()
    }
}

/// `*` matches within one segment, `**` matches across segments.
pub fn glob_match(pattern: &str, target: &str) -> bool {
    glob_bytes(pattern.as_bytes(), target.as_bytes())
}

fn glob_bytes(pattern: &[u8], target: &[u8]) -> bool {
    if pattern.is_empty() {
        return target.is_empty();
    }
    if pattern.starts_with(b"**") {
        let rest = &pattern[2..];
        for i in 0..=target.len() {
            if glob_bytes(rest, &target[i..]) {
                return true;
            }
        }
        return false;
    }
    if pattern[0] == b'*' {
        let rest = &pattern[1..];
        for i in 0..=target.len() {
            if target[..i].contains(&b'/') {
                break;
            }
            if glob_bytes(rest, &target[i..]) {
                return true;
            }
        }
        return false;
    }
    if target.is_empty() {
        return false;
    }
    if pattern[0] == target[0] {
        return glob_bytes(&pattern[1..], &target[1..]);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn empty_profile_includes_everything() {
        let p = SparseProfile::default();
        assert!(!p.is_active());
        assert!(p.includes("any/path/at/all.rs"));
    }

    #[test]
    fn include_restricts_to_matching() {
        let p = SparseProfile {
            include: vec!["src/**".into()],
            exclude: vec![],
        };
        assert!(p.is_active());
        assert!(p.includes("src/main.rs"));
        assert!(p.includes("src/a/b/c.rs"));
        assert!(!p.includes("tests/it.rs"));
        assert!(!p.includes("README.md"));
    }

    #[test]
    fn exclude_overrides_include() {
        let p = SparseProfile {
            include: vec!["**".into()],
            exclude: vec!["**/generated/**".into(), "*.lock".into()],
        };
        assert!(p.includes("src/main.rs"));
        assert!(!p.includes("src/generated/proto.rs"));
        assert!(!p.includes("Cargo.lock"));
    }

    #[test]
    fn filter_keeps_only_included() {
        let p = SparseProfile {
            include: vec!["payments/**".into()],
            exclude: vec![],
        };
        let all: Vec<String> = vec![
            "payments/api.rs".into(),
            "payments/db.rs".into(),
            "billing/api.rs".into(),
        ];
        let kept = p.filter(all.iter());
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|p| p.starts_with("payments/")));
    }

    #[test]
    fn single_star_does_not_cross_slash() {
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/a/b.rs"));
    }

    #[test]
    fn round_trip_on_disk() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".mosaic")).unwrap();
        let p = SparseProfile {
            include: vec!["src/**".into(), "docs/**".into()],
            exclude: vec!["**/*.tmp".into()],
        };
        p.save(dir.path()).unwrap();
        let back = SparseProfile::load(dir.path()).unwrap();
        assert_eq!(back.include.len(), 2);
        assert_eq!(back.exclude.len(), 1);
        assert!(back.includes("src/main.rs"));
        assert!(!back.includes("src/x.tmp"));
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = TempDir::new().unwrap();
        let p = SparseProfile::load(dir.path()).unwrap();
        assert!(!p.is_active());
    }
}
