//! One-way export from Mosaic to a Git repository.
//!
//! Pairs with `import_git` to close the migration loop: a team can
//! import their existing Git history into Mosaic, work in Mosaic for a
//! while, and periodically push back to Git so collaborators who
//! haven't switched still see updates.
//!
//! Strategy: walk the branch's changes in topological order. For each
//! Mosaic Change, create a Git commit whose:
//!   - parent(s) are the previously-mapped Git commit(s)
//!   - tree is reconstructed by replaying every reachable change's
//!     `body` (latest-content-per-path wins)
//!   - author = the Mosaic change's Identity.display()
//!   - committer = the local importer
//!   - message = the Mosaic change's intent + a "[mosaic <hex>]" trailer
//!
//! Uses subprocess `git` for portability (no libgit2 binding needed).
//! Requires a `git` binary on PATH and a target dir that is either an
//! empty directory (we `git init` it) or already a Git repo.

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::m1::change::ChangeId;
use crate::repo::Repository;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct ExportReport {
    pub commits_written: Vec<(Hash, String)>, // (mosaic id, git sha)
    pub branch: String,
    pub head_git_sha: Option<String>,
}

pub fn export(
    repo: &Repository,
    branch: &str,
    target_dir: impl AsRef<Path>,
) -> Result<ExportReport> {
    let target = target_dir.as_ref().to_path_buf();
    ensure_git_repo(&target)?;
    configure_git(&target)?;

    let frontier = match repo.refs().get(branch) {
        Ok(f) => f,
        Err(Error::RefNotFound(_)) => {
            return Ok(ExportReport {
                commits_written: vec![],
                branch: branch.into(),
                head_git_sha: None,
            })
        }
        Err(e) => return Err(e),
    };

    // Collect every ancestor of the branch frontier.
    let mut ancestors: BTreeSet<Hash> = BTreeSet::new();
    let mut queue: std::collections::VecDeque<Hash> =
        frontier.0.iter().copied().collect();
    while let Some(h) = queue.pop_front() {
        if !ancestors.insert(h) {
            continue;
        }
        if let Some(ps) = repo.index().parents_of(&h) {
            for p in ps {
                queue.push_back(*p);
            }
        }
    }
    let ordered = repo.topo_order(&ancestors);

    let mut mosaic_to_git: BTreeMap<Hash, String> = BTreeMap::new();
    let mut content: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut head_git_sha: Option<String> = None;

    for h in &ordered {
        let change = repo.load_change(&ChangeId(*h))?;

        // Replay this change's files into the working content map.
        for file in &change.body {
            content.insert(file.path.clone(), file.patch.clone());
        }

        // Materialize the working content into the target git working tree
        // and stage every path with git add.
        materialize_and_stage(&target, &content)?;

        // Build the commit. Parents are previously-mapped commits.
        let parent_args: Vec<String> = change
            .deps
            .iter()
            .filter_map(|d| mosaic_to_git.get(&d.0).cloned())
            .collect();

        let message = match &change.intent {
            Some(s) => format!("{s}\n\n[mosaic {}]\n", &h.to_hex()[..16]),
            None => format!("[mosaic {}]\n", &h.to_hex()[..16]),
        };

        let mut author = change.author.display();
        if !author.contains('<') {
            author = format!("{author} <unknown@mosaic.local>");
        }

        let sha = git_commit(&target, &parent_args, &message, &author)?;
        mosaic_to_git.insert(*h, sha.clone());
        head_git_sha = Some(sha);
    }

    // Update the target branch ref.
    if let Some(ref sha) = head_git_sha {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&target)
            .args(["update-ref", &format!("refs/heads/{branch}"), sha])
            .status();
    }

    let commits_written = ordered
        .iter()
        .filter_map(|h| mosaic_to_git.get(h).map(|sha| (*h, sha.clone())))
        .collect();

    Ok(ExportReport {
        commits_written,
        branch: branch.into(),
        head_git_sha,
    })
}

fn ensure_git_repo(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(Error::Io)?;
    if dir.join(".git").exists() {
        return Ok(());
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["init", "-q", "-b", "_export_tmp"])
        .status()
        .map_err(Error::Io)?;
    if !status.success() {
        return Err(Error::Serialization("git init failed".into()));
    }
    Ok(())
}

fn configure_git(dir: &Path) -> Result<()> {
    for (k, v) in [
        ("user.email", "exporter@mosaic.local"),
        ("user.name", "Mosaic Exporter"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
        ("gpg.format", "openpgp"),
    ] {
        let _ = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["config", k, v])
            .status();
    }
    Ok(())
}

fn materialize_and_stage(target: &Path, content: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    // First, clear the git index — we'll re-stage from scratch each commit.
    let _ = Command::new("git")
        .arg("-C")
        .arg(target)
        .args(["read-tree", "--empty"])
        .status();

    for (path, bytes) in content {
        let abs = target.join(path);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        std::fs::write(&abs, bytes).map_err(Error::Io)?;
        let status = Command::new("git")
            .arg("-C")
            .arg(target)
            .args(["add", "--"])
            .arg(path)
            .status()
            .map_err(Error::Io)?;
        if !status.success() {
            return Err(Error::Serialization(format!("git add {path} failed")));
        }
    }
    Ok(())
}

