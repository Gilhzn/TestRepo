# Mosaic Roadmap

Updated continuously as milestones progress. ✅ = done, 🔨 = in progress, ⬜ = not started.

---

## ✅ M0 — Storage spine
- ✅ BLAKE3 hashing + streaming hasher
- ✅ Content-addressable store (FsCas) with zstd compression
- ✅ FastCDC chunking + manifests for large blobs
- ✅ Tests: 12

## ✅ M1 — Change DAG + patches
- ✅ Identity (Human | Agent) with validation + provenance
- ✅ ed25519 signing wrappers
- ✅ Change schema with deterministic canonical serialization
- ✅ Vector clocks per author
- ✅ DAG index: ancestors, common ancestors, topological order
- ✅ Branch frontiers (jj-style named sets)
- ✅ Line graph with sentinel root/sink
- ✅ Patch algebra: InsertAfter / Kill / Resurrect, atomic apply, inverse
- ✅ commute() + three_way_merge() with conflict-as-data
- ✅ Repository facade
- ✅ CLI: init, id, commit, log, branch, stats, put, cat, demo
- ✅ Tests: 67

## ✅ M2 — Sync + Git interop
- ✅ Bundle format: serialize a set of changes + their CAS blobs
- ✅ Bundle apply: import bundle into another repo, verify sigs, register DAG
- ✅ Frontier diff: compute "changes you have, I don't" given two frontiers
- ✅ CLI: `mos bundle create/apply/inspect`, two-repo sync demo
- ✅ Git import bridge (read-only): walk git history, emit Mosaic changes
- ✅ Tests: 10

## ✅ M3 — CRDT integration
- ✅ yrs working-copy CRDT doc per text file (two-peer convergence)
- ✅ Yjs state-vector based delta sync
- ✅ Session compiler: before/after text → Mosaic Patch (line-graph)
- ✅ LSP server (`mosaic-lsp`): document tracking + custom commands
  (mosaic.commit/log/branches/status) + hover info
- ✅ **VS Code extension** (`mosaic-vscode`): launches mosaic-lsp via
  vscode-languageclient/node, surfaces all 5 LSP commands in the
  palette, status-bar entry, configurable LSP path + server URL
- ✅ **Yjs↔Pijul equivalence under fuzzing**: property-based tests with
  deterministic seeded PRNG verify that (a) a CRDT session's final
  snapshot, when lowered to a Mosaic patch + applied to a fresh line
  graph, flattens to the same lines as the snapshot (32 rounds × 40
  random ops), and (b) two peers exchanging Yjs updates after
  concurrent random edits converge byte-identically (16 rounds × 25
  ops per peer).

## ✅ M4 — Real-time collaboration
- ✅ HTTP sync server (`mosaic-serve` binary)
- ✅ REST API: /health, /branches, /branches/:name, /missing, /bundle
- ✅ Branch-aware bundles: branch_advances ride along with push
- ✅ CLI: `mos remote add/list/remove`, `mos push/pull`
- ✅ Two-machine multi-direction sync verified end-to-end
- ✅ WebSocket relay (`/ws/doc/:name`): broadcasts binary CRDT updates
  between peers, replays history to late joiners, per-document rooms
- ✅ Rust SDK live client (`mosaic-sdk::live::LiveSession`) behind `live`
  feature flag
