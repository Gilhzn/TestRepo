# Mosaic — "Winning Product" Completion Roadmap

The 8-milestone architecture roadmap (M0–M7) is essentially complete: see
the bottom of this file / git history. This document tracks the remaining
**code-buildable** gaps that separate "complete architecture" from
"daily-driver product a team would actually live in." Everything here is
implementable; the non-code gaps (dogfooding, hosting/SaaS, external
security audit, GTM) are tracked separately and are out of scope for the
engine itself.

Legend: ✅ done · 🔨 in progress · ⬜ todo

---

## M8 — Daily-driver completeness

Features every developer reaches for that Mosaic doesn't have yet.

- ✅ **`mos blame <file>`** — per-line attribution via similar-diff over the
  topo-ordered change history (6 tests).
- ✅ **`mos stash push/pop/apply/list/drop`** — shelve working-tree changes,
  reset to tip, restore later (5 tests).
- ✅ **Tags & releases** — `mos tag create/list/show/delete`; lightweight or
  ed25519-signed annotated tags (6 tests).
- ✅ **Reflog** — `mos reflog` over an append-only ref-movement log (6 tests).
- ✅ **Short-hash prefixes** — every change-id arg resolves Git-style
  prefixes against the repo (ambiguity-checked).
- ✅ **Conflict resolution** — `mos resolve <file> --base --ours --theirs
  --strategy ours|theirs|union`; `MergeResult::resolve` kills the losing
  side's vertices and re-flattens (1 test).
- ✅ **Hooks** (`mosaic-core::hooks`) — pre-commit/post-commit (+ generic
  pre-/post-) from `.mosaic/hooks/`; pre-hooks gate, post-hooks advisory,
  env context; wired into `mos commit` (6 tests).

## M9 — Advanced engine

- ✅ **Full GumTree** (`mosaic-core::gumtree`) — AST tree-diff with
  Match/Insert/Delete/Update/Move classification + `hints_from_script`
  bridge to SemanticHint (8 tests).
- ✅ **`mos bisect start/good/bad/status/reset`** — binary-search the change
  DAG for a regression, state persisted in `.mosaic/bisect.json` (6 tests).
- ✅ **Server push notifications (SSE)** — `GET /api/v1/events` server-sent
  stream; post_bundle publishes a `{type:push,branch,tips,applied}` event to
  every subscriber (1 integration test).
- ✅ **Packfiles** (`mosaic-core::pack`) — whole-repo zstd-19 archive with a
  hash→(kind,offset,len) index; `mos pack create/restore/inspect` (5 tests).

**M8 + M9 are now complete.** Remaining gaps are all non-code.

## Non-code (tracked, out of engine scope)
- ⬜ Dogfooding with a real team · Hosting / "Mosaic Cloud" · external
  security audit + SOC2/ISO · community + reference customers + pricing.

---

## Current test count: **355 / 355 passing** (343 Rust + 6 TS + 6 Python)
