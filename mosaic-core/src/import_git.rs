//! Git read-only import bridge.
//!
//! Walks an existing Git repository in topological-oldest-first order and
//! emits one Mosaic `Change` per Git commit. Original commit metadata
//! (author, subject, sha) is preserved in the change's `intent` string;
//! signing is performed by the local importing identity since the original
//! Git signing keys (if any) are unrecoverable.

use crate::error::{Error, Result};
use crate::m1::change::{ChangeBuilder, ChangeId, FileChange, FileKind};
use crate::m1::identity::Identity;
use crate::m1::signing::SigningKey;
use crate::repo::Repository;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct ImportReport {
    pub imported: Vec<(String, ChangeId)>,
    pub skipped: Vec<String>,
}

pub fn import_git(
    repo: &mut Repository,
    git_dir: &Path,
    importer: &Identity,
    signing_key: &SigningKey,
) -> Result<ImportReport> {
    let shas = git_rev_list(git_dir)?;
    let mut mapping: BTreeMap<String, ChangeId> = BTreeMap::new();
    let mut imported = Vec::new();
    let mut skipped = Vec::new();

    for sha in shas {
        if repo_already_has(repo, &sha)? {
            skipped.push(sha);
            continue;
        }
        let commit = git_commit_info(git_dir, &sha)?;
        let parents: Vec<ChangeId> = commit
            .parents
            .iter()
            .filter_map(|p| mapping.get(p).copied())
            .collect();
        let files = git_files_at_commit(git_dir, &sha)?;

        let intent = format!(
            "[git {}] {} (by {} <{}>)",
            &sha[..12.min(sha.len())],
            commit.subject,
            commit.author_name,
            commit.author_email,
        );

        let mut builder = ChangeBuilder::new(importer.clone(), signing_key.clone()).intent(intent);
        for p in &parents {
            builder = builder.dep(*p);
        }
        for file in files {
            builder = builder.file(file);
        }
        let change = builder.build()?;
        let id = repo.commit(change)?;
        mapping.insert(sha.clone(), id);
        imported.push((sha, id));
    }

    Ok(ImportReport { imported, skipped })
}

fn repo_already_has(_repo: &Repository, _sha: &str) -> Result<bool> {
    // We don't yet track origin shas in Mosaic changes; re-import would
    // simply add new (deterministically different) Mosaic changes. Future
    // work: stash sha->ChangeId in a side table to make import idempotent.
    Ok(false)
}

struct CommitInfo {
    subject: String,
    author_name: String,
    author_email: String,
    parents: Vec<String>,
}

