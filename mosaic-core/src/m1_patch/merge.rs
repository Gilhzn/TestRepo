//! Minimal merge engine: commutation check + structured three-way merge.
//!
//! The merge engine never produces inline `<<<<<<<` markers. Conflicts are
//! returned as data alongside a graph that always contains both sides' work,
//! so the repository can never wedge — even concurrent inserts at the same
//! position coexist as siblings in the graph for later resolution.

use crate::error::Result;
use crate::m1_patch::line_graph::{LineGraph, VertexId};
use crate::m1_patch::patch::{Op, Patch};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PatchSide {
    Ours,
    Theirs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StructuredConflict {
    ConcurrentInsert {
        anchor: VertexId,
        before: VertexId,
        ours: VertexId,
        theirs: VertexId,
    },
    EditVsDelete {
        target: VertexId,
        deleter: PatchSide,
        editor: PatchSide,
    },
}

#[derive(Clone, Debug)]
pub struct MergeResult {
    pub graph: LineGraph,
    pub conflicts: Vec<StructuredConflict>,
}

/// How to deterministically resolve the structured conflicts in a
/// `MergeResult` into a single concrete file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveStrategy {
    /// Keep the local side's contribution; drop the remote's on conflict.
    Ours,
    /// Keep the remote side's contribution; drop the local's on conflict.
    Theirs,
    /// Keep both (the default merged graph already does this).
    Union,
}

impl MergeResult {
    /// Apply a resolution strategy to every structured conflict, mutating
    /// the graph (killing the losing side's vertices / resurrecting an
    /// edited-then-deleted line for `Ours`/`Theirs`), and return the
    /// resolved lines.
    pub fn resolve(mut self, strategy: ResolveStrategy) -> Result<Vec<String>> {
        for c in &self.conflicts {
            match c {
                StructuredConflict::ConcurrentInsert { ours, theirs, .. } => match strategy {
                    ResolveStrategy::Ours => {
                        let _ = self.graph.set_alive(theirs, false);
                    }
                    ResolveStrategy::Theirs => {
                        let _ = self.graph.set_alive(ours, false);
                    }
                    ResolveStrategy::Union => {}
                },
                StructuredConflict::EditVsDelete { target, deleter, .. } => {
                    // Ours/Theirs decide whether the deletion or the edit wins.
                    let keep_alive = match (strategy, deleter) {
                        (ResolveStrategy::Ours, PatchSide::Theirs) => true, // ours edited
                        (ResolveStrategy::Theirs, PatchSide::Ours) => true,
                        (ResolveStrategy::Union, _) => true,
                        _ => false,
                    };
                    let _ = self.graph.set_alive(target, keep_alive);
                }
            }
        }
        Ok(self
            .graph
            .flatten()
            .into_iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect())
    }
}

/// Two patches commute iff their op sets touch disjoint sets of vertices
/// AND neither's inserts anchor on a vertex that the other inserts/kills.
///
/// Because insert ops introduce fresh vertices (content-addressed from the
/// creator change), the only overlap that matters is when one patch anchors
/// or targets a vertex the other introduces, or both patches mutate the
/// alive-flag of the same vertex.
pub fn commute(a: &Patch, b: &Patch) -> bool {
    let a_intro = a.introduced();
    let b_intro = b.introduced();
    let a_anchor = a.anchored_on();
    let b_anchor = b.anchored_on();

    // a anchors / kills on something b creates -> b must be applied first.
    if !a_anchor.is_disjoint(&b_intro) {
        return false;
    }
    if !b_anchor.is_disjoint(&a_intro) {
        return false;
    }

    // Both patches touch the alive-flag of the same vertex.
    let a_flips = flips(a);
    let b_flips = flips(b);
    if !a_flips.is_disjoint(&b_flips) {
        return false;
    }

    // Either side kills a vertex the other anchors on (edit-vs-delete:
    // ordering doesn't change the final graph, but it changes semantics).
    if !a_flips.is_disjoint(&b_anchor) {
        return false;
    }
    if !b_flips.is_disjoint(&a_anchor) {
        return false;
    }

    true
}

fn flips(p: &Patch) -> BTreeSet<VertexId> {
    let mut out = BTreeSet::new();
    for op in &p.ops {
        match op {
            Op::Kill { target } | Op::Resurrect { target } => {
                out.insert(*target);
            }
            _ => {}
        }
    }
    out
}

