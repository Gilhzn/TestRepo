//! Per-session rollback.
//!
//! The AI-trust feature no other VCS has: "undo *everything* agent X did in
//! session Y." Given an agent session id, find every change authored under
//! that session and rewind a branch past them — dropping those changes and
//! anything built on top of them — without rewriting the immutable DAG.
//!
//! Changes are never deleted (they remain in the DAG, recoverable, prunable
//! by `gc`). We only move the branch ref to a frontier that excludes the
//! session's work.
//!
//! `keep` is ancestor-closed: a change is dropped iff it *is* in the session
//! or *descends from* a session change. So the new frontier is simply the
//! maximal kept changes.

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::m1::change::ChangeId;
use crate::m1::identity::Identity;
use crate::m1_dag::refs::Frontier;
use crate::repo::Repository;
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct RollbackReport {
    pub session_id: String,
    pub branch: String,
    pub dropped: Vec<Hash>,
    pub new_frontier: Frontier,
    pub previous_frontier: Frontier,
}

/// Every change whose author is an Agent with the given `session_id`.
pub fn changes_in_session(repo: &Repository, session_id: &str) -> Result<BTreeSet<Hash>> {
    let mut out = BTreeSet::new();
    for h in repo.all_change_ids()? {
        let change = repo.load_change(&ChangeId(h))?;
        if let Identity::Agent {
            session_id: sid, ..
        } = &change.author
        {
            if sid == session_id {
                out.insert(h);
            }
        }
    }
    Ok(out)
}

/// Compute the set of changes that must be dropped to remove a session from
/// a branch: reachable changes that are in the session OR descend from it.
pub fn dropped_set(
    repo: &Repository,
    branch_frontier: &Frontier,
    session: &BTreeSet<Hash>,
) -> BTreeSet<Hash> {
    // All changes reachable from the branch frontier.
    let mut reachable: BTreeSet<Hash> = BTreeSet::new();
    let mut queue: std::collections::VecDeque<Hash> =
        branch_frontier.0.iter().copied().collect();
    while let Some(h) = queue.pop_front() {
        if !reachable.insert(h) {
            continue;
        }
        if let Some(parents) = repo.index().parents_of(&h) {
            for p in parents {
                queue.push_back(*p);
            }
        }
    }

    let mut dropped = BTreeSet::new();
    for c in &reachable {
        if session.contains(c) {
            dropped.insert(*c);
            continue;
        }
        // Drop if any ancestor is in the session.
        let ancestors = repo.index().ancestors_of(c);
        if ancestors.iter().any(|a| session.contains(a)) {
            dropped.insert(*c);
        }
    }
    dropped
}