fn git_rev_list(git_dir: &Path) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .args(["rev-list", "--all", "--topo-order", "--reverse"])
        .output()
        .map_err(Error::Io)?;
    if !out.status.success() {
        return Err(Error::Serialization(format!(
            "git rev-list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .collect())
}

fn git_commit_info(git_dir: &Path, sha: &str) -> Result<CommitInfo> {
    let separator = "\x1fMOSAIC\x1f";
    let format = format!("%an{sep}%ae{sep}%P{sep}%s", sep = separator);
    let out = Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .args([
            "show",
            "--no-patch",
            &format!("--pretty=format:{format}"),
            sha,
        ])
        .output()
        .map_err(Error::Io)?;
    if !out.status.success() {
        return Err(Error::Serialization(format!(
            "git show failed for {sha}: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let line = raw.lines().next().unwrap_or("");
    let parts: Vec<&str> = line.split(separator).collect();
    if parts.len() != 4 {
        return Err(Error::Serialization(format!(
            "unexpected git show output for {sha}: {line:?}"
        )));
    }
    let parents = if parts[2].trim().is_empty() {
        Vec::new()
    } else {
        parts[2].split_whitespace().map(str::to_string).collect()
    };
    Ok(CommitInfo {
        author_name: parts[0].to_string(),
        author_email: parts[1].to_string(),
        parents,
        subject: parts[3].to_string(),
    })
}

fn git_files_at_commit(git_dir: &Path, sha: &str) -> Result<Vec<FileChange>> {
    let parents_output = Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .args(["show", "--no-patch", "--pretty=format:%P", sha])
        .output()
        .map_err(Error::Io)?;
    let parents_line = String::from_utf8_lossy(&parents_output.stdout);
    let first_parent = parents_line
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .next();

    let diff_out = match first_parent {
        Some(p) => Command::new("git")
            .arg("-C")
            .arg(git_dir)
            .args(["diff-tree", "-r", "--name-only", p, sha])
            .output()
            .map_err(Error::Io)?,
        None => Command::new("git")
            .arg("-C")
            .arg(git_dir)
            .args(["ls-tree", "-r", "--name-only", sha])
            .output()
            .map_err(Error::Io)?,
    };
    if !diff_out.status.success() {
        return Err(Error::Serialization(format!(
            "git diff-tree failed for {sha}: {}",
            String::from_utf8_lossy(&diff_out.stderr)
        )));
    }

    let mut files = Vec::new();
    for path in String::from_utf8_lossy(&diff_out.stdout).lines() {
        if path.is_empty() {
            continue;
        }
        let contents = git_show_blob(git_dir, sha, path)?;
        let kind = if std::str::from_utf8(&contents).is_ok() {
            FileKind::Text
        } else {
            FileKind::Binary
        };
        files.push(FileChange {
            path: path.to_string(),
            kind,
            patch: contents,
            conflicts: Vec::new(),
        });
    }
    Ok(files)
}

fn git_show_blob(git_dir: &Path, sha: &str, path: &str) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .arg("show")
        .arg(format!("{sha}:{path}"))
        .output()
        .map_err(Error::Io)?;
    if !out.status.success() {
        // File deleted at this commit, etc. — treat as empty.
        return Ok(Vec::new());
    }
    Ok(out.stdout)
}

pub fn looks_like_git_repo(path: &Path) -> bool {
    path.join(".git").is_dir() || path.join("HEAD").is_file()
}

pub fn canonical_git_dir(path: &Path) -> PathBuf {
    if path.join(".git").is_dir() {
        path.to_path_buf()
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use crate::m1::identity::Identity;
    use crate::m1::signing::SigningKey;
    use std::fs;
    use std::process::Command as Cmd;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let out = Cmd::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn make_git_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "alice@example.com"]);
        git(dir.path(), &["config", "user.name", "Alice"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        git(dir.path(), &["config", "tag.gpgsign", "false"]);
        git(dir.path(), &["config", "gpg.format", "openpgp"]);
        git(dir.path(), &["-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "(root)"]);
        dir
    }

    fn git_commit(dir: &Path, message: &str) {
        git(dir, &["-c", "commit.gpgsign=false", "commit", "-q", "-m", message]);
    }

    fn write(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn import_empty_repo_after_one_commit() {
        let src = make_git_repo();
        write(src.path(), "hello.txt", "hello world\n");
        git(src.path(), &["add", "hello.txt"]);
        git_commit(src.path(), "add greeting");

        let dst_dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dst_dir.path()).unwrap();
        let importer = Identity::human("bot@mosaic", Some("Importer".into())).unwrap();
        let key = SigningKey::generate();
        let report = import_git(&mut repo, src.path(), &importer, &key).unwrap();

        assert_eq!(report.imported.len(), 2);
        let (sha, mosaic_id) = &report.imported[1];
        let change = repo.load_change(mosaic_id).unwrap();
        assert!(change.intent.unwrap().contains(&sha[..12]));
        assert_eq!(change.body.len(), 1);
        assert_eq!(change.body[0].path, "hello.txt");
        assert_eq!(change.body[0].patch, b"hello world\n");
    }

    #[test]
    fn import_preserves_parent_chain() {
        let src = make_git_repo();
        write(src.path(), "a.txt", "1");
        git(src.path(), &["add", "a.txt"]);
        git_commit(src.path(), "a");
        write(src.path(), "a.txt", "2");
        git(src.path(), &["add", "a.txt"]);
        git_commit(src.path(), "b");

        let dst_dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dst_dir.path()).unwrap();
        let importer = Identity::human("bot@mosaic", None).unwrap();
        let key = SigningKey::generate();
        let report = import_git(&mut repo, src.path(), &importer, &key).unwrap();

        assert_eq!(report.imported.len(), 3);
        let last = repo.load_change(&report.imported.last().unwrap().1).unwrap();
        assert_eq!(last.deps.len(), 1);

        let chain: Vec<Hash> = repo
            .topo_order(&repo.all_change_ids().unwrap())
            .into_iter()
            .collect();
        assert_eq!(chain.len(), 3);
    }

    #[test]
    fn import_carries_author_metadata_in_intent() {
        let src = make_git_repo();
        write(src.path(), "x", "x");
        git(src.path(), &["add", "x"]);
        git_commit(src.path(), "topic message");

        let dst_dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dst_dir.path()).unwrap();
        let importer = Identity::human("bot@mosaic", None).unwrap();
        let key = SigningKey::generate();
        let report = import_git(&mut repo, src.path(), &importer, &key).unwrap();

        let last = repo.load_change(&report.imported.last().unwrap().1).unwrap();
        let intent = last.intent.unwrap();
        assert!(intent.contains("topic message"));
        assert!(intent.contains("alice@example.com"));
    }
}