/// Three-way merge: produce a graph holding both sides' operations, plus a
/// list of structured conflicts. The graph is always well-formed and acyclic;
/// callers (or downstream resolvers) decide what to do with the conflicts.
pub fn three_way_merge(
    base: &LineGraph,
    ours: &Patch,
    theirs: &Patch,
) -> Result<MergeResult> {
    let mut graph = base.clone();
    ours.apply(&mut graph)?;
    theirs.apply(&mut graph)?;

    let mut conflicts = Vec::new();

    // --- Concurrent inserts at the same (anchor, before) -------------------
    let mut ours_by_slot: BTreeMap<(VertexId, VertexId), VertexId> = BTreeMap::new();
    for op in &ours.ops {
        if let Op::InsertAfter {
            anchor,
            before,
            vertex,
        } = op
        {
            ours_by_slot.insert((*anchor, *before), vertex.id);
        }
    }
    for op in &theirs.ops {
        if let Op::InsertAfter {
            anchor,
            before,
            vertex,
        } = op
        {
            if let Some(ours_v) = ours_by_slot.get(&(*anchor, *before)) {
                if *ours_v != vertex.id {
                    conflicts.push(StructuredConflict::ConcurrentInsert {
                        anchor: *anchor,
                        before: *before,
                        ours: *ours_v,
                        theirs: vertex.id,
                    });
                }
            }
        }
    }

    // --- Edit-vs-delete: one side kills v, the other inserts anchored on v -
    let ours_kills = kill_targets(ours);
    let theirs_kills = kill_targets(theirs);
    let ours_anchors = insert_anchors(ours);
    let theirs_anchors = insert_anchors(theirs);

    for v in ours_kills.intersection(&theirs_anchors) {
        conflicts.push(StructuredConflict::EditVsDelete {
            target: *v,
            deleter: PatchSide::Ours,
            editor: PatchSide::Theirs,
        });
    }
    for v in theirs_kills.intersection(&ours_anchors) {
        conflicts.push(StructuredConflict::EditVsDelete {
            target: *v,
            deleter: PatchSide::Theirs,
            editor: PatchSide::Ours,
        });
    }

    Ok(MergeResult { graph, conflicts })
}

fn kill_targets(p: &Patch) -> BTreeSet<VertexId> {
    let mut out = BTreeSet::new();
    for op in &p.ops {
        if let Op::Kill { target } = op {
            out.insert(*target);
        }
    }
    out
}

