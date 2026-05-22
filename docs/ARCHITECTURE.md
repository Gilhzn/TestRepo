# Mosaic Architecture

A deep dive into how Mosaic is put together, layer by layer. If you've
read the README's one-screen diagram, this is the long form.

## Design principles

1. **Agent-native, not agent-bolted-on.** Most commits will come from AI
   agents. Every interface is API-first; conflicts are structured data;
   identity is cryptographically attributable to a human-rooted chain.
2. **The repo is never wedged.** Merges always produce a valid, acyclic,
   deterministically-orderable graph. Residual ambiguity is data, never a
   `<<<<<<<` marker that blocks the working tree.
3. **Content-addressed everything.** Blobs, changes, comments, issues, and
   attestations are all identified by BLAKE3 of their canonical bytes.
4. **Local-first, server-optional.** Every clone is a full peer. The server
   is a well-known peer for sync, auth, and the web forge — not a
   single point of truth.

## The seven layers

### L1 — Storage (`storage`, `storage_remote`, `encryption`, `chunker`)

A content-addressable store (CAS): `put(bytes) -> BLAKE3`, `get(hash)`,
`has(hash)`. Three implementations behind one `Cas` trait:

- `FsCas` — loose objects on disk, zstd-compressed, laid out as
  `<hash[..2]>/<hash[2..]>` (Git's scheme).
- `HttpCas` — any S3-compatible / MinIO / R2 / plain-HTTP object store,
  optional bearer token, same compression + integrity verification.
- `EncryptedCas` — ChaCha20-Poly1305 AEAD over the compressed bytes,
  keyed by a 32-byte master key. Content-addressed on the *plaintext*
  hash so dedup still works; plaintext never touches disk.

`TieredCas<L, R>` composes two stores: write-through to both, read local
first with remote fallback + cache-fill. Typical: `TieredCas<FsCas, HttpCas>`.

Large/binary files go through **FastCDC** content-defined chunking
(rolling hash, ~2 MiB average chunk), so near-identical blobs share most
chunks — 80-90% dedup on small edits to big files.

### L2 — Content model (`m1::change::FileChange`, `chunker::Manifest`)

A file in a change is `FileChange { path, kind, patch, conflicts }`.
Text files carry their content (and, post-M3, a CRDT sidecar). Binary
files become a `Manifest` of chunk hashes. Trees are hash-addressed
manifests.

### L3 — History DAG (`m1_dag`)

- `DagIndex` — in-memory parents/children/vector-clock index, rebuilt by
  replaying changes from the store. Provides `ancestors_of`,
  `common_ancestors`, `topological_order` (Kahn, deterministic hash
  tie-break), `is_ancestor`.
- `VectorClock` — one monotone counter per **author** (humans and agents
  alike), so cross-branch causality works.
- Branches are **frontiers**: a `Frontier` is a *set* of change hashes,
  not a chain. Two peers with the same change set see the same branch
  regardless of arrival order. (Jujutsu's model.)

### L4 — Change model (`m1::change`, `attestation`)

A `Change` is the atomic unit: `{ author, ts, deps, intent, body, sig,
author_key }`. Canonical bytes (everything but the signature) are signed
with ed25519; the change id is BLAKE3 of those bytes.

`Identity` is `Human { email }` or `Agent { name, session_id, invoker }`.
An `Attestation` binds an agent's ephemeral session key to its claimed
identity + a validity window, signed by the invoker's long-term human
key. This is the chain the server enforces on push.

### L5 — Merge engine (`m1_patch`, `merge_strategies`, `semantic`, `ast`)

A fallback chain:

1. **Patch commutation** — files are line graphs; patches are
   `InsertAfter` / `Kill` / `Resurrect` ops. Disjoint patches commute.
   `three_way_merge` produces a valid graph + a list of
   `StructuredConflict::{ConcurrentInsert, EditVsDelete}`.
2. **CRDT** — when both sides share a yrs op history (live co-editing).
3. **Semantic** — tree-sitter (Rust/Python/TS) parses both sides;
   body-only hashing detects renames; `merge_text_file` surfaces
   `SemanticHint`s (rename, edit, add, remove, callsite-uses-old-name).

`FileMerge::explain()` renders the whole thing as a human/AI-readable
report.

### L6 — Sync & collaboration (`sync`, `mosaic-server`)

- **Bundles** — a `Bundle` is `{ changes, blobs, branch_advances,
  attestations }`, bincode-encoded. `missing_changes_for` computes the
  delta between two frontiers; `apply_bundle` verifies every signature
  and registers changes parent-first.
- **HTTP server** — REST for push/pull, branches, changes, diffs, review,
  issues; a web dashboard + per-change review pages + branch-graph SVG.
- **WebSocket** — `/ws/doc/:name` relays binary CRDT updates;
  `/ws/awareness/:name` carries presence; `/ws/signal/:name` brokers
  WebRTC SDP/ICE for peer-to-peer data channels.
- **Webhooks** — outbound POSTs on push for CI/chat/deploy.

### L7 — Interfaces

`mos` CLI · `mosaic-serve` · `mosaic-lsp` (+ VS Code extension) ·
`mosaic-mount` (FUSE) · `mosaic-mcp` (AI agents) · Rust/TypeScript/Python
SDKs.

## Data flow: an agent commits and pushes

```
agent edits file
  → mosaic-sdk Session.stage_text(...)
  → Session.commit()
      → ChangeBuilder signs a Change with the agent's session key
      → Repository.commit(): verify sig, write change bytes to CAS,
        register in DagIndex, bump vector clock
      → advance_branch(): recompute the branch Frontier
  → mos push origin
      → missing_changes_for(local, remote_frontier) → delta
      → build_bundle(delta) + the agent's Attestation
      → POST /api/v1/bundle
          → auth::Policy.authorize_bundle(): is the invoker key trusted,
            does the attestation cover each change?
          → protection::check_push(): branch rules (approvals, signed,
            no-force-push), path rules
          → apply_bundle(): verify, register, advance branch
          → webhooks::fire(Push { ... })
```

## Why a clean break from Git

Git's snapshot model fights every Mosaic goal: commutativity wants
patches, conflict-as-data wants structured residue, live editing wants
CRDTs, agent identity wants signed attestations. Rather than contort the
Git object model, Mosaic uses its own and ships read+write **bridges**
(`import git`, `export-git`) so teams can adopt incrementally.
