//! Per-line blame for a path at a branch tip.
//!
//! "Line-introduction" blame: each line of the final file is attributed to the
//! change that most recently *introduced* that exact line. We replay the
//! branch's reachable changes in topological order (oldest -> newest),
//! carrying an attribution alongside every line, and diff each new version of
//! the file against the previously-attributed content. Lines that survive
//! unchanged keep their attribution; freshly inserted lines are attributed to
//! the change doing the insert.

use crate::error::{Error, Result};
use crate::m1::change::{ChangeId, Tai64N};
use crate::m1::identity::Identity;
use crate::repo::Repository;
use std::collections::{BTreeSet, VecDeque};

pub struct BlameLine {
    /// 1-based, in the final file.
    pub line_no: usize,
    pub content: String,
    /// The change that introduced this exact line.
    pub change: ChangeId,
    pub author: Identity,
    pub ts: Tai64N,
}

pub struct Blame {
    pub path: String,
    pub lines: Vec<BlameLine>,
}

/// One attributed line carried through the replay.
#[derive(Clone)]
struct Attributed {
    content: String,
    change: ChangeId,
    author: Identity,
    ts: Tai64N,
}

/// Attribute each line of `path` at the tip of `branch` to the change that
/// most recently introduced that line.
pub fn blame(repo: &Repository, branch: &str, path: &str) -> Result<Blame> {
    // 1. Collect the branch's reachable changes (BFS the frontier's
    //    ancestors), then put them in topological order oldest -> newest.
    let frontier = match repo.refs().get(branch) {
        Ok(f) => f,
        Err(Error::RefNotFound(_)) => {
            return Ok(Blame {
                path: path.to_string(),
                lines: Vec::new(),
            })
        }
        Err(e) => return Err(e),
    };

    let mut ancestors: BTreeSet<crate::Hash> = BTreeSet::new();
    let mut queue: VecDeque<crate::Hash> = frontier.0.iter().copied().collect();
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

    // 2. Replay, carrying attribution per line.
    let mut attributed: Vec<Attributed> = Vec::new();

    for h in ordered {
        let change = repo.load_change(&ChangeId(h))?;
        let cid = ChangeId(h);
        let author = change.author.clone();
        let ts = change.ts;

        // Find this change's contribution to `path`, if any.
        let Some(file) = change.body.iter().find(|f| f.path == path) else {
            continue;
        };

        let new_text = String::from_utf8_lossy(&file.patch).into_owned();
        let new_lines: Vec<&str> = split_lines(&new_text);

        // The slices we diff: previous attributed line strings vs new lines.
        let old_lines: Vec<&str> = attributed.iter().map(|a| a.content.as_str()).collect();

        use similar::{ChangeTag, TextDiff};
        let diff = TextDiff::from_slices(&old_lines, &new_lines);

        let mut rebuilt: Vec<Attributed> = Vec::with_capacity(new_lines.len());
        let mut old_idx = 0usize; // cursor into `attributed`
        for op in diff.iter_all_changes() {
            match op.tag() {
                ChangeTag::Equal => {
                    // Surviving line keeps its existing attribution.
                    rebuilt.push(attributed[old_idx].clone());
                    old_idx += 1;
                }
                ChangeTag::Delete => {
                    // Line drops out of the new file.
                    old_idx += 1;
                }
                ChangeTag::Insert => {
                    // Newly introduced line, attributed to THIS change.
                    rebuilt.push(Attributed {
                        content: op.value().to_string(),
                        change: cid,
                        author: author.clone(),
                        ts,
                    });
                }
            }
        }
        attributed = rebuilt;
    }

    let lines = attributed
        .into_iter()
        .enumerate()
        .map(|(i, a)| BlameLine {
            line_no: i + 1,
            content: a.content,
            change: a.change,
            author: a.author,
            ts: a.ts,
        })
        .collect();

    Ok(Blame {
        path: path.to_string(),
        lines,
    })
}