fn insert_anchors(p: &Patch) -> BTreeSet<VertexId> {
    let mut out = BTreeSet::new();
    for op in &p.ops {
        if let Op::InsertAfter { anchor, .. } = op {
            out.insert(*anchor);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use crate::m1_patch::line_graph::Vertex;

    fn creator(tag: &[u8]) -> Hash {
        Hash::of(tag)
    }

    fn vx(c: &Hash, idx: u64, bytes: &[u8]) -> Vertex {
        Vertex {
            id: VertexId::derive(c, idx, bytes),
            bytes: bytes.to_vec(),
            alive: true,
        }
    }

    /// Base graph `[a, b, c]` from a fixed creator, with vertex ids for a, b, c.
    fn base_abc() -> (Hash, LineGraph, VertexId, VertexId, VertexId) {
        let c = creator(b"base");
        let g = LineGraph::from_lines(&c, &[b"a", b"b", b"c"]);
        let a = VertexId::derive(&c, 0, b"a");
        let b = VertexId::derive(&c, 1, b"b");
        let cc = VertexId::derive(&c, 2, b"c");
        (c, g, a, b, cc)
    }

    #[test]
    fn commutation_disjoint_inserts() {
        let (_c, base, a, b, cc) = base_abc();
        let c1 = creator(b"ours");
        let c2 = creator(b"theirs");
        let x = vx(&c1, 0, b"x");
        let y = vx(&c2, 0, b"y");
        let p1 = Patch::from_ops(vec![Op::InsertAfter {
            anchor: a,
            before: b,
            vertex: x.clone(),
        }]);
        let p2 = Patch::from_ops(vec![Op::InsertAfter {
            anchor: b,
            before: cc,
            vertex: y.clone(),
        }]);
        assert!(commute(&p1, &p2));
        assert!(commute(&p2, &p1));

        let mut g1 = base.clone();
        p1.apply(&mut g1).unwrap();
        p2.apply(&mut g1).unwrap();

        let mut g2 = base.clone();
        p2.apply(&mut g2).unwrap();
        p1.apply(&mut g2).unwrap();

        let expected = vec![
            b"a".to_vec(),
            b"x".to_vec(),
            b"b".to_vec(),
            b"y".to_vec(),
            b"c".to_vec(),
        ];
        let g1f: Vec<Vec<u8>> = g1.flatten().iter().map(|s| s.to_vec()).collect();
        let g2f: Vec<Vec<u8>> = g2.flatten().iter().map(|s| s.to_vec()).collect();
        assert_eq!(g1f, expected);
        assert_eq!(g2f, expected);
    }

    #[test]
    fn commutation_dependent_inserts() {
        let (_c, _base, a, b, _cc) = base_abc();
        let c1 = creator(b"ours");
        let c2 = creator(b"theirs");
        let x = vx(&c1, 0, b"x");
        let p1 = Patch::from_ops(vec![Op::InsertAfter {
            anchor: a,
            before: b,
            vertex: x.clone(),
        }]);
        // P2 references x as its anchor — it depends on P1.
        let y = vx(&c2, 0, b"y");
        let p2 = Patch::from_ops(vec![Op::InsertAfter {
            anchor: x.id,
            before: b,
            vertex: y,
        }]);
        assert!(!commute(&p1, &p2));
        assert!(!commute(&p2, &p1));
    }

    #[test]
    fn three_way_merge_happy_path() {
        let (_c, base, a, b, cc) = base_abc();
        let c1 = creator(b"ours");
        let c2 = creator(b"theirs");
        let x = vx(&c1, 0, b"x");
        let y = vx(&c2, 0, b"y");
        let ours = Patch::from_ops(vec![Op::InsertAfter {
            anchor: a,
            before: b,
            vertex: x.clone(),
        }]);
        let theirs = Patch::from_ops(vec![Op::InsertAfter {
            anchor: b,
            before: cc,
            vertex: y.clone(),
        }]);
        let res = three_way_merge(&base, &ours, &theirs).unwrap();
        assert!(res.conflicts.is_empty());
        let got: Vec<Vec<u8>> = res.graph.flatten().iter().map(|s| s.to_vec()).collect();
        assert_eq!(
            got,
            vec![
                b"a".to_vec(),
                b"x".to_vec(),
                b"b".to_vec(),
                b"y".to_vec(),
                b"c".to_vec()
            ]
        );
    }

    #[test]
    fn three_way_merge_concurrent_insertion_same_anchor() {
        let (_c, base, a, b, _cc) = base_abc();
        let c1 = creator(b"ours");
        let c2 = creator(b"theirs");
        let x = vx(&c1, 0, b"x");
        let y = vx(&c2, 0, b"y");
        let ours = Patch::from_ops(vec![Op::InsertAfter {
            anchor: a,
            before: b,
            vertex: x.clone(),
        }]);
        let theirs = Patch::from_ops(vec![Op::InsertAfter {
            anchor: a,
            before: b,
            vertex: y.clone(),
        }]);
        let res = three_way_merge(&base, &ours, &theirs).unwrap();
        assert_eq!(res.conflicts.len(), 1);
        match &res.conflicts[0] {
            StructuredConflict::ConcurrentInsert {
                anchor,
                before,
                ours: o,
                theirs: t,
            } => {
                assert_eq!(*anchor, a);
                assert_eq!(*before, b);
                assert_eq!(*o, x.id);
                assert_eq!(*t, y.id);
            }
            other => panic!("expected ConcurrentInsert, got {other:?}"),
        }
        // Repository not wedged: both lines present, valid topological order.
        let got: Vec<Vec<u8>> = res.graph.flatten().iter().map(|s| s.to_vec()).collect();
        assert!(got.contains(&b"x".to_vec()));
        assert!(got.contains(&b"y".to_vec()));
        assert!(got.contains(&b"a".to_vec()));
        assert!(got.contains(&b"b".to_vec()));
        assert!(got.contains(&b"c".to_vec()));
        // a precedes both x and y; b follows both.
        let pos = |needle: &[u8]| got.iter().position(|v| v.as_slice() == needle).unwrap();
        assert!(pos(b"a") < pos(b"x"));
        assert!(pos(b"a") < pos(b"y"));
        assert!(pos(b"x") < pos(b"b"));
        assert!(pos(b"y") < pos(b"b"));
        assert!(pos(b"b") < pos(b"c"));
    }

    #[test]
    fn three_way_merge_edit_vs_delete() {
        let (_c, base, _a, b, _cc) = base_abc();
        let c2 = creator(b"theirs");
        let y = vx(&c2, 0, b"y");
        let ours = Patch::from_ops(vec![Op::Kill { target: b }]);
        // Theirs inserts after b — even though b is about to be killed.
        let cc = VertexId::derive(&creator(b"base"), 2, b"c");
        let theirs = Patch::from_ops(vec![Op::InsertAfter {
            anchor: b,
            before: cc,
            vertex: y.clone(),
        }]);
        let res = three_way_merge(&base, &ours, &theirs).unwrap();
        assert_eq!(res.conflicts.len(), 1);
        match &res.conflicts[0] {
            StructuredConflict::EditVsDelete {
                target,
                deleter,
                editor,
            } => {
                assert_eq!(*target, b);
                assert!(matches!(deleter, PatchSide::Ours));
                assert!(matches!(editor, PatchSide::Theirs));
            }
            other => panic!("expected EditVsDelete, got {other:?}"),
        }
        // b is dead; y is alive between (dead) b and c.
        assert_eq!(res.graph.is_alive(&b), Some(false));
        assert_eq!(res.graph.is_alive(&y.id), Some(true));
        let got: Vec<Vec<u8>> = res.graph.flatten().iter().map(|s| s.to_vec()).collect();
        assert_eq!(got, vec![b"a".to_vec(), b"y".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn merge_result_graph_is_acyclic() {
        let (_c, base, a, b, _cc) = base_abc();
        let c1 = creator(b"ours");
        let c2 = creator(b"theirs");
        let x = vx(&c1, 0, b"x");
        let y = vx(&c2, 0, b"y");
        let ours = Patch::from_ops(vec![Op::InsertAfter {
            anchor: a,
            before: b,
            vertex: x,
        }]);
        let theirs = Patch::from_ops(vec![Op::InsertAfter {
            anchor: a,
            before: b,
            vertex: y,
        }]);
        let res = three_way_merge(&base, &ours, &theirs).unwrap();
        assert!(res.graph.is_acyclic());
    }
}
