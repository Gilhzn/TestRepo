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

## 🔨 M3 — CRDT integration
- ✅ yrs working-copy CRDT doc per text file (two-peer convergence)
- ✅ Yjs state-vector based delta sync
- ✅ Session compiler: before/after text → Mosaic Patch (line-graph)
- ✅ LSP server (`mosaic-lsp`): document tracking + custom commands
  (mosaic.commit/log/branches/status) + hover info
- ✅ **VS Code extension** (`mosaic-vscode`): launches mosaic-lsp via
  vscode-languageclient/node, surfaces all 5 LSP commands in the
  palette, status-bar entry, configurable LSP path + server URL
- ⬜ Full equivalence proof: Yjs op-by-op → Pijul ops (research-grade)

## 🔨 M4 — Real-time collaboration
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
- ⬜ WebRTC P2P fallback (server-optional path)
- ⬜ Server-side compaction of the update log

## 🔨 M5 — Semantic merge
- ✅ tree-sitter integration (Rust, Python, TypeScript)
- ✅ Content-addressable AST + body-only hashes
- ✅ Definition extraction (functions, structs, classes, interfaces)
- ✅ Rename detection via body-hash matching
- ✅ Three-way semantic analysis with call-site hints
- ✅ Wire into merge engine as 3rd fallback strategy (`merge_text_file`)
- ✅ SDK exposure: `MosaicAgent::merge_file` / `analyze_three_way`
- ⬜ Full GumTree algorithm (research-grade)
- ⬜ "Explain my merge" UI affordance

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
- ⬜ Virtual FS (FUSE on Linux/macOS, ProjectedFS on Windows)
- ⬜ Public beta

---

## Current test count: **176 Rust + 6 TypeScript + 6 Python = 188 / 188 passing**