- ✅ TypeScript SDK live client (`LiveSession.connect()`)
- ✅ Awareness / presence channel (`/ws/awareness/:name`): server-assigned
  peer ids (clients can't spoof), JSON envelopes with auto-injected peer,
  hello/leave notifications
- ✅ History cap / compaction: per-room update log drops oldest entries
  past `DEFAULT_HISTORY_CAP` (10k), runtime-tunable via `set_history_cap`
- ✅ **WebRTC signaling channel** (`/ws/signal/:name`): peers exchange
  SDP offers / answers / ICE candidates with server-tagged `from` and
  optional `to` for directed routing; broadcast fallback; hello carries
  the room roster so a peer can target the others immediately

## 🔨 M5 — Semantic merge
- ✅ tree-sitter integration (Rust, Python, TypeScript)
- ✅ Content-addressable AST + body-only hashes
- ✅ Definition extraction (functions, structs, classes, interfaces)
- ✅ Rename detection via body-hash matching
- ✅ Three-way semantic analysis with call-site hints
- ✅ Wire into merge engine as 3rd fallback strategy (`merge_text_file`)
- ✅ SDK exposure: `MosaicAgent::merge_file` / `analyze_three_way`
- ✅ **"Explain my merge"**: `FileMerge::explain()` + `mos merge --explain`
  produce human/AI-readable summary of strategy chain, outcome, renames,
  semantic hints with file positions, and structured conflicts
- ⬜ Full GumTree algorithm (research-grade)

## ✅ M6 — Agent SDK
- ✅ Rust SDK (`mosaic-sdk`)
- ✅ Session API: begin/edit/abort/commit → atomic Change
- ✅ Speculation: branch / promote / discard for trial-and-error work
- ✅ attach_as_agent for agent-scoped identities inside human repos
- ✅ TypeScript SDK (`mosaic-sdk-ts`): MosaicClient over HTTP, full types
- ✅ **Python SDK (`mosaic-sdk-py`)**: stdlib-only HTTP client + optional
  LiveSession over WebSocket; opens Mosaic to Python AI agents
- ✅ Agent attestation chain (`mosaic_core::attestation`): human's long-term
  key signs an Attestation pinning a session pubkey to an Agent identity
  with a validity window; verify() + authorize(Change) check the full chain
- ✅ Push pipeline enforces attestations: Bundle carries Vec<Attestation>;
  server accepts changes signed by session keys covered by a valid
  attestation from a trusted invoker (allowlist holds only the team's
  human keys; agents rotate session keys freely)

## 🔨 M7 — Scale + forge
- ✅ Web UI: dashboard with stats / branches / change history
- ✅ Web UI: per-change review page with file-level unified diffs
  (insert/delete/equal hunks), status badges, parent navigation
- ✅ `GET /api/v1/changes/:id/diff` returns structured FileDiff JSON
- ✅ Auth: per-key push allowlist; `mos trust add/list/remove/me` CLI;
  rejected push returns structured 403 with the offending key
- ✅ Branch graph visualization: `/api/v1/graph` computes lane assignment
  + depth via Kahn topo walk; SVG renderer in the dashboard with colored
  lanes, branch badges, and clickable change hashes
- ✅ **Object-storage Cas backends** (`mosaic_core::storage_remote`):
  `HttpCas` for any S3-compatible / MinIO / R2 / plain-HTTP object
  store, optional bearer token, zstd + integrity verify. `TieredCas<L,R>`
  composer for write-through local + remote with cache-on-miss reads.
- ✅ **Virtual FS via FUSE** (`mosaic-fuse` crate + `mosaic-mount` binary):
  mounts the latest state of a branch as a read-only filesystem; lazy
  lookup of files from CAS; standard ls/cat/find work end-to-end.
  Verified live in the Linux container.
- ⬜ Public beta

---

## Working-tree workflow (NEW — closes the muscle-memory gap)
- ✅ `mos status` — modified / untracked / staged / removed entries vs. branch tip
- ✅ `mos diff [path]` — unified diff of working tree vs. branch tip
- ✅ `mos add .` / `mos add <files>` — Git-like staging into `.mosaic/index.json`
- ✅ `mos unstage <files>` — drop from staged index without touching working tree
- ✅ `mos restore <path>` — discard working-tree changes back to branch tip
- ✅ `mos commit -i "..."` — uses staged index when no `-f` given, clears index after success
- ✅ `mos gc [--dry-run]` — mark-and-sweep prune of unreachable changes + CAS blobs
- ✅ Code review system: `mos comment / approve / request-changes / review`
  with ed25519-signed Comments + Approvals stored at
  `.mosaic/reviews/{comments,approvals}/<change>.jsonl`; server endpoints
  `GET/POST /api/v1/changes/:id/{comments,approvals}` verify signatures
  and reject tampered payloads with 403.

## Adoption + ecosystem layer (NEW)
- ✅ MCP server (`mosaic-mcp`) — Claude Code & other AI agents drive Mosaic
  natively via JSON-RPC tools over stdio (7-tool catalog)
- ✅ Outgoing webhooks (`mosaic-server::webhooks`) — POST events to CI / chat /
  deploy bots on push, optional HMAC signature
- ✅ Audit log (`mosaic-core::audit`) — append-only, ed25519-signed,
  per-session replay (`mos audit session <id>`), per-actor filter,
  full chain query
- ✅ Branch protection (`mosaic-server::protection`) — per-branch require_signed
  / approvals / no-force-push, per-path allow/deny by author (glob)
- ✅ Branch lifecycle (jj-style): `mos undo / abandon / amend / squash`
- ✅ Marketing-grade landing page at `/`; dashboard moved to `/dashboard`

## Current test count: **244 Rust + 6 TypeScript + 6 Python = 256 / 256 passing**
