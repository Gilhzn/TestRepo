//! Binary search over the change DAG for the first bad change.
//!
//! Given a known-`good` change and a known-`bad` change (with `good` an
//! ancestor of `bad`), [`Bisect`] builds the candidate set of every change
//! that is an ancestor of `bad` (inclusive) but not an ancestor of `good`
//! (exclusive), in topological order. It then walks that ordered vector with
//! a standard `lo`/`hi` bisection: [`next_candidate`](Bisect::next_candidate)
//! hands back the midpoint to test, [`record`](Bisect::record) narrows the
//! bounds based on the verdict, and [`culprit`](Bisect::culprit) reports the
//! first bad change once the range collapses.
//!
//! The candidate vector is treated as a linear oldest→newest sequence. On a
//! true DAG this is an approximation (topological order is one valid
//! linearization), but it mirrors how `git bisect` reduces a history to a
//! sequence for the purpose of the search.

use crate::error::Result;
use crate::hash::Hash;
use crate::m1::change::ChangeId;
use crate::repo::Repository;
use std::collections::BTreeSet;

pub struct Bisect {
    /// Topologically ordered good..bad range (oldest → newest).
    candidates: Vec<Hash>,
    /// Inclusive lower bound: smallest index still possibly the culprit.
    lo: usize,
    /// Inclusive upper bound: largest index known/assumed bad.
    hi: usize,
}

impl Bisect {
    /// Build the candidate set: all changes that are ancestors of `bad`
    /// (inclusive) but NOT ancestors of `good` (exclusive), in topological
    /// order.
    pub fn new(repo: &Repository, good: ChangeId, bad: ChangeId) -> Result<Self> {
        let index = repo.index();

        // Ancestors of bad, inclusive.
        let mut bad_set: BTreeSet<Hash> = index.ancestors_of(&bad.0);
        bad_set.insert(bad.0);

        // Ancestors of good, exclusive (good itself is known-good, so drop it
        // and everything before it).
        let mut good_set: BTreeSet<Hash> = index.ancestors_of(&good.0);
        good_set.insert(good.0);

        let candidate_set: BTreeSet<Hash> =
            bad_set.difference(&good_set).copied().collect();

        let candidates = index.topological_order(&candidate_set);
        let hi = candidates.len().saturating_sub(1);
        Ok(Self {
            candidates,
            lo: 0,
            hi,
        })
    }

    /// How many candidates are still in the live `lo..=hi` window.
    pub fn remaining(&self) -> usize {
        if self.candidates.is_empty() {
            return 0;
        }
        self.hi - self.lo + 1
    }

    /// The midpoint candidate to test next, or `None` when the search has
    /// converged (range of one) or there is nothing to search.
    pub fn next_candidate(&self) -> Option<Hash> {
        if self.candidates.is_empty() {
            return None;
        }
        if self.hi <= self.lo {
            return None;
        }
        let mid = (self.lo + self.hi) / 2;
        Some(self.candidates[mid])
    }

    /// Record a verdict for a tested change and narrow the range.
    ///
    /// `is_bad == true` means the regression is present at/after `tested`, so
    /// the culprit is at `tested` or earlier (`hi` moves down to `tested`).
    /// `is_bad == false` means `tested` is good, so the culprit is strictly
    /// after it (`lo` moves up past `tested`).
    pub fn record(&mut self, tested: Hash, is_bad: bool) {
        let idx = match self.candidates.iter().position(|h| *h == tested) {
            Some(i) => i,
            None => return,
        };
        if is_bad {
            // tested and everything after it is suspect; culprit is <= idx.
            if idx < self.hi {
                self.hi = idx;
            }
        } else {
            // tested is good; culprit is strictly after idx.
            let next = idx + 1;
            if next > self.lo {
                self.lo = next.min(self.hi);
            }
        }
    }

