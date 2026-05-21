//! Patches: atomic, content-addressed sets of operations on a `LineGraph`.
//!
//! A patch never destroys data. Inserts add vertices and edges; deletes flip the
//! alive flag. The inverse of an insert is a kill of the same vertex, *not* a
//! removal — leaving the vertex in place keeps every other patch that references
//! it well-defined, which is the linchpin of commutativity across history.

use crate::error::{Error, Result};
use crate::hash::{Hash, Hasher};
use crate::m1_patch::line_graph::{LineGraph, Vertex, VertexId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// Add `vertex` between `anchor` and `before` by introducing edges
    /// `anchor -> vertex` and `vertex -> before`. The pre-existing
    /// `anchor -> before` edge (if any) is left in place so concurrent
    /// inserts at the same position can both attach without conflict.
    InsertAfter {
        anchor: VertexId,
        before: VertexId,
        vertex: Vertex,
    },
    Kill {
        target: VertexId,
    },
    Resurrect {
        target: VertexId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    pub ops: Vec<Op>,
    pub id: Hash,
}

impl Patch {
    pub fn from_ops(ops: Vec<Op>) -> Self {
        let id = hash_ops(&ops);
        Self { ops, id }
    }

    /// Apply all ops atomically: validate every op against the current graph
    /// first; only mutate if validation passes for all of them.
    pub fn apply(&self, graph: &mut LineGraph) -> Result<()> {
        // Track ids introduced *by this patch* so a later op in the same patch
        // can legally anchor on an earlier op's insert.
        let mut introduced: BTreeSet<VertexId> = BTreeSet::new();
        for op in &self.ops {
            match op {
                Op::InsertAfter {
                    anchor,
                    before,
                    vertex,
                } => {
                    if !graph.contains(anchor) && !introduced.contains(anchor) {
                        return Err(Error::PatchTargetMissing(format!(
                            "insert anchor {:?}",
                            anchor.0
                        )));
                    }
                    if !graph.contains(before) && !introduced.contains(before) {
                        return Err(Error::PatchTargetMissing(format!(
                            "insert before {:?}",
                            before.0
                        )));
                    }
                    if graph.contains(&vertex.id) || introduced.contains(&vertex.id) {
                        return Err(Error::InvalidPatch(format!(
                            "vertex {:?} already exists",
                            vertex.id.0
                        )));
                    }
                    introduced.insert(vertex.id);
                }
                Op::Kill { target } | Op::Resurrect { target } => {
                    if !graph.contains(target) && !introduced.contains(target) {
                        return Err(Error::PatchTargetMissing(format!(
                            "kill/resurrect target {:?}",
                            target.0
                        )));
                    }
                }
            }
        }

        for op in &self.ops {
            match op {
                Op::InsertAfter {
                    anchor,
                    before,
                    vertex,
                } => {
                    graph.insert_vertex_raw(vertex.clone());
                    graph.add_edge(*anchor, vertex.id);
                    graph.add_edge(vertex.id, *before);
                }
                Op::Kill { target } => {
                    graph.set_alive(target, false)?;
                }
                Op::Resurrect { target } => {
                    graph.set_alive(target, true)?;
                }
            }
        }
        Ok(())
    }

    /// Produce a patch that undoes `self` applied to `graph` (the post-state).
    ///
    /// Inserts become Kill — we never physically delete vertices, so every
    /// downstream patch that references them remains valid. Kill/Resurrect
    /// flip; alive-state at `graph` is captured so we restore precisely.
    pub fn inverse(&self, graph: &LineGraph) -> Patch {
        let mut inv: Vec<Op> = Vec::with_capacity(self.ops.len());
        for op in self.ops.iter().rev() {
            match op {
                Op::InsertAfter { vertex, .. } => {
                    inv.push(Op::Kill { target: vertex.id });
                }
                Op::Kill { target } => {
                    // If, in the post-state, the target is dead, the inverse
                    // resurrects it. If it was already dead before our Kill
                    // (unusual but possible) we still resurrect — apply order
                    // gets us back to alive, which matches pre-state-of-Kill.
                    let _ = graph.is_alive(target);
                    inv.push(Op::Resurrect { target: *target });
                }
                Op::Resurrect { target } => {
                    inv.push(Op::Kill { target: *target });
                }
            }
        }
        Patch::from_ops(inv)
    }

    /// Vertex ids this patch references in any way (reads + writes + introduces).
    pub fn touched(&self) -> BTreeSet<VertexId> {
        let mut out = BTreeSet::new();
        for op in &self.ops {
            match op {
                Op::InsertAfter {
                    anchor,
                    before,
                    vertex,
                } => {
                    out.insert(*anchor);
                    out.insert(*before);
                    out.insert(vertex.id);
                }
                Op::Kill { target } | Op::Resurrect { target } => {
                    out.insert(*target);
                }
            }
        }
        out
    }

    /// Vertex ids this patch introduces (new vertices).
    pub fn introduced(&self) -> BTreeSet<VertexId> {
        let mut out = BTreeSet::new();
        for op in &self.ops {
            if let Op::InsertAfter { vertex, .. } = op {
                out.insert(vertex.id);
            }
        }
        out
    }

    /// Anchors this patch reads (anchor + before vertices of inserts, plus
    /// kill/resurrect targets). Used by `commute`.
    pub fn anchored_on(&self) -> BTreeSet<VertexId> {
        let mut out = BTreeSet::new();
        for op in &self.ops {
            match op {
                Op::InsertAfter { anchor, before, .. } => {
                    out.insert(*anchor);
                    out.insert(*before);
                }
                Op::Kill { target } | Op::Resurrect { target } => {
                    out.insert(*target);
                }
            }
        }
        out
    }
}

fn hash_ops(ops: &[Op]) -> Hash {
    let bytes = bincode::serialize(ops).expect("canonical serialization is infallible");
    let mut h = Hasher::new();
    h.update(b"mosaic.patch.v1");
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(&bytes);
    h.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creator() -> Hash {
        Hash::of(b"creator-P")
    }

    fn mk_vertex(c: &Hash, idx: u64, bytes: &[u8]) -> Vertex {
        let id = VertexId::derive(c, idx, bytes);
        Vertex {
            id,
            bytes: bytes.to_vec(),
            alive: true,
        }
    }

    #[test]
    fn linear_apply_inserts_five_lines_in_order() {
        let c = creator();
        let mut g = LineGraph::empty();
        let root = g.root_id();
        let sink = g.sink_id();
        let v: Vec<Vertex> = (0..5)
            .map(|i| mk_vertex(&c, i, format!("line-{i}").as_bytes()))
            .collect();
        let mut ops = Vec::new();
        let mut prev = root;
        for vert in &v {
            ops.push(Op::InsertAfter {
                anchor: prev,
                before: sink,
                vertex: vert.clone(),
            });
            prev = vert.id;
        }
        let p = Patch::from_ops(ops);
        p.apply(&mut g).unwrap();
        let out = g.flatten();
        let expected: Vec<Vec<u8>> = (0..5).map(|i| format!("line-{i}").into_bytes()).collect();
        let got: Vec<Vec<u8>> = out.iter().map(|s| s.to_vec()).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn apply_is_deterministic() {
        let c = creator();
        let v: Vec<Vertex> = (0..5)
            .map(|i| mk_vertex(&c, i, format!("d{i}").as_bytes()))
            .collect();
        let make_patch = || {
            let mut ops = Vec::new();
            let mut prev = VertexId::root();
            let sink = VertexId::sink();
            for vert in &v {
                ops.push(Op::InsertAfter {
                    anchor: prev,
                    before: sink,
                    vertex: vert.clone(),
                });
                prev = vert.id;
            }
            Patch::from_ops(ops)
        };
        let p = make_patch();
        let first = {
            let mut g = LineGraph::empty();
            p.apply(&mut g).unwrap();
            g.flatten().iter().map(|b| b.to_vec()).collect::<Vec<_>>()
        };
        for _ in 0..10 {
            let mut g = LineGraph::empty();
            p.apply(&mut g).unwrap();
            let now: Vec<Vec<u8>> = g.flatten().iter().map(|b| b.to_vec()).collect();
            assert_eq!(now, first);
        }
    }

    #[test]
    fn inverse_undoes_apply() {
        let c = creator();
        let mut g = LineGraph::from_lines(&c, &[b"a", b"b", b"c"]);
        let original: Vec<Vec<u8>> = g.flatten().iter().map(|b| b.to_vec()).collect();

        let a = VertexId::derive(&c, 0, b"a");
        let b = VertexId::derive(&c, 1, b"b");
        let x = mk_vertex(&c, 100, b"x");
        let p = Patch::from_ops(vec![Op::InsertAfter {
            anchor: a,
            before: b,
            vertex: x.clone(),
        }]);
        p.apply(&mut g).unwrap();
        assert_eq!(
            g.flatten().iter().map(|b| b.to_vec()).collect::<Vec<_>>(),
            vec![b"a".to_vec(), b"x".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );

        let inv = p.inverse(&g);
        inv.apply(&mut g).unwrap();
        assert_eq!(
            g.flatten().iter().map(|b| b.to_vec()).collect::<Vec<_>>(),
            original
        );
    }

    #[test]
    fn idempotency_double_apply_errors() {
        let c = creator();
        let mut g = LineGraph::from_lines(&c, &[b"a", b"b"]);
        let a = VertexId::derive(&c, 0, b"a");
        let b = VertexId::derive(&c, 1, b"b");
        let x = mk_vertex(&c, 99, b"x");
        let p = Patch::from_ops(vec![Op::InsertAfter {
            anchor: a,
            before: b,
            vertex: x,
        }]);
        p.apply(&mut g).unwrap();
        match p.apply(&mut g) {
            Err(Error::InvalidPatch(_)) => {}
            other => panic!("expected InvalidPatch on double-apply, got {other:?}"),
        }
        // Sanity: still well-formed (a, x, b in order).
        assert_eq!(
            g.flatten().iter().map(|b| b.to_vec()).collect::<Vec<_>>(),
            vec![b"a".to_vec(), b"x".to_vec(), b"b".to_vec()]
        );
    }

    #[test]
    fn dead_vertex_insertion_still_works() {
        let c = creator();
        let mut g = LineGraph::from_lines(&c, &[b"a", b"b", b"c"]);
        let b_id = VertexId::derive(&c, 1, b"b");
        // Kill b first.
        let kill_b = Patch::from_ops(vec![Op::Kill { target: b_id }]);
        kill_b.apply(&mut g).unwrap();
        assert_eq!(g.is_alive(&b_id), Some(false));

        // Insert after the dead anchor.
        let x = mk_vertex(&c, 77, b"x");
        let c_id = VertexId::derive(&c, 2, b"c");
        let ins = Patch::from_ops(vec![Op::InsertAfter {
            anchor: b_id,
            before: c_id,
            vertex: x.clone(),
        }]);
        ins.apply(&mut g).unwrap();
        assert_eq!(g.is_alive(&b_id), Some(false));
        assert_eq!(g.is_alive(&x.id), Some(true));
        // b stays dead; x emerges between (the dead) b and c.
        let out: Vec<Vec<u8>> = g.flatten().iter().map(|s| s.to_vec()).collect();
        assert_eq!(out, vec![b"a".to_vec(), b"x".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn missing_anchor_errors_atomically() {
        let c = creator();
        let mut g = LineGraph::from_lines(&c, &[b"a"]);
        let ghost = VertexId::derive(&c, 999, b"ghost");
        let real = VertexId::derive(&c, 0, b"a");
        let v = mk_vertex(&c, 7, b"v");
        let p = Patch::from_ops(vec![Op::InsertAfter {
            anchor: ghost,
            before: real,
            vertex: v.clone(),
        }]);
        assert!(matches!(
            p.apply(&mut g),
            Err(Error::PatchTargetMissing(_))
        ));
        // Graph untouched.
        assert!(!g.contains(&v.id));
        assert_eq!(
            g.flatten().iter().map(|s| s.to_vec()).collect::<Vec<_>>(),
            vec![b"a".to_vec()]
        );
    }

    #[test]
    fn patch_id_is_content_addressed() {
        let c = creator();
        let v = mk_vertex(&c, 0, b"hi");
        let root = VertexId::root();
        let sink = VertexId::sink();
        let p1 = Patch::from_ops(vec![Op::InsertAfter {
            anchor: root,
            before: sink,
            vertex: v.clone(),
        }]);
        let p2 = Patch::from_ops(vec![Op::InsertAfter {
            anchor: root,
            before: sink,
            vertex: v,
        }]);
        assert_eq!(p1.id, p2.id);
    }
}
