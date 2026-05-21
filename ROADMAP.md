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
- ⬜ Full equivalence proof: Yjs op-by-op → Pijul ops (research-grade)
- ⬜ VS Code extension alpha

## 🔨 M4 — Real-time collaboration
- ✅ HTTP sync server (`mosaic-serve` binary)
- ✅ REST API: /health, /branches, /branches/:name, /missing, /bundle
- ✅ Branch-aware bundles: branch_advances ride along with push
- ✅ CLI: `mos remote add/list/remove`, `mos push/pull`
- ✅ Two-machine multi-direction sync verified end-to-end
- ⬜ WebSocket hub (y-websocket protocol) for live co-editing
- ⬜ WebRTC P2P fallback
- ⬜ Awareness / presence channel
- ⬜ Agent session protocol (session → atomic change stamp)

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

## 🔨 M6 — Agent SDK
- ✅ Rust SDK (`mosaic-sdk`)
- ✅ Session API: begin/edit/abort/commit → atomic Change
- ✅ Speculation: branch / promote / discard for trial-and-error work
- ✅ attach_as_agent for agent-scoped identities inside human repos
- ✅ TypeScript SDK (`mosaic-sdk-ts`): MosaicClient over HTTP, full types
- ⬜ Agent identity attestation (Sigstore-style)

## 🔨 M7 — Scale + forge
- ✅ Web UI: dashboard with stats / branches / change history (served by `mosaic-serve`)
- ⬜ Virtual FS (FUSE on Linux/macOS, ProjectedFS on Windows)
- ⬜ S3 / GCS backend for CAS offload
- ⬜ Web UI: review pages, branch graph, conflict resolution
- ⬜ Permissions, ACLs, signed-push policies
- ⬜ Public beta

---

## Current test count: **136 Rust + 4 TypeScript = 140 / 140 passing**