    /// Once the range collapses (`hi - lo <= 1`), the first bad change.
    ///
    /// Empty candidate set → `None`. A single candidate is the culprit.
    pub fn culprit(&self) -> Option<Hash> {
        if self.candidates.is_empty() {
            return None;
        }
        if self.hi.saturating_sub(self.lo) <= 1 {
            return Some(self.candidates[self.lo]);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m1::change::{ChangeBuilder, FileChange, FileKind};
    use crate::m1::identity::Identity;
    use crate::m1::signing::SigningKey;
    use crate::repo::Repository;
    use tempfile::TempDir;

    fn human() -> (Identity, SigningKey) {
        (
            Identity::human("dev@example.com", Some("Dev".into())).unwrap(),
            SigningKey::generate(),
        )
    }

    fn empty_file_change(path: &str) -> FileChange {
        FileChange {
            path: path.into(),
            kind: FileKind::Text,
            patch: Vec::new(),
            conflicts: Vec::new(),
        }
    }

    /// Build a linear chain c0 -> c1 -> ... -> c{n-1}, returning the ids.
    fn linear_chain(repo: &mut Repository, n: usize) -> Vec<ChangeId> {
        let (identity, key) = human();
        let mut ids = Vec::with_capacity(n);
        let mut prev: Option<ChangeId> = None;
        for i in 0..n {
            let mut builder = ChangeBuilder::new(identity.clone(), key.clone())
                .intent(format!("c{i}"))
                .file(empty_file_change(&format!("f{i}")));
            if let Some(p) = prev {
                builder = builder.dep(p);
            }
            let change = builder.build().unwrap();
            let id = repo.commit(change).unwrap();
            prev = Some(id);
            ids.push(id);
        }
        ids
    }

    #[test]
    fn range_excludes_goods_ancestors() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let ids = linear_chain(&mut repo, 8);

        // good = c2, bad = c7. Candidate range should be c3..=c7 (5 changes):
        // ancestors of bad inclusive minus ancestors of good inclusive.
        let bisect = Bisect::new(&repo, ids[2], ids[7]).unwrap();
        assert_eq!(bisect.candidates.len(), 5);
        assert_eq!(bisect.candidates.first(), Some(&ids[3].0));
        assert_eq!(bisect.candidates.last(), Some(&ids[7].0));
        // good (c2) and its ancestors must be absent.
        assert!(!bisect.candidates.contains(&ids[2].0));
        assert!(!bisect.candidates.contains(&ids[0].0));
        assert_eq!(bisect.remaining(), 5);
    }

    #[test]
    fn next_candidate_is_midpoint() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let ids = linear_chain(&mut repo, 8);

        // good = c0, bad = c7 -> candidates c1..=c7 (indices 0..=6).
        let bisect = Bisect::new(&repo, ids[0], ids[7]).unwrap();
        assert_eq!(bisect.candidates.len(), 7);
        // lo=0, hi=6 -> mid=3 -> candidates[3] which is c4.
        assert_eq!(bisect.next_candidate(), Some(bisect.candidates[3]));
        assert_eq!(bisect.next_candidate(), Some(ids[4].0));
    }

    #[test]
    fn full_search_converges_to_known_culprit() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let ids = linear_chain(&mut repo, 8);

        // c5 is the first bad change: c5,c6,c7 bad; c1..c4 good.
        let culprit_id = ids[5];
        let bad_from: BTreeSet<Hash> =
            [ids[5].0, ids[6].0, ids[7].0].into_iter().collect();

        let mut bisect = Bisect::new(&repo, ids[0], ids[7]).unwrap();
        while let Some(candidate) = bisect.next_candidate() {
            let is_bad = bad_from.contains(&candidate);
            bisect.record(candidate, is_bad);
        }
        assert_eq!(bisect.culprit(), Some(culprit_id.0));
    }

    #[test]
    fn single_candidate_is_culprit() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let ids = linear_chain(&mut repo, 8);

        // good = c4, bad = c5 -> exactly one candidate: c5.
        let bisect = Bisect::new(&repo, ids[4], ids[5]).unwrap();
        assert_eq!(bisect.candidates.len(), 1);
        assert_eq!(bisect.next_candidate(), None);
        assert_eq!(bisect.culprit(), Some(ids[5].0));
        assert_eq!(bisect.remaining(), 1);
    }

    #[test]
    fn record_bad_and_good_narrow_correctly() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let ids = linear_chain(&mut repo, 8);

        // candidates c1..=c7 (7 entries, indices 0..=6).
        let mut bisect = Bisect::new(&repo, ids[0], ids[7]).unwrap();
        assert_eq!(bisect.remaining(), 7);

        // Mark midpoint (c4, index 3) bad -> hi collapses to 3.
        let mid = bisect.next_candidate().unwrap();
        assert_eq!(mid, ids[4].0);
        bisect.record(mid, true);
        assert_eq!(bisect.remaining(), 4); // lo=0, hi=3

        // New midpoint = index (0+3)/2 = 1 -> c2. Mark good -> lo=2.
        let mid = bisect.next_candidate().unwrap();
        assert_eq!(mid, ids[2].0);
        bisect.record(mid, false);
        assert_eq!(bisect.remaining(), 2); // lo=2, hi=3

        // Range of two -> midpoint index (2+3)/2 = 2 -> c3.
        let mid = bisect.next_candidate().unwrap();
        assert_eq!(mid, ids[3].0);
        bisect.record(mid, true); // c3 bad -> hi=2 == lo
        assert_eq!(bisect.culprit(), Some(ids[3].0));
    }

    #[test]
    fn empty_candidate_set_has_no_culprit() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let ids = linear_chain(&mut repo, 4);

        // good == bad -> nothing between -> empty range.
        let bisect = Bisect::new(&repo, ids[2], ids[2]).unwrap();
        assert_eq!(bisect.candidates.len(), 0);
        assert_eq!(bisect.remaining(), 0);
        assert_eq!(bisect.next_candidate(), None);
        assert_eq!(bisect.culprit(), None);
    }
}
