//! Line-graph: directed graph of text lines.
//!
//! Vertices are content-addressed lines; edges express "must come before".
//! The graph carries two sentinels (`root`, `sink`) so every line lives strictly
//! between them. Dead vertices stay in the graph so patches that reference them
//! remain meaningful — this is what lets the inverse of an insert be a kill.

use crate::error::{Error, Result};
use crate::hash::{Hash, Hasher};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VertexId(pub Hash);

impl VertexId {
    pub fn derive(creator: &Hash, index: u64, bytes: &[u8]) -> Self {
        let mut h = Hasher::new();
        h.update(b"mosaic.vertex.v1");
        h.update(creator.as_bytes());
        h.update(&index.to_le_bytes());
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(bytes);
        VertexId(h.finalize())
    }

    fn sentinel(tag: &[u8]) -> Self {
        let mut h = Hasher::new();
        h.update(b"mosaic.vertex.sentinel.v1");
        h.update(tag);
        VertexId(h.finalize())
    }

    pub fn root() -> Self {
        Self::sentinel(b"root")
    }

    pub fn sink() -> Self {
        Self::sentinel(b"sink")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vertex {
    pub id: VertexId,
    pub bytes: Vec<u8>,
    pub alive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineGraph {
    vertices: BTreeMap<VertexId, Vertex>,
    edges_out: BTreeMap<VertexId, BTreeSet<VertexId>>,
    edges_in: BTreeMap<VertexId, BTreeSet<VertexId>>,
    root: VertexId,
    sink: VertexId,
}

impl LineGraph {
    pub fn empty() -> Self {
        let root = VertexId::root();
        let sink = VertexId::sink();
        let mut vertices = BTreeMap::new();
        vertices.insert(
            root,
            Vertex {
                id: root,
                bytes: Vec::new(),
                alive: false,
            },
        );
        vertices.insert(
            sink,
            Vertex {
                id: sink,
                bytes: Vec::new(),
                alive: false,
            },
        );
        let mut edges_out: BTreeMap<VertexId, BTreeSet<VertexId>> = BTreeMap::new();
        let mut edges_in: BTreeMap<VertexId, BTreeSet<VertexId>> = BTreeMap::new();
        edges_out.entry(root).or_default().insert(sink);
        edges_in.entry(sink).or_default().insert(root);
        edges_out.entry(sink).or_default();
        edges_in.entry(root).or_default();
        Self {
            vertices,
            edges_out,
            edges_in,
            root,
            sink,
        }
    }

    pub fn from_lines(creator: &Hash, lines: &[&[u8]]) -> Self {
        let mut g = Self::empty();
        let mut prev = g.root;
        // Drop the original root->sink edge; we will re-link at the end.
        g.remove_edge(&g.root.clone(), &g.sink.clone());
        for (i, line) in lines.iter().enumerate() {
            let id = VertexId::derive(creator, i as u64, line);
            let v = Vertex {
                id,
                bytes: line.to_vec(),
                alive: true,
            };
            g.insert_vertex_raw(v);
            g.add_edge(prev, id);
            prev = id;
        }
        g.add_edge(prev, g.sink);
        g
    }

    pub fn root_id(&self) -> VertexId {
        self.root
    }

    pub fn sink_id(&self) -> VertexId {
        self.sink
    }

    pub fn contains(&self, id: &VertexId) -> bool {
        self.vertices.contains_key(id)
    }

    pub fn is_alive(&self, id: &VertexId) -> Option<bool> {
        self.vertices.get(id).map(|v| v.alive)
    }

    pub fn get(&self, id: &VertexId) -> Option<&Vertex> {
        self.vertices.get(id)
    }

    pub fn has_edge(&self, from: &VertexId, to: &VertexId) -> bool {
        self.edges_out
            .get(from)
            .map(|s| s.contains(to))
            .unwrap_or(false)
    }

    pub(crate) fn insert_vertex_raw(&mut self, v: Vertex) {
        let id = v.id;
        self.vertices.insert(id, v);
        self.edges_out.entry(id).or_default();
        self.edges_in.entry(id).or_default();
    }

    pub(crate) fn add_edge(&mut self, from: VertexId, to: VertexId) {
        self.edges_out.entry(from).or_default().insert(to);
        self.edges_in.entry(to).or_default().insert(from);
    }

    pub(crate) fn remove_edge(&mut self, from: &VertexId, to: &VertexId) {
        if let Some(s) = self.edges_out.get_mut(from) {
            s.remove(to);
        }
        if let Some(s) = self.edges_in.get_mut(to) {
            s.remove(from);
        }
    }

    pub(crate) fn set_alive(&mut self, id: &VertexId, alive: bool) -> Result<()> {
        let v = self
            .vertices
            .get_mut(id)
            .ok_or_else(|| Error::PatchTargetMissing(format!("{:?}", id.0)))?;
        v.alive = alive;
        Ok(())
    }

    /// Deterministic topological walk of alive vertices from root toward sink.
    /// Uses Kahn's algorithm with hash tie-break (BTreeSet already orders).
    /// Returns the line bytes in order, excluding sentinels.
    pub fn flatten(&self) -> Vec<&[u8]> {
        // Kahn over the FULL graph (alive + dead) so topological structure is preserved,
        // then filter alive at emission time. This keeps order consistent with anchors
        // even when some intermediate vertices are dead.
        let mut indeg: BTreeMap<VertexId, usize> = BTreeMap::new();
        for (v, _) in self.vertices.iter() {
            let d = self.edges_in.get(v).map(|s| s.len()).unwrap_or(0);
            indeg.insert(*v, d);
        }
        // Ready set is a BTreeSet so we pop the smallest id deterministically.
        let mut ready: BTreeSet<VertexId> = BTreeSet::new();
        for (v, d) in indeg.iter() {
            if *d == 0 {
                ready.insert(*v);
            }
        }
        let mut order: Vec<VertexId> = Vec::with_capacity(self.vertices.len());
        let mut indeg_mut = indeg;
        while let Some(&v) = ready.iter().next() {
            ready.remove(&v);
            order.push(v);
            if let Some(succs) = self.edges_out.get(&v) {
                for s in succs {
                    if let Some(d) = indeg_mut.get_mut(s) {
                        *d -= 1;
                        if *d == 0 {
                            ready.insert(*s);
                        }
                    }
                }
            }
        }
        order
            .into_iter()
            .filter_map(|id| self.vertices.get(&id))
            .filter(|v| v.alive && v.id != self.root && v.id != self.sink)
            .map(|v| v.bytes.as_slice())
            .collect()
    }

    /// True iff Kahn covers every vertex — i.e. there are no cycles. Used for sanity.
    pub fn is_acyclic(&self) -> bool {
        let mut indeg: BTreeMap<VertexId, usize> = BTreeMap::new();
        for v in self.vertices.keys() {
            indeg.insert(*v, self.edges_in.get(v).map(|s| s.len()).unwrap_or(0));
        }
        let mut ready: VecDeque<VertexId> = indeg
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(v, _)| *v)
            .collect();
        let mut visited = 0usize;
        while let Some(v) = ready.pop_front() {
            visited += 1;
            if let Some(succs) = self.edges_out.get(&v) {
                for s in succs {
                    if let Some(d) = indeg.get_mut(s) {
                        *d -= 1;
                        if *d == 0 {
                            ready.push_back(*s);
                        }
                    }
                }
            }
        }
        visited == self.vertices.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creator() -> Hash {
        Hash::of(b"creator-A")
    }

    #[test]
    fn empty_graph_flatten_is_empty() {
        let g = LineGraph::empty();
        assert!(g.flatten().is_empty());
        assert!(g.has_edge(&g.root_id(), &g.sink_id()));
    }

    #[test]
    fn from_lines_flatten_round_trip() {
        let c = creator();
        let g = LineGraph::from_lines(&c, &[b"a", b"b", b"c"]);
        let out: Vec<&[u8]> = g.flatten();
        assert_eq!(out, vec![b"a".as_ref(), b"b".as_ref(), b"c".as_ref()]);
    }

    #[test]
    fn vertex_ids_are_content_addressed() {
        let c = creator();
        let id1 = VertexId::derive(&c, 0, b"hello");
        let id2 = VertexId::derive(&c, 0, b"hello");
        let id3 = VertexId::derive(&c, 1, b"hello");
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn from_lines_is_acyclic() {
        let c = creator();
        let g = LineGraph::from_lines(&c, &[b"x", b"y", b"z"]);
        assert!(g.is_acyclic());
    }
}
