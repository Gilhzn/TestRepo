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

Active build progressing through the 8-milestone roadmap. **3 milestones
fully complete (M0/M1/M2), 3 with working kernels (M3/M5/M6), 2 untouched
(M4/M7).** See `ROADMAP.md` for the live status checklist.

- **129 unit tests** across the workspace, 0 failures
- The full architecture spec lives in `/root/.claude/plans/lexical-wibbling-whistle.md`

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

## See the semantic-merge thesis in action

```bash
# base: fn chargeCard
# ours: renamed to processCharge
# theirs: kept old name, but changed body
mos merge payments.rs --base base.rs --ours ours.rs --theirs theirs.rs
```

Output:

```
--- merged result ---
fn processCharge(amount: u32) -> u32 {
    log_call();
    amount * 2
}
--- end ---

semantic hints:
  rename function_item chargeCard -> processCharge
  callsite at 1:4 uses old name chargeCard (now processCharge)
```

Mosaic picks up both the rename (from `ours`) and the body edit (from
`theirs`) and surfaces a structured hint about an old-name reference for
an AI or a human resolver to act on.

## Bundle-based sync between two repositories

```bash
# Alice
cd alice/
mos bundle create -o /tmp/alice.bundle

# Bob, on another machine or directory:
cd bob/
mos bundle apply /tmp/alice.bundle  # signatures verified end-to-end
mos log                              # sees Alice's history
```

## Two AI agents collaborating in real-time

End-to-end demo from the TypeScript SDK that ties every layer together:

```bash
cd mosaic-sdk-ts
npm install && npx tsc
node dist/examples/two-agents.js
```

Output (excerpted):

```
[14:58:50] system     starting mosaic-serve at http://127.0.0.1:44129
[14:58:50] system     agents joining live room /ws/doc/payments.ts
[14:58:50] agent-A    sending: 'fn chargeCard() { ... }'
[14:58:50] agent-B    received update (23B): fn chargeCard() { ... }
[14:58:50] agent-B    sending: 'fn fraudCheck() { ... }'
[14:58:50] agent-A    received update (23B): fn fraudCheck() { ... }
[14:58:50] agent-A    pushing to server
[14:58:50] agent-A    push result: 1 applied, 0 skipped
[14:58:50] system     server now has 1 change(s) on 1 branch(es)
```

## Import an existing Git repository

```bash
mos init && mos id setup --email you@example.com
mos import git /path/to/some/git/repo
mos log
```

Walks the full Git history (including merges) in topological-oldest-first
order, preserving authorship metadata in the change intent.

## Layout

```
mosaic-core/                Core library
  src/
    hash.rs                 BLAKE3 + streaming hasher
    storage.rs              FsCas: zstd-compressed loose-object CAS
    chunker.rs              FastCDC large-file chunking + manifest
    m1/{identity,signing,change}.rs    Identity, ed25519, Change schema
    m1_dag/{vclock,dag,refs}.rs        Vector clocks, DAG, branch frontiers
    m1_patch/{line_graph,patch,merge}.rs  Commutative patches, three-way merge
    repo.rs                 Repository facade
    sync.rs                 Bundle format + frontier diff + apply
    import_git.rs           Read-only Git import bridge
    crdt.rs                 yrs working-copy + session→patch compiler
    ast.rs                  tree-sitter AST + content-addressable hashes
    semantic.rs             Rename detection + call-site hints
    merge_strategies.rs     Combined patch + semantic merge facade

mosaic-sdk/                 Agent-native Rust SDK
  src/
    agent.rs                MosaicAgent: init / attach / sessions / speculation
    session.rs              Stage edits → atomic signed Change
    speculation.rs          Cheap parallel branches an agent can promote/discard

mosaic-cli/                 `mos` command-line binary
  src/main.rs               init / id / commit / log / branch / bundle /
                            import / merge / put / cat / stats / demo
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