/// Roll a branch back past every change made in `session_id`. Returns the
/// new frontier (and writes it). The dropped changes remain in the DAG.
pub fn rollback_session(
    repo: &Repository,
    branch: &str,
    session_id: &str,
) -> Result<RollbackReport> {
    let previous_frontier = match repo.refs().get(branch) {
        Ok(f) => f,
        Err(Error::RefNotFound(_)) => Frontier::default(),
        Err(e) => return Err(e),
    };

    let session = changes_in_session(repo, session_id)?;
    let dropped = dropped_set(repo, &previous_frontier, &session);

    // reachable \ dropped, then take the maximal (no kept child) elements.
    let mut reachable: BTreeSet<Hash> = BTreeSet::new();
    let mut queue: std::collections::VecDeque<Hash> =
        previous_frontier.0.iter().copied().collect();
    while let Some(h) = queue.pop_front() {
        if !reachable.insert(h) {
            continue;
        }
        if let Some(parents) = repo.index().parents_of(&h) {
            for p in parents {
                queue.push_back(*p);
            }
        }
    }
    let keep: BTreeSet<Hash> = reachable.difference(&dropped).copied().collect();

    // New frontier = kept changes with no kept child.
    let mut new_tips: BTreeSet<Hash> = BTreeSet::new();
    for k in &keep {
        let has_kept_child = repo
            .index()
            .children_of(k)
            .map(|cs| cs.iter().any(|c| keep.contains(c)))
            .unwrap_or(false);
        if !has_kept_child {
            new_tips.insert(*k);
        }
    }

    let new_frontier = Frontier(new_tips);
    repo.refs().put(branch, &new_frontier)?;

    Ok(RollbackReport {
        session_id: session_id.to_string(),
        branch: branch.to_string(),
        dropped: dropped.into_iter().collect(),
        new_frontier,
        previous_frontier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m1::change::{ChangeBuilder, FileChange, FileKind};
    use crate::m1::signing::SigningKey;
    use tempfile::TempDir;

    fn human() -> (Identity, SigningKey) {
        (
            Identity::human("eyal@example.com", None).unwrap(),
            SigningKey::generate(),
        )
    }

    fn agent(session: &str) -> (Identity, SigningKey) {
        let invoker = Identity::human("eyal@example.com", None).unwrap();
        (
            Identity::agent("claude-code", session, invoker).unwrap(),
            SigningKey::generate(),
        )
    }

    fn commit(
        repo: &mut Repository,
        id: &Identity,
        key: &SigningKey,
        intent: &str,
        parent: Option<ChangeId>,
    ) -> ChangeId {
        let mut b = ChangeBuilder::new(id.clone(), key.clone()).intent(intent);
        if let Some(p) = parent {
            b = b.dep(p);
        }
        b = b.file(FileChange {
            path: format!("{intent}.txt"),
            kind: FileKind::Text,
            patch: intent.as_bytes().to_vec(),
            conflicts: Vec::new(),
        });
        let cid = repo.commit(b.build().unwrap()).unwrap();
        repo.advance_branch("main", cid).unwrap();
        cid
    }

    #[test]
    fn changes_in_session_filters_by_agent_session() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (h, hk) = human();
        let (a, ak) = agent("sess-1");

        let c1 = commit(&mut repo, &h, &hk, "human-1", None);
        let c2 = commit(&mut repo, &a, &ak, "agent-1", Some(c1));
        let _c3 = commit(&mut repo, &a, &ak, "agent-2", Some(c2));

        let session = changes_in_session(&repo, "sess-1").unwrap();
        assert_eq!(session.len(), 2);
        assert!(!session.contains(&c1.0));
    }

    #[test]
    fn rollback_drops_session_and_descendants() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (h, hk) = human();
        let (a, ak) = agent("sess-x");

        // human → agent → agent (the agent's two commits are at the tip)
        let c1 = commit(&mut repo, &h, &hk, "base", None);
        let c2 = commit(&mut repo, &a, &ak, "agent-work-1", Some(c1));
        let _c3 = commit(&mut repo, &a, &ak, "agent-work-2", Some(c2));

        let report = rollback_session(&repo, "main", "sess-x").unwrap();
        assert_eq!(report.dropped.len(), 2);

        // Branch should now point only at the human base commit.
        let frontier = repo.refs().get("main").unwrap();
        assert_eq!(frontier.0.len(), 1);
        assert!(frontier.0.contains(&c1.0));
    }

    #[test]
    fn rollback_keeps_human_work_built_before_session() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (h, hk) = human();
        let (a, ak) = agent("sess-y");

        let c1 = commit(&mut repo, &h, &hk, "h1", None);
        let c2 = commit(&mut repo, &h, &hk, "h2", Some(c1));
        let _agent_commit = commit(&mut repo, &a, &ak, "a1", Some(c2));

        let report = rollback_session(&repo, "main", "sess-y").unwrap();
        assert_eq!(report.dropped.len(), 1);
        let frontier = repo.refs().get("main").unwrap();
        assert!(frontier.0.contains(&c2.0));
    }

    #[test]
    fn rollback_unknown_session_is_noop() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (h, hk) = human();
        let c1 = commit(&mut repo, &h, &hk, "only", None);

        let report = rollback_session(&repo, "main", "nonexistent").unwrap();
        assert!(report.dropped.is_empty());
        let frontier = repo.refs().get("main").unwrap();
        assert!(frontier.0.contains(&c1.0));
    }

    #[test]
    fn human_work_on_top_of_session_is_also_dropped() {
        // If a human commits ON TOP of agent work, rolling back the agent
        // session must also drop the human commit that depends on it —
        // otherwise the history would be inconsistent.
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (h, hk) = human();
        let (a, ak) = agent("sess-z");

        let c1 = commit(&mut repo, &h, &hk, "base", None);
        let c2 = commit(&mut repo, &a, &ak, "agent", Some(c1));
        let _c3 = commit(&mut repo, &h, &hk, "human-on-agent", Some(c2));

        let report = rollback_session(&repo, "main", "sess-z").unwrap();
        // Both the agent commit and the human commit built on it are dropped.
        assert_eq!(report.dropped.len(), 2);
        let frontier = repo.refs().get("main").unwrap();
        assert_eq!(frontier.0.len(), 1);
        assert!(frontier.0.contains(&c1.0));
    }
}
