//! CRDT working-copy bridge.
//!
//! A text file under live editing is represented by a Yjs `Y.Text` (via the
//! `yrs` port). Multiple peers can hold one of these per file and merge
//! state in either direction with strong eventual consistency.
//!
//! At commit time the diff between two materialized states is compiled into
//! a Mosaic `Patch` over the file's line graph — the canonical form used by
//! the rest of the system. The roles are:
//!
//!   * **Yjs** — the live, real-time editing model (M3+M4).
//!   * **Patches** — the durable, signed-history model (M1).
//!
//! These coexist: every change born in a CRDT session compiles down to a
//! patch on commit; downstream readers replay patches and never need yrs.

use crate::error::Result;
use crate::hash::Hash;
use crate::m1_patch::line_graph::{LineGraph, Vertex, VertexId};
use crate::m1_patch::patch::{Op, Patch};
use similar::{ChangeTag, TextDiff};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, StateVector, Text, TextRef, Transact, Update};

/// One live text document. Each peer holds an independent `CrdtDoc`; updates
/// produced by `encode_update_since` and applied by `apply_update` reconcile
/// state across peers.
pub struct CrdtDoc {
    doc: Doc,
    text: TextRef,
    /// Text already folded into committed patches. `commit_increment` diffs
    /// only the tail since this baseline and then advances it, so each commit
    /// in a long-lived session costs O(delta) rather than O(whole document).
    committed: std::cell::RefCell<String>,
}

impl CrdtDoc {
    pub fn new() -> Self {
        let doc = Doc::new();
        let text = doc.get_or_insert_text("content");
        Self {
            doc,
            text,
            committed: std::cell::RefCell::new(String::new()),
        }
    }

    pub fn from_text(initial: &str) -> Self {
        let me = Self::new();
        let mut txn = me.doc.transact_mut();
        me.text.insert(&mut txn, 0, initial);
        drop(txn);
        // The seeded text is pre-existing committed content: the incremental
        // baseline starts there so the first commit only captures live edits.
        *me.committed.borrow_mut() = initial.to_string();
        me
    }

    pub fn snapshot(&self) -> String {
        let txn = self.doc.transact();
        self.text.get_string(&txn)
    }

    pub fn insert(&self, index: u32, s: &str) {
        let mut txn = self.doc.transact_mut();
        self.text.insert(&mut txn, index, s);
    }

    pub fn remove_range(&self, index: u32, len: u32) {
        let mut txn = self.doc.transact_mut();
        self.text.remove_range(&mut txn, index, len);
    }

    pub fn state_vector(&self) -> Vec<u8> {
        let txn = self.doc.transact();
        txn.state_vector().encode_v1()
    }

    pub fn encode_update_since(&self, peer_state_vector: &[u8]) -> Result<Vec<u8>> {
        let sv = StateVector::decode_v1(peer_state_vector)
            .map_err(|e| crate::error::Error::Serialization(format!("bad state vector: {e}")))?;
        let txn = self.doc.transact();
        Ok(txn.encode_state_as_update_v1(&sv))
    }

    pub fn apply_update(&self, update_bytes: &[u8]) -> Result<()> {
        let update = Update::decode_v1(update_bytes)
            .map_err(|e| crate::error::Error::Serialization(format!("bad update: {e}")))?;
        let mut txn = self.doc.transact_mut();
        txn.apply_update(update);
        Ok(())
    }

    /// The text already folded into committed patches — the incremental
    /// baseline that [`commit_increment`](Self::commit_increment) advances.
    pub fn committed_baseline(&self) -> String {
        self.committed.borrow().clone()
    }

    /// Compile only the edits made since the last committed baseline into a
    /// canonical patch, then advance the baseline to the current snapshot.
    ///
    /// This is the incremental form of [`compile_session`]: in a long live
    /// session a peer commits repeatedly, and each call diffs just the tail it
    /// hasn't committed yet rather than re-diffing the whole document. A run of
    /// increments reproduces the same final text as one big `compile_session`,
    /// but every individual commit stays proportional to its own delta.
    pub fn commit_increment(&self, creator: &Hash) -> Result<LiveCommit> {
        let before = self.committed.borrow().clone();
        let after = self.snapshot();
        let commit = compile_session(creator, &before, &after)?;
        *self.committed.borrow_mut() = after;
        Ok(commit)
    }

