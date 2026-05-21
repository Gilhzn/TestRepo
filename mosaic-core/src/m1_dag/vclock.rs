//! Vector clocks keyed by author identity.
//!
//! A vector clock tracks one monotone counter per author. Two clocks have a
//! causal ordering iff one dominates the other component-wise; otherwise the
//! changes they label are concurrent.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorClock(BTreeMap<String, u64>);

impl VectorClock {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn get(&self, author_id: &str) -> u64 {
        self.0.get(author_id).copied().unwrap_or(0)
    }

    pub fn increment(&mut self, author_id: impl Into<String>) {
        let entry = self.0.entry(author_id.into()).or_insert(0);
        *entry += 1;
    }

    pub fn merge(&mut self, other: &Self) {
        for (k, v) in &other.0 {
            let entry = self.0.entry(k.clone()).or_insert(0);
            if *v > *entry {
                *entry = *v;
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &u64)> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.values().all(|v| *v == 0)
    }

    pub fn happens_before(&self, other: &Self) -> bool {
        let mut strictly_less_somewhere = false;
        for (k, v) in &self.0 {
            let o = other.get(k);
            if *v > o {
                return false;
            }
            if *v < o {
                strictly_less_somewhere = true;
            }
        }
        for (k, v) in &other.0 {
            if !self.0.contains_key(k) && *v > 0 {
                strictly_less_somewhere = true;
            }
        }
        strictly_less_somewhere
    }

    pub fn concurrent_with(&self, other: &Self) -> bool {
        !self.happens_before(other) && !other.happens_before(self) && self != other
    }
}

impl PartialOrd for VectorClock {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if self == other {
            return Some(Ordering::Equal);
        }
        if self.happens_before(other) {
            return Some(Ordering::Less);
        }
        if other.happens_before(self) {
            return Some(Ordering::Greater);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_then_increment() {
        let mut vc = VectorClock::new();
        assert!(vc.is_empty());
        vc.increment("a");
        assert_eq!(vc.get("a"), 1);
        vc.increment("a");
        assert_eq!(vc.get("a"), 2);
        assert_eq!(vc.get("b"), 0);
    }

    #[test]
    fn merge_picks_max_per_author() {
        let mut lhs = VectorClock::new();
        lhs.increment("a");
        lhs.increment("a");
        lhs.increment("b");

        let mut rhs = VectorClock::new();
        rhs.increment("a");
        rhs.increment("b");
        rhs.increment("b");
        rhs.increment("c");

        lhs.merge(&rhs);
        assert_eq!(lhs.get("a"), 2);
        assert_eq!(lhs.get("b"), 2);
        assert_eq!(lhs.get("c"), 1);
    }

    #[test]
    fn causal_ordering_and_concurrency() {
        let mut a1 = VectorClock::new();
        a1.increment("a");
        let mut a2 = VectorClock::new();
        a2.increment("a");
        a2.increment("a");
        assert!(a1.happens_before(&a2));
        assert!(!a2.happens_before(&a1));
        assert!(!a1.concurrent_with(&a2));

        let mut left = VectorClock::new();
        left.increment("a");
        let mut right = VectorClock::new();
        right.increment("b");
        assert!(!left.happens_before(&right));
        assert!(!right.happens_before(&left));
        assert!(left.concurrent_with(&right));
    }

    #[test]
    fn partial_cmp_returns_none_for_concurrent() {
        let mut left = VectorClock::new();
        left.increment("a");
        let mut right = VectorClock::new();
        right.increment("b");
        assert_eq!(left.partial_cmp(&right), None);

        let mut a1 = VectorClock::new();
        a1.increment("a");
        let mut a2 = a1.clone();
        a2.increment("a");
        assert_eq!(a1.partial_cmp(&a2), Some(Ordering::Less));
        assert_eq!(a2.partial_cmp(&a1), Some(Ordering::Greater));
        assert_eq!(a1.partial_cmp(&a1.clone()), Some(Ordering::Equal));
    }

    #[test]
    fn serde_round_trip() {
        let mut vc = VectorClock::new();
        vc.increment("alice");
        vc.increment("bob");
        vc.increment("alice");
        let bytes = bincode::serialize(&vc).unwrap();
        let back: VectorClock = bincode::deserialize(&bytes).unwrap();
        assert_eq!(vc, back);
    }
}
