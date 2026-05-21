# Mosaic

An agent-native version control system. Designed from scratch for a world where
most commits will come from AI agents, orchestrated by human developers.

Git was designed in 2005 for a small team of humans mailing each other patches.
Its mental model — snapshots, linear history, three-way merges — generates
brutal friction when ten AI agents and three humans are working in parallel on
the same code. Mosaic replaces that model with one engineered for the world we
now live in.

## Pillars

- **Mathematically clean parallel merges.** Files are stored as line graphs;
  patches are commutative ops on those graphs. Two patches touching disjoint
  regions yield the same result in either order. The repository is never
  wedged — leftover ambiguities surface as structured conflicts, not text
  markers.
- **Conflict-as-data.** Conflicts are typed values an AI can read and resolve
  programmatically. `ConcurrentInsert`, `EditVsDelete`, etc. No `<<<<<<<`.
- **First-class agent identity.** `Identity` is either `Human { email }` or
  `Agent { name, session_id, invoker: Human }`. Every change is signed
  (ed25519) and carries full provenance.
- **Content-addressable + chunked.** BLAKE3-keyed CAS, zstd compression,
  FastCDC content-defined chunking for binaries and large files (datasets,
  ML models). Deduplication is automatic and global.
- **Branches as frontiers.** A branch is a set of change hashes, not a chain.
  Two peers with the same set of changes see the same branch state, regardless
  of arrival order. Borrowed from Jujutsu.

## Status

This is an active build of the M1 milestone (storage + change model + patch
algebra + branch frontiers). The CLI exercises every piece end to end.

- 79 unit tests across the workspace, 0 failures
- The full architecture spec lives in `/root/.claude/plans/lexical-wibbling-whistle.md`
  (12-month roadmap from storage spine to public beta)

## Try it

```bash
cargo build --release
alias mos=./target/release/mos

mkdir myrepo && cd myrepo
mos init
mos id setup --email you@example.com --name "Your Name"

echo "hello world" > readme.md
mos commit --intent "first change" --file readme.md
mos log
mos branch list
mos stats
```

## See the parallel-merge thesis in action

```bash
mos demo
```

Three independent edits to the same source file, two on disjoint regions
(auto-merged, zero conflicts) and two on the same anchor (merged into a valid
graph with one structured `ConcurrentInsert` conflict an agent can resolve).

## Layout

```
mosaic-core/
  src/
    hash.rs           BLAKE3 + streaming hasher
    storage.rs        FsCas: zstd-compressed loose-object CAS
    chunker.rs        FastCDC for large blobs, with Manifest + reassemble
    m1/
      identity.rs     Human | Agent (validated, depth-capped)
      signing.rs      ed25519 wrappers
      change.rs       Change, ChangeId, ChangeBuilder, Tai64N, FileChange
    m1_dag/
      vclock.rs       VectorClock with merge / happens_before / partial_cmp
      dag.rs          ChangeStore, FsChangeStore, DagIndex (Kahn topo, LCA)
      refs.rs         Branch frontiers + atomic RefStore
    m1_patch/
      line_graph.rs   LineGraph with sentinel root/sink, deterministic flatten
      patch.rs        Op, Patch (atomic apply, inverse via demote-to-kill)
      merge.rs        commute(), three_way_merge(), StructuredConflict
    repo.rs           Repository facade: storage + DAG + refs + identity
mosaic-cli/
  src/main.rs         `mos` binary (init, id, commit, log, branch, demo, …)
```

## Next milestones

See the plan file referenced above. In short:

- **M2** — bulk sync over HTTP/2 with vector-clock negotiation, Git read-only
  import bridge
- **M3** — Yjs working-copy CRDT and ops ↔ patch compiler; LSP server
- **M4** — WebSocket hub + WebRTC fallback for live multi-agent co-editing
- **M5** — tree-sitter + GumTree semantic merge layered on top of the patch
  fallback chain (TS / Python / Rust / Java first)
- **M6** — Agent SDK (Rust + TS) with session API and identity attestation
- **M7** — virtual FS, S3 backend, web UI, public beta

## License

Apache-2.0 OR MIT, your choice.