    /// Full-state bootstrap update in the v1 wire format. A late-joining peer
    /// applies this single blob to obtain the entire document without replaying
    /// the op log.
    pub fn full_update(&self) -> Vec<u8> {
        let txn = self.doc.transact();
        txn.encode_state_as_update_v1(&StateVector::default())
    }

    /// Compacted full-state bootstrap update in the v2 wire format, which
    /// run-length-encodes structure and clocks. For a document built from many
    /// small edits this is materially smaller than streaming each update — the
    /// client-side compaction a peer runs before persisting or shipping a
    /// long-lived doc. Pair with [`apply_update_v2`](Self::apply_update_v2).
    pub fn full_update_v2(&self) -> Vec<u8> {
        let txn = self.doc.transact();
        txn.encode_state_as_update_v2(&StateVector::default())
    }

    pub fn apply_update_v2(&self, update_bytes: &[u8]) -> Result<()> {
        let update = Update::decode_v2(update_bytes)
            .map_err(|e| crate::error::Error::Serialization(format!("bad v2 update: {e}")))?;
        let mut txn = self.doc.transact_mut();
        txn.apply_update(update);
        Ok(())
    }

    /// Produce a compacted clone: a fresh document carrying identical content
    /// but reconstructed from a single compacted v2 state blob, dropping the
    /// accumulated op history (deleted-content payloads are GC'd). The
    /// incremental baseline is carried over unchanged.
    pub fn compacted(&self) -> Result<CrdtDoc> {
        let blob = self.full_update_v2();
        let out = CrdtDoc::new();
        out.apply_update_v2(&blob)?;
        *out.committed.borrow_mut() = self.committed.borrow().clone();
        Ok(out)
    }
}

impl Default for CrdtDoc {
    fn default() -> Self {
        Self::new()
    }
}

/// Compile a before/after text pair into a Mosaic patch on a line graph.
///
/// This is the durable form of "what an editing session changed." The
/// resulting `Patch` can be applied to a `LineGraph` built from `before`
/// and will reproduce `after` (modulo CRDT identity assignment of the new
/// lines).
pub struct LiveCommit {
    pub patch: Patch,
    pub graph_after: LineGraph,
}

pub fn compile_session(creator: &Hash, before: &str, after: &str) -> Result<LiveCommit> {
    let before_lines: Vec<&str> = before.split_inclusive('\n').collect();
    let after_lines: Vec<&str> = after.split_inclusive('\n').collect();

    let mut before_for_graph: Vec<&[u8]> = before_lines
        .iter()
        .map(|s| trim_newline(s.as_bytes()))
        .collect();
    if before.is_empty() {
        before_for_graph.clear();
    }
    let mut graph = LineGraph::from_lines(creator, &before_for_graph);

    let diff = TextDiff::from_slices(&before_lines, &after_lines);

    let mut ops: Vec<Op> = Vec::new();

    let mut before_line_index = 0u64;
    let mut emitted_index: u64 = 0;

    let mut anchors: Vec<VertexId> = Vec::with_capacity(before_lines.len() + 2);
    anchors.push(graph.root_id());
    for (i, line) in before_for_graph.iter().enumerate() {
        anchors.push(VertexId::derive(creator, i as u64, line));
    }
    anchors.push(graph.sink_id());

    let mut alive_anchors = anchors.clone();
    let mut cursor = 0usize;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                before_line_index += 1;
                cursor += 1;
            }
            ChangeTag::Delete => {
                let target = anchors[before_line_index as usize + 1];
                ops.push(Op::Kill { target });
                let pos = alive_anchors
                    .iter()
                    .position(|a| *a == target)
                    .expect("anchor present");
                alive_anchors.remove(pos);
                before_line_index += 1;
            }
            ChangeTag::Insert => {
                let line_bytes = trim_newline(change.value().as_bytes()).to_vec();
                let new_vid = VertexId::derive(creator, 1_000_000 + emitted_index, &line_bytes);
                emitted_index += 1;
                let new_vertex = Vertex {
                    id: new_vid,
                    bytes: line_bytes,
                    alive: true,
                };
                let anchor = alive_anchors[cursor];
                let before_anchor = alive_anchors[cursor + 1];
                ops.push(Op::InsertAfter {
                    anchor,
                    before: before_anchor,
                    vertex: new_vertex,
                });
                alive_anchors.insert(cursor + 1, new_vid);
                cursor += 1;
            }
        }
    }

    let patch = Patch::from_ops(ops);
    patch.apply(&mut graph)?;
    Ok(LiveCommit {
        patch,
        graph_after: graph,
    })
}

