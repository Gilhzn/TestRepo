//! Append-only branch ref log.
//!
//! Every time a branch ref moves — a commit, a fast-forward advance, an undo,
//! a rollback, or a manual repoint — we append a [`RefLogEntry`] recording the
//! previous and new frontier. This makes a mistaken undo/abandon/rollback
//! recoverable: the old frontier hashes are right there in the log, so a tip
//! can always be re-pointed at where it used to be.
//!
//! On-disk layout: `<repo_root>/.mosaic/reflog.jsonl`. Append-only — never
//! rewritten. Each line is one JSON-encoded [`RefLogEntry`], in chronological
//! (append) order.
//!
//! Note: [`Repository::root`](crate::repo::Repository::root) returns the
//! `.mosaic` directory itself, so callers pass `repo.root().parent()` here.
//! [`RefLog::open`] joins `.mosaic/reflog.jsonl` onto the given path and
//! creates the `.mosaic` directory if needed.

use crate::error::{Error, Result};
use crate::m1::change::Tai64N;
use crate::m1_dag::refs::Frontier;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefLogEntry {
    pub ts: Tai64N,
    pub branch: String,
    /// Previous frontier as hex hashes.
    pub from: Vec<String>,
    /// New frontier as hex hashes.
    pub to: Vec<String>,
    /// What moved the ref: "commit" | "advance" | "undo" | "rollback" | "manual" ...
    pub op: String,
}

pub struct RefLog {
    root: PathBuf,
}

impl RefLog {
    /// Open (creating `.mosaic` if needed) a reflog under `repo_root`.
    ///
    /// The on-disk file is `<repo_root>/.mosaic/reflog.jsonl`.
    pub fn open(repo_root: impl AsRef<Path>) -> Result<Self> {
        let root = repo_root.as_ref().join(".mosaic");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path(&self) -> PathBuf {
        self.root.join("reflog.jsonl")
    }

    fn frontier_hex(frontier: &Frontier) -> Vec<String> {
        frontier.0.iter().map(|h| h.to_hex()).collect()
    }

    /// Append one ref movement to the log.
    pub fn record(
        &self,
        branch: &str,
        from: &Frontier,
        to: &Frontier,
        op: &str,
    ) -> Result<()> {
        let entry = RefLogEntry {
            ts: Tai64N::now(),
            branch: branch.to_string(),
            from: Self::frontier_hex(from),
            to: Self::frontier_hex(to),
            op: op.to_string(),
        };
        let line = serde_json::to_string(&entry)
            .map_err(|e| Error::Serialization(format!("reflog append: {e}")))?;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path())?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }

    /// All entries in chronological (append) order.
    pub fn entries(&self) -> Result<Vec<RefLogEntry>> {
        let path = self.path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = fs::File::open(&path)?;
        let reader = BufReader::new(file);
        let mut out = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: RefLogEntry = serde_json::from_str(&line)
                .map_err(|e| Error::Serialization(format!("reflog read: {e}")))?;
            out.push(entry);
        }
        Ok(out)
    }

    /// Entries for a single branch, in chronological order.
    pub fn for_branch(&self, branch: &str) -> Result<Vec<RefLogEntry>> {
        Ok(self
            .entries()?
            .into_iter()
            .filter(|e| e.branch == branch)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use tempfile::TempDir;

    fn mk_frontier(seeds: &[u8]) -> Frontier {
        Frontier::from_iter(seeds.iter().map(|s| Hash::of(&[*s; 8])))
    }

    #[test]
    fn record_then_read_back_in_order() {
        let dir = TempDir::new().unwrap();
        let log = RefLog::open(dir.path()).unwrap();

        let f0 = Frontier::default();
        let f1 = mk_frontier(&[1]);
        let f2 = mk_frontier(&[2]);

        log.record("main", &f0, &f1, "commit").unwrap();
        log.record("main", &f1, &f2, "advance").unwrap();

        let entries = log.entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].op, "commit");
        assert_eq!(entries[0].from, Vec::<String>::new());
        assert_eq!(entries[0].to, vec![Hash::of(&[1u8; 8]).to_hex()]);
        assert_eq!(entries[1].op, "advance");
        assert_eq!(entries[1].from, vec![Hash::of(&[1u8; 8]).to_hex()]);
        assert_eq!(entries[1].to, vec![Hash::of(&[2u8; 8]).to_hex()]);
    }

    #[test]
    fn for_branch_filters() {
        let dir = TempDir::new().unwrap();
        let log = RefLog::open(dir.path()).unwrap();
        let f = mk_frontier(&[9]);

        log.record("main", &Frontier::default(), &f, "commit").unwrap();
        log.record("feature", &Frontier::default(), &f, "commit").unwrap();
        log.record("main", &f, &mk_frontier(&[10]), "advance").unwrap();

        let main = log.for_branch("main").unwrap();
        assert_eq!(main.len(), 2);
        assert!(main.iter().all(|e| e.branch == "main"));

        let feature = log.for_branch("feature").unwrap();
        assert_eq!(feature.len(), 1);
        assert_eq!(feature[0].branch, "feature");

        let none = log.for_branch("nope").unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn multiple_ops_preserved() {
        let dir = TempDir::new().unwrap();
        let log = RefLog::open(dir.path()).unwrap();
        let f = mk_frontier(&[3]);

        for op in ["commit", "advance", "undo", "rollback", "manual"] {
            log.record("main", &f, &f, op).unwrap();
        }

        let ops: Vec<String> = log
            .entries()
            .unwrap()
            .into_iter()
            .map(|e| e.op)
            .collect();
        assert_eq!(ops, vec!["commit", "advance", "undo", "rollback", "manual"]);
    }

    #[test]
    fn empty_log_is_empty() {
        let dir = TempDir::new().unwrap();
        let log = RefLog::open(dir.path()).unwrap();
        assert!(log.entries().unwrap().is_empty());
        assert!(log.for_branch("main").unwrap().is_empty());
    }

    #[test]
    fn open_creates_mosaic_dir() {
        let dir = TempDir::new().unwrap();
        let _ = RefLog::open(dir.path()).unwrap();
        assert!(dir.path().join(".mosaic").exists());
    }

    #[test]
    fn persists_across_reopen() {
        let dir = TempDir::new().unwrap();
        {
            let log = RefLog::open(dir.path()).unwrap();
            log.record("main", &Frontier::default(), &mk_frontier(&[1]), "commit")
                .unwrap();
        }
        let log = RefLog::open(dir.path()).unwrap();
        assert_eq!(log.entries().unwrap().len(), 1);
    }
}