fn git_commit(
    target: &Path,
    parents: &[String],
    message: &str,
    author: &str,
) -> Result<String> {
    let tree_out = Command::new("git")
        .arg("-C")
        .arg(target)
        .args(["write-tree"])
        .output()
        .map_err(Error::Io)?;
    if !tree_out.status.success() {
        return Err(Error::Serialization(format!(
            "git write-tree failed: {}",
            String::from_utf8_lossy(&tree_out.stderr)
        )));
    }
    let tree = String::from_utf8_lossy(&tree_out.stdout).trim().to_string();

    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(target)
        .args(["-c", "commit.gpgsign=false"])
        .args(["commit-tree", &tree]);
    for p in parents {
        cmd.args(["-p", p]);
    }
    cmd.env("GIT_AUTHOR_NAME", author_name(author));
    cmd.env("GIT_AUTHOR_EMAIL", author_email(author));
    cmd.env("GIT_COMMITTER_NAME", "Mosaic Exporter");
    cmd.env("GIT_COMMITTER_EMAIL", "exporter@mosaic.local");
    cmd.arg("-m").arg(message);

    let out = cmd.output().map_err(Error::Io)?;
    if !out.status.success() {
        return Err(Error::Serialization(format!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn author_name(display: &str) -> String {
    match display.split('<').next() {
        Some(name) => name.trim().to_string(),
        None => display.to_string(),
    }
}

fn author_email(display: &str) -> String {
    if let Some(start) = display.find('<') {
        if let Some(end) = display.find('>') {
            if end > start + 1 {
                return display[start + 1..end].to_string();
            }
        }
    }
    "unknown@mosaic.local".into()
}

pub fn looks_like_git_repo(p: &Path) -> bool {
    p.join(".git").is_dir() || p.join("HEAD").is_file()
}

pub fn target_dir(p: impl Into<PathBuf>) -> PathBuf {
    p.into()
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
            Identity::human("alice@example.com", Some("Alice".into())).unwrap(),
            SigningKey::generate(),
        )
    }

    #[test]
    fn export_chain_creates_git_commits() {
        let dir = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (idn, key) = human();

        let a = ChangeBuilder::new(idn.clone(), key.clone())
            .intent("first")
            .file(FileChange {
                path: "hello.txt".into(),
                kind: FileKind::Text,
                patch: b"hello\n".to_vec(),
                conflicts: Vec::new(),
            })
            .build()
            .unwrap();
        let a_id = repo.commit(a).unwrap();

        let b = ChangeBuilder::new(idn, key)
            .intent("second")
            .dep(a_id)
            .file(FileChange {
                path: "hello.txt".into(),
                kind: FileKind::Text,
                patch: b"hello world\n".to_vec(),
                conflicts: Vec::new(),
            })
            .build()
            .unwrap();
        let b_id = repo.commit(b).unwrap();
        repo.advance_branch("main", b_id).unwrap();

        let report = export(&repo, "main", target.path()).unwrap();
        assert_eq!(report.commits_written.len(), 2);
        assert!(report.head_git_sha.is_some());

        // Confirm git can see the resulting branch.
        let log = Command::new("git")
            .arg("-C")
            .arg(target.path())
            .args(["log", "--oneline", "main"])
            .output()
            .unwrap();
        assert!(log.status.success(), "git log on exported repo failed");
        let log_str = String::from_utf8_lossy(&log.stdout);
        assert!(log_str.contains("first"), "git log missing 'first': {log_str}");
        assert!(log_str.contains("second"), "git log missing 'second'");

        // Confirm the working tree contains the latest content.
        let final_content =
            std::fs::read_to_string(target.path().join("hello.txt")).unwrap();
        assert_eq!(final_content, "hello world\n");

        // Confirm the [mosaic <sha>] trailer is in the message.
        let show = Command::new("git")
            .arg("-C")
            .arg(target.path())
            .args(["log", "-1", "--format=%B", "main"])
            .output()
            .unwrap();
        let msg = String::from_utf8_lossy(&show.stdout);
        assert!(msg.contains("[mosaic"));
        assert!(msg.contains(&b_id.to_hex()[..16]));
    }

    #[test]
    fn export_empty_branch_is_noop() {
        let dir = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let repo = Repository::open(dir.path()).unwrap();
        let report = export(&repo, "main", target.path()).unwrap();
        assert!(report.commits_written.is_empty());
        assert!(report.head_git_sha.is_none());
    }

    #[test]
    fn author_email_parsing() {
        assert_eq!(
            author_email("Alice <alice@example.com>"),
            "alice@example.com"
        );
        assert_eq!(author_email("anonymous"), "unknown@mosaic.local");
    }
}
