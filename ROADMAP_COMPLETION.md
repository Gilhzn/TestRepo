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
- ⬜ **Conflict resolution** — `mos resolve <change>` to inspect structured
  conflicts and pick a side / write a resolution change; web UI affordance.
- ⬜ **Hooks** — pre-commit / post-commit / pre-push hook execution from
  `.mosaic/hooks/`.

## M9 — Advanced engine

- ⬜ **Full GumTree** — proper AST tree-diff with move/copy detection,
  replacing the body-hash rename heuristic (M5's research-grade item).
- ✅ **`mos bisect start/good/bad/status/reset`** — binary-search the change
  DAG for a regression, state persisted in `.mosaic/bisect.json` (6 tests).
- ⬜ **Server push notifications (SSE)** — `/api/v1/events` server-sent
  stream so clients learn of pushes without polling.
- ⬜ **Packfiles** — pack many loose changes/blobs into a single
  compressed file for efficient bulk transfer + cold storage.

## Non-code (tracked, out of engine scope)
- ⬜ Dogfooding with a real team · Hosting / "Mosaic Cloud" · external
  security audit + SOC2/ISO · community + reference customers + pricing.

---

## Current test count: **329 / 329 passing** (317 Rust + 6 TS + 6 Python)