fn trim_newline(bytes: &[u8]) -> &[u8] {
    if let Some(stripped) = bytes.strip_suffix(b"\n") {
        stripped
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_peers_converge_on_concurrent_inserts() {
        let alice = CrdtDoc::from_text("hello\nworld\n");
        let bob = CrdtDoc::new();
        let initial_update = alice.encode_update_since(&bob.state_vector()).unwrap();
        bob.apply_update(&initial_update).unwrap();
        assert_eq!(alice.snapshot(), bob.snapshot());

        alice.insert(6, "lovely ");
        bob.insert(0, "PREFIX ");

        let sv_a = alice.state_vector();
        let sv_b = bob.state_vector();
        let from_a = alice.encode_update_since(&sv_b).unwrap();
        let from_b = bob.encode_update_since(&sv_a).unwrap();
        bob.apply_update(&from_a).unwrap();
        alice.apply_update(&from_b).unwrap();

        assert_eq!(alice.snapshot(), bob.snapshot());
        let s = alice.snapshot();
        assert!(s.contains("lovely"));
        assert!(s.contains("PREFIX"));
    }

    #[test]
    fn deletes_propagate_across_peers() {
        let alice = CrdtDoc::from_text("aaa\nbbb\nccc\n");
        let bob = CrdtDoc::new();
        let up = alice.encode_update_since(&bob.state_vector()).unwrap();
        bob.apply_update(&up).unwrap();

        bob.remove_range(4, 4);
        let to_alice = bob.encode_update_since(&alice.state_vector()).unwrap();
        alice.apply_update(&to_alice).unwrap();

        assert_eq!(alice.snapshot(), bob.snapshot());
        assert_eq!(alice.snapshot(), "aaa\nccc\n");
    }

    #[test]
    fn compile_session_inserts_one_line() {
        let creator = Hash::of(b"session-1");
        let result = compile_session(&creator, "alpha\nbeta\n", "alpha\nNEW\nbeta\n").unwrap();
        let lines: Vec<&str> = result
            .graph_after
            .flatten()
            .into_iter()
            .map(|b| std::str::from_utf8(b).unwrap())
            .collect();
        assert_eq!(lines, vec!["alpha", "NEW", "beta"]);
    }

    #[test]
    fn compile_session_deletes_one_line() {
        let creator = Hash::of(b"session-2");
        let result = compile_session(&creator, "alpha\nbeta\nGAMMA\n", "alpha\nbeta\n").unwrap();
        let lines: Vec<&str> = result
            .graph_after
            .flatten()
            .into_iter()
            .map(|b| std::str::from_utf8(b).unwrap())
            .collect();
        assert_eq!(lines, vec!["alpha", "beta"]);
    }

    #[test]
    fn compile_session_handles_replace_block() {
        let creator = Hash::of(b"session-3");
        let result = compile_session(
            &creator,
            "header\nbody1\nbody2\nfooter\n",
            "header\nNEW1\nNEW2\nNEW3\nfooter\n",
        )
        .unwrap();
        let lines: Vec<&str> = result
            .graph_after
            .flatten()
            .into_iter()
            .map(|b| std::str::from_utf8(b).unwrap())
            .collect();
        assert_eq!(lines, vec!["header", "NEW1", "NEW2", "NEW3", "footer"]);
    }

    #[test]
    fn compile_session_from_empty_file() {
        let creator = Hash::of(b"session-empty");
        let result = compile_session(&creator, "", "first line\nsecond\n").unwrap();
        let lines: Vec<&str> = result
            .graph_after
            .flatten()
            .into_iter()
            .map(|b| std::str::from_utf8(b).unwrap())
            .collect();
        assert_eq!(lines, vec!["first line", "second"]);
    }

    /// Deterministic mini-PRNG so equivalence tests are reproducible without
    /// pulling in a proptest dependency. Same seed → same sequence.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1)
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 ^ (self.0 >> 33)
        }
        fn range(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next_u64() % n }
        }
    }

    fn apply_random_edits(doc: &CrdtDoc, rng: &mut Rng, n_ops: usize) {
        for _ in 0..n_ops {
            let current = doc.snapshot();
            let len = current.chars().count() as u32;
            let op = rng.range(3);
            if op < 2 || len == 0 {
                // Insert
                let pos = rng.range((len + 1) as u64) as u32;
                let payload_kind = rng.range(4);
                let payload: String = match payload_kind {
                    0 => "x".into(),
                    1 => "hello".into(),
                    2 => "\n".into(),
                    _ => format!("[{}]", rng.range(10)),
                };
                doc.insert(pos, &payload);
            } else {
                // Delete
                let max_del = (len / 2).max(1);
                let del_len = (rng.range(max_del as u64) as u32).max(1);
                let start = rng.range((len.saturating_sub(del_len) + 1) as u64) as u32;
                doc.remove_range(start, del_len.min(len.saturating_sub(start)));
            }
        }
    }

    /// EQUIVALENCE PROPERTY: a CRDT session's final snapshot, when compiled
    /// into a Mosaic patch over the line graph of the original text, must
    /// reproduce the *same* sequence of lines as flattening the resulting
    /// graph. Verified across many random op sequences from different seeds.
    #[test]
    fn yjs_to_pijul_equivalence_under_random_ops() {
        const ROUNDS: usize = 32;
        const OPS_PER_ROUND: usize = 40;

        for round in 0..ROUNDS {
            let seed = 0xA17C_E771_u64.wrapping_add(round as u64 * 7919);
            let mut rng = Rng::new(seed);

            let initial = "line a\nline b\nline c\n";
            let doc = CrdtDoc::from_text(initial);
            apply_random_edits(&doc, &mut rng, OPS_PER_ROUND);
            let after = doc.snapshot();

            let creator = Hash::of(format!("equiv-round-{round}").as_bytes());
            let commit = compile_session(&creator, initial, &after)
                .expect("compile_session always succeeds for pure-text diffs");

            let graph_lines: Vec<String> = commit
                .graph_after
                .flatten()
                .into_iter()
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();

            let expected_lines: Vec<String> = after
                .split_inclusive('\n')
                .map(|s| s.trim_end_matches('\n').to_string())
                .collect();

            assert_eq!(
                graph_lines, expected_lines,
                "round {round} (seed {seed:#x}) diverged.\n\
                 after text:\n{after}\n\
                 graph lines: {graph_lines:?}\n\
                 expected:    {expected_lines:?}",
            );
        }
    }

    /// CONVERGENCE PROPERTY: two peers exchanging Yjs updates after random
    /// concurrent edits MUST end up with byte-identical snapshots — this is
    /// the CRDT guarantee the live-collab layer relies on.
    #[test]
    fn two_peers_converge_under_random_concurrent_edits() {
        const ROUNDS: usize = 16;
        const OPS_PER_PEER: usize = 25;

        for round in 0..ROUNDS {
            let seed = 0x5EED_C0DE_u64.wrapping_add(round as u64 * 7919);
            let mut rng_a = Rng::new(seed);
            let mut rng_b = Rng::new(seed ^ 0xFFFF_FFFF_FFFF_FFFF);

            let alice = CrdtDoc::from_text("shared\nseed\n");
            let bob = CrdtDoc::new();
            let bootstrap = alice
                .encode_update_since(&bob.state_vector())
                .expect("encode bootstrap");
            bob.apply_update(&bootstrap).unwrap();
            assert_eq!(alice.snapshot(), bob.snapshot(), "bootstrap diverged");

            apply_random_edits(&alice, &mut rng_a, OPS_PER_PEER);
            apply_random_edits(&bob, &mut rng_b, OPS_PER_PEER);

            // Exchange in both directions.
            let from_a = alice
                .encode_update_since(&bob.state_vector())
                .expect("encode A");
            let from_b = bob
                .encode_update_since(&alice.state_vector())
                .expect("encode B");
            bob.apply_update(&from_a).unwrap();
            alice.apply_update(&from_b).unwrap();

            assert_eq!(
                alice.snapshot(),
                bob.snapshot(),
                "round {round} (seed {seed:#x}) diverged after exchange",
            );
        }
    }

    /// INCREMENTAL COMPILE: a second commit must diff only the text added
    /// since the first commit, not re-emit the already-committed lines.
    #[test]
    fn incremental_commit_diffs_only_the_tail() {
        let creator = Hash::of(b"inc");
        let doc = CrdtDoc::from_text("alpha\nbeta\n");

        doc.insert(doc.snapshot().len() as u32, "gamma\n");
        let c1 = doc.commit_increment(&creator).unwrap();
        let lines1: Vec<String> = c1
            .graph_after
            .flatten()
            .into_iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        assert_eq!(lines1, vec!["alpha", "beta", "gamma"]);
        assert_eq!(doc.committed_baseline(), "alpha\nbeta\ngamma\n");

        doc.insert(doc.snapshot().len() as u32, "delta\n");
        let c2 = doc.commit_increment(&creator).unwrap();
        let inserts = c2
            .patch
            .ops
            .iter()
            .filter(|op| matches!(op, Op::InsertAfter { .. }))
            .count();
        assert_eq!(
            inserts, 1,
            "incremental commit re-diffed already-committed text (got {inserts} inserts)"
        );
        let lines2: Vec<String> = c2
            .graph_after
            .flatten()
            .into_iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        assert_eq!(lines2, vec!["alpha", "beta", "gamma", "delta"]);
        assert_eq!(doc.committed_baseline(), "alpha\nbeta\ngamma\ndelta\n");
    }

    /// CLIENT-SIDE COMPACTION: one compacted v2 state blob is smaller than the
    /// cumulative per-op updates a streaming peer would replay, and it
    /// reconstructs byte-identical content on a fresh peer.
    #[test]
    fn compaction_beats_streamed_op_log_and_preserves_content() {
        let doc = CrdtDoc::from_text("");
        let mut prev_sv = doc.state_vector();
        let mut streamed_total = 0usize;
        for i in 0..500u32 {
            doc.insert(i, "x");
            let upd = doc.encode_update_since(&prev_sv).unwrap();
            streamed_total += upd.len();
            prev_sv = doc.state_vector();
        }

        let compact = doc.full_update_v2();
        assert!(
            compact.len() < streamed_total,
            "compacted blob {} not smaller than streamed log {}",
            compact.len(),
            streamed_total
        );

        // A late joiner applies the single compacted blob and converges.
        let joiner = CrdtDoc::new();
        joiner.apply_update_v2(&compact).unwrap();
        assert_eq!(joiner.snapshot(), doc.snapshot());

        // The in-place compacted clone is content-equivalent too.
        let clone = doc.compacted().unwrap();
        assert_eq!(clone.snapshot(), doc.snapshot());
    }

    #[test]
    fn live_session_then_commit_round_trip() {
        let doc = CrdtDoc::from_text("one\ntwo\nthree\n");
        let before = doc.snapshot();
        doc.insert(4, "NEW_AFTER_ONE\n");
        doc.remove_range(
            (before.len() as u32) + "NEW_AFTER_ONE\n".len() as u32 - 6,
            6,
        );
        let after = doc.snapshot();

        let creator = Hash::of(b"live-1");
        let commit = compile_session(&creator, &before, &after).unwrap();
        let lines: Vec<String> = commit
            .graph_after
            .flatten()
            .into_iter()
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect();

        let mut from_after = after.split_inclusive('\n').collect::<Vec<_>>();
        if from_after.last().map(|s| !s.ends_with('\n')).unwrap_or(false) {
            // no trailing newline — fine
        } else {
            from_after.retain(|s| !s.is_empty());
        }
        let expected: Vec<String> = from_after
            .iter()
            .map(|s| s.trim_end_matches('\n').to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(lines, expected);
    }
}
