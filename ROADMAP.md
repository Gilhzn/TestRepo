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
- ⬜ Full equivalence proof: Yjs op-by-op → Pijul ops (research-grade)
- ⬜ LSP server (`mosaic-lsp`)
- ⬜ VS Code extension alpha

## ⬜ M4 — Real-time collaboration
- ⬜ WebSocket hub (y-websocket protocol)
- ⬜ WebRTC P2P fallback
- ⬜ Awareness / presence channel
- ⬜ Agent session protocol (session → atomic change stamp)

## 🔨 M5 — Semantic merge
- ✅ tree-sitter integration (Rust, Python, TypeScript)
- ✅ Content-addressable AST + body-only hashes
- ✅ Definition extraction (functions, structs, classes, interfaces)
- ✅ Rename detection via body-hash matching
- ✅ Three-way semantic analysis with call-site hints
- ⬜ Full GumTree algorithm (research-grade)
- ⬜ Wire into merge engine as 3rd fallback strategy
- ⬜ "Explain my merge" UI affordance

## 🔨 M6 — Agent SDK
- ✅ Rust SDK (`mosaic-sdk`)
- ✅ Session API: begin/edit/abort/commit → atomic Change
- ✅ Speculation: branch / promote / discard for trial-and-error work
- ✅ attach_as_agent for agent-scoped identities inside human repos
- ⬜ TypeScript SDK (`mosaic-sdk-ts`)
- ⬜ Agent identity attestation (Sigstore-style)

## ⬜ M7 — Scale + forge
- ⬜ Virtual FS (FUSE on Linux/macOS, ProjectedFS on Windows)
- ⬜ S3 / GCS backend for CAS offload
- ⬜ Web UI: review, branch graph, conflict resolution
- ⬜ Permissions, ACLs, signed-push policies
- ⬜ Public beta

---

## Current test count: **124 / 124 passing**