/// Split file content into logical lines, preserving content and dropping a
/// trailing empty line caused by a final newline (so "a\nb\n" -> ["a","b"]).
/// `similar` matches on these strings, so they must be stable across versions.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<&str> = text.split('\n').collect();
    // Trailing newline yields a spurious empty final element.
    if let Some(last) = out.last() {
        if last.is_empty() {
            out.pop();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m1::change::{ChangeBuilder, FileChange, FileKind};
    use crate::m1::identity::Identity;
    use crate::m1::signing::SigningKey;
    use tempfile::TempDir;

    fn human(email: &str, name: &str) -> (Identity, SigningKey) {
        (
            Identity::human(email, Some(name.to_string())).unwrap(),
            SigningKey::generate(),
        )
    }

    fn text_file(path: &str, bytes: &[u8]) -> FileChange {
        FileChange {
            path: path.to_string(),
            kind: FileKind::Text,
            patch: bytes.to_vec(),
            conflicts: Vec::new(),
        }
    }

    #[test]
    fn single_change_attributes_all_lines() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (idn, key) = human("a@example.com", "A");

        let id = repo
            .commit(
                ChangeBuilder::new(idn, key)
                    .intent("first")
                    .file(text_file("f", b"a\nb\nc\n"))
                    .build()
                    .unwrap(),
            )
            .unwrap();
        repo.advance_branch("main", id).unwrap();

        let b = blame(&repo, "main", "f").unwrap();
        assert_eq!(b.path, "f");
        assert_eq!(b.lines.len(), 3);
        for (i, line) in b.lines.iter().enumerate() {
            assert_eq!(line.line_no, i + 1);
            assert_eq!(line.change, id);
        }
        assert_eq!(b.lines[0].content, "a");
        assert_eq!(b.lines[2].content, "c");
    }

    #[test]
    fn append_keeps_old_attribution_for_old_lines() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (idn, key) = human("a@example.com", "A");

        let c1 = repo
            .commit(
                ChangeBuilder::new(idn.clone(), key.clone())
                    .intent("c1")
                    .file(text_file("f", b"a\nb\n"))
                    .build()
                    .unwrap(),
            )
            .unwrap();
        repo.advance_branch("main", c1).unwrap();

        let c2 = repo
            .commit(
                ChangeBuilder::new(idn, key)
                    .intent("c2")
                    .dep(c1)
                    .file(text_file("f", b"a\nb\nc\n"))
                    .build()
                    .unwrap(),
            )
            .unwrap();
        repo.advance_branch("main", c2).unwrap();

        let b = blame(&repo, "main", "f").unwrap();
        assert_eq!(b.lines.len(), 3);
        assert_eq!(b.lines[0].change, c1);
        assert_eq!(b.lines[1].change, c1);
        assert_eq!(b.lines[2].change, c2); // the appended line
        assert_eq!(b.lines[2].content, "c");
    }

    #[test]
    fn middle_modification_only_flips_that_line() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (idn, key) = human("a@example.com", "A");

        let c1 = repo
            .commit(
                ChangeBuilder::new(idn.clone(), key.clone())
                    .intent("c1")
                    .file(text_file("f", b"a\nb\nc\n"))
                    .build()
                    .unwrap(),
            )
            .unwrap();
        repo.advance_branch("main", c1).unwrap();

        let c2 = repo
            .commit(
                ChangeBuilder::new(idn, key)
                    .intent("c2")
                    .dep(c1)
                    .file(text_file("f", b"a\nB\nc\n"))
                    .build()
                    .unwrap(),
            )
            .unwrap();
        repo.advance_branch("main", c2).unwrap();

        let b = blame(&repo, "main", "f").unwrap();
        assert_eq!(b.lines.len(), 3);
        assert_eq!(b.lines[0].change, c1);
        assert_eq!(b.lines[1].change, c2); // only the middle line flips
        assert_eq!(b.lines[1].content, "B");
        assert_eq!(b.lines[2].change, c1);
    }

    #[test]
    fn author_attribution_uses_introducing_change() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (alice, akey) = human("alice@example.com", "Alice");
        let (bob, bkey) = human("bob@example.com", "Bob");

        let c1 = repo
            .commit(
                ChangeBuilder::new(alice.clone(), akey)
                    .intent("alice")
                    .file(text_file("f", b"a\nb\n"))
                    .build()
                    .unwrap(),
            )
            .unwrap();
        repo.advance_branch("main", c1).unwrap();

        let c2 = repo
            .commit(
                ChangeBuilder::new(bob.clone(), bkey)
                    .intent("bob")
                    .dep(c1)
                    .file(text_file("f", b"a\nb\nc\n"))
                    .build()
                    .unwrap(),
            )
            .unwrap();
        repo.advance_branch("main", c2).unwrap();

        let b = blame(&repo, "main", "f").unwrap();
        assert_eq!(b.lines.len(), 3);
        assert_eq!(b.lines[0].author, alice);
        assert_eq!(b.lines[1].author, alice);
        assert_eq!(b.lines[2].author, bob); // bob introduced the new line
    }

    #[test]
    fn missing_path_returns_empty() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (idn, key) = human("a@example.com", "A");

        let id = repo
            .commit(
                ChangeBuilder::new(idn, key)
                    .intent("first")
                    .file(text_file("f", b"a\nb\n"))
                    .build()
                    .unwrap(),
            )
            .unwrap();
        repo.advance_branch("main", id).unwrap();

        let b = blame(&repo, "main", "does-not-exist").unwrap();
        assert_eq!(b.path, "does-not-exist");
        assert!(b.lines.is_empty());
    }

    #[test]
    fn unknown_branch_returns_empty() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let b = blame(&repo, "no-such-branch", "f").unwrap();
        assert!(b.lines.is_empty());
    }
}
