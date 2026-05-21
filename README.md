# Mosaic

> The version control system built from scratch for the agent era.

Git was designed in 2005 for a small team of humans mailing each other
patches. Its mental model — snapshots, linear history, brittle three-way
merge — generates brutal friction when ten AI agents and three humans
work in parallel on the same code. **Mosaic was designed for the world
we now live in.**

```bash
mos quickstart --email you@your-company.com
# 5-step wizard from zero to your first commit
```

## Four pillars

1. **Mathematically clean parallel merges.** Files are line graphs;
   patches are commutative ops. Two changes touching disjoint regions
   merge identically in any order. The repo is provably never wedged —
   leftover conflicts surface as structured data, not `<<<<<<<` markers.
   [Property-tested across 32 random op sequences.]

2. **Real-time co-editing.** yrs-backed CRDT working copy. Two peers —
   human, agent, or a mix — edit the same file simultaneously and
   converge byte-identically. WebSocket relay + WebRTC signaling
   included. [Property-tested across 16 concurrent-edit scenarios.]

3. **Verifiable agent identity.** Sigstore-style attestation chain. A
   human's long-term ed25519 key signs an attestation pinning an agent's
   session key to its claimed identity + validity window. The server
   enforces it on every push. The audit log records every signed event
   per session for compliance replay.

4. **Semantic merge that understands code.** tree-sitter parsers for
   Rust, Python, TypeScript. Mosaic recognizes when one branch renames a
   function and another edits its body — and propagates the rename.
   `mos merge --explain` prints the reasoning in plain English.

## What you get

| Surface             | Binary / package                    | Purpose                                              |
| ------------------- | ----------------------------------- | ---------------------------------------------------- |
| CLI                 | `mos`                               | Daily driver — init/commit/status/diff/log/push/...  |
| HTTP sync server    | `mosaic-serve`                      | Multi-machine sync, dashboard, review pages          |
| LSP server          | `mosaic-lsp`                        | Editor integration via Language Server Protocol     |
| VS Code extension   | `mosaic-vscode`                     | Native palette commands + status bar                 |
| FUSE mount          | `mosaic-mount`                      | Browse any branch as a read-only filesystem        |
| MCP server          | `mosaic-mcp`                        | Direct AI agent integration (Claude Code etc.)      |
| Rust SDK            | `mosaic-sdk` (crate)                | In-process; sessions, speculation, attestation     |
| TypeScript SDK      | `mosaic-sdk-ts` (npm)               | Over-HTTP client + WebSocket live editing          |
| Python SDK          | `mosaic-sdk-py` (pypi)              | Stdlib-only HTTP + optional WebSocket               |

## 5-minute tour

```bash
# 1. One-command setup
mos quickstart --email you@team.com --name "You"

# 2. Daily workflow (Git muscle memory works)
mos status
mos add .
mos commit -i "first change"
mos log

# 3. Open a sync server (any machine the team can reach)
mosaic-serve --repo /var/repos/team --bind 0.0.0.0:7700
# → http://server:7700/         landing page
# → http://server:7700/dashboard branch graph + change history

# 4. From any teammate's machine
mos remote add origin http://server:7700
mos trust me                         # share my pubkey with the admin
mos push origin
mos pull origin

# 5. Code review
mos comment <change> --body "looks great"
mos approve <change>
mos request-changes <change> --body "rename chargeCard?"
mos review <change>                  # threaded log

# 6. Branch lifecycle (jj-style)
mos undo                             # back out the last commit
mos amend -i "rephrase"              # rewrite the tip
mos squash                           # collapse tip into parent
mos abandon old-branch

# 7. Browse history as a filesystem
mosaic-mount --repo . --branch main /mnt/preview
grep -r "TODO" /mnt/preview

# 8. Semantic merge with explanation
mos merge file.rs --base base.rs --ours a.rs --theirs b.rs --explain

# 9. Mirror back out to a Git remote
mos export-git /path/to/git/clone

# 10. Compliance: replay every action an agent took
mos audit session <agent-session-id>
mos audit actor human:alice@team.com
```

## Three demonstrations of the thesis

```bash
mos demo                             # math: parallel patches commute
node mosaic-sdk-ts/dist/examples/two-agents.js
                                     # two LiveSession agents in real time
mos merge file.rs --base ... --explain
                                     # semantic merge with rename detection
```

## Architecture in one screen

```
+-----------------------------------------------------------+
|  L7 Interfaces     CLI · LSP · MCP · TS/Py SDK · Web UI   |
+-----------------------------------------------------------+
|  L6 Sync & Collab  HTTP push/pull · WebSocket · WebRTC    |
|                    Awareness · Signaling · Webhooks       |
+-----------------------------------------------------------+
|  L5 Merge Engine   Patch commutation → CRDT → Semantic    |
|                    Conflict-as-data resolver              |
+-----------------------------------------------------------+
|  L4 Change Model   Patch (canonical) + CRDT ops sidecar   |
|                    + AST delta + Attestation chain        |
+-----------------------------------------------------------+
|  L3 History DAG    Vector-clock change graph              |
|                    Branches = named frontiers (jj-style)  |
+-----------------------------------------------------------+
|  L2 Content Model  Text = CRDT doc · Binary = CDC chunks  |
|                    Trees = hash-addressed manifests       |
+-----------------------------------------------------------+
|  L1 Storage        CAS (BLAKE3 + zstd) · S3-compatible    |
|                    HttpCas · TieredCas · FUSE virtual fs  |
+-----------------------------------------------------------+
```

## Status

| Milestone                  | State |
| -------------------------- | ----- |
| M0 — Storage spine         | ✅    |
| M1 — Change DAG + patches  | ✅    |
| M2 — Sync + Git interop    | ✅    |
| M3 — CRDT integration      | ✅    |
| M4 — Real-time collab      | ✅    |
| M5 — Semantic merge        | 🔨 (research-grade GumTree remains)         |
| M6 — Agent SDK             | ✅    |
| M7 — Scale + forge         | 🔨 (public beta = business decision)        |

**259 unit tests** across the workspace (247 Rust + 6 TypeScript + 6
Python), 0 failures. See `ROADMAP.md` for the live status checklist.

## How to compare against Git / jj

| Capability                                  | Git  | jj   | Mosaic |
| ------------------------------------------- | ---- | ---- | ------ |
| Mathematically commutative merges           | ❌   | ❌   | ✅     |
| Conflict-as-data (never wedged repo)        | ❌   | ✅   | ✅     |
| Real-time co-editing                        | ❌   | ❌   | ✅     |
| Semantic merge with rename detection        | ❌   | ❌   | ✅     |
| Cryptographic agent identity chain          | sigs | sigs | ✅     |
| Native MCP server for AI agents             | ❌   | ❌   | ✅     |
| Read-only virtual filesystem                | EdenFS only | ❌ | ✅ |
| S3-compatible object-store backend          | LFS  | ❌   | ✅     |
| Per-session audit replay                    | ❌   | ❌   | ✅     |
| Multi-language SDK (Rust/TS/Python)         | ❌   | ❌   | ✅     |

## License

Apache-2.0 OR MIT, your choice.
