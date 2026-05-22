# Dogfooding Mosaic — real multi-agent run (2026-05-22)

A small, honest field test: run a real project with multiple agents committing
in parallel through Mosaic, then merge. Goal — find where Mosaic genuinely
beats Git and where it hurts today. Nothing here is theoretical; every claim
has a reproduced command behind it.

## Setup

- **Project:** a tiny Python `todo` CLI — `todo.py` (command dispatch),
  `store.py` (JSON persistence), `README.md`.
- **Actors:**
  - `agent-a` — implemented the `add` command in `todo.py`.
  - `agent-list` — implemented the `list` command in `todo.py` (**same
    function/region as `agent-a`** → genuine concurrent same-file edit).
  - `agent-store` — implemented `store.py` (**independent file** → clean
    parallel work).
  - `agent-list` and `agent-store` were run as **real autonomous sub-agents**,
    each in its own repository, each committing via `mos`.
- **Topology:** one base commit, three repos seeded from a base bundle, each
  agent commits on `main`, bundles shipped back to a central repo and applied.

## Where Mosaic wins over Git (verified)

1. **Parallel work is never rejected.** Three agents committed on `main`
   independently; applying all three bundles produced a **3-tip frontier**:
   ```
   $ mos branch show main
   28b9171708b70526…   (agent-store)
   96e34e671d5e37eb…   (agent-list)
   b8ad5ddf50825f4c…   (agent-a)
   ```
   Git would reject two of these as non-fast-forward and force a rebase/merge
   dance. Mosaic just stores the divergence as data.

2. **Clean parallel merges across files actually work.** `agent-store`'s
   `store.py` and the others' `todo.py` merged with zero ceremony — after
   `mos checkout main`, `store.py` contained the full JSON implementation
   *and* `todo.py` was present. Independent-file parallelism is effortless.

3. **Conflict-as-data is real (for disjoint edits) and the UX is genuinely
   nice.** A 3-way merge of two disjoint concurrent inserts:
   ```
   $ mos merge --base b.txt --ours o.txt --theirs t.txt --explain x.txt
   --- merged result ---
   base line 1
   aaa_unique_ours
   zzz_unique_theirs
   base line 3
   --- end ---
   Outcome: merged graph is valid (acyclic, deterministic order) but carries
   structured conflicts. The repository is NOT wedged — these are data your
   agent or reviewer can resolve.
     • concurrent insert at anchor 87347369: ours=2aa2a891, theirs=a02b6509
   ```
   No `<<<<<<<` markers; a machine-readable conflict an agent can resolve. This
   is the headline promise, and for this case it delivers.

4. **Full provenance.** Every change is ed25519-signed and carries its author
   identity (`agent-store`, `agent-list`, the human lead) — real attribution
   and audit, out of the box.

## Where it hurts today (verified)

1. **`checkout` silently drops concurrent same-file work — the biggest gap.**
   `agent-a` and `agent-list` both edited `todo.py`'s command dispatch.
   After `mos checkout main` on the 3-tip frontier:
   ```
   $ grep -E 'cmd == "(add|list)"' todo.py
       if cmd == "add":          # agent-list's "list" command is GONE
   ```
   The `list` change is still in the DAG, but the working tree shows only one
   tip's version — **no merge, no conflict, no warning.** An agent or human
   would not know work was hidden. The auto-merge engine is **not wired into
   the everyday `checkout` flow**; checkout just picks a tip.

2. **`mos merge` crashes on identical lines on both sides (real bug).** When
   both sides insert the same line (extremely common — `return 0`, `}`,
   `import os`, a blank line), the merge aborts:
   ```
   $ mos merge --base b.txt --ours o2.txt --theirs t2.txt --explain x.txt
   error: invalid patch: vertex Hash(a3c510a22c4f64ec) already exists
   ```
   Root cause (hypothesis): the CLI builds both sides' line graphs from a
   single creator, so an identical inserted line derives a **colliding
   `VertexId`** and the second insert fails to apply. The library unit tests
   use a *distinct* creator per side, which masks this — so the test suite is
   green while a common real-world merge crashes.

3. **No `mos clone`, no `mos branch create`, no branch-level merge.** Seeding a
   second working repo is a manual `init` + `bundle apply` + `checkout`. There
   is no command to merge two branches/changes; `mos merge` operates on **three
   files on disk** (`--base/--ours/--theirs <path>`), decoupled from the DAG.
   To combine two agents' work you must manually export each file version and
   know which files diverged.

4. **`bundle create` footgun.** The default `mos bundle create -o x` reports
   `0 branch advances` and produces a bundle that, when applied, loads changes
   into the DAG but **creates no branch ref** — so `mos checkout main` yields
   *zero files*. You must remember `-b main`. The default makes a useless seed.

5. **CLI ergonomics vs Git muscle memory (agent feedback).** Both autonomous
   agents independently flagged: `commit -i` instead of git's `-m` (and `-i` is
   *interactive* in git — actively misleading); `add` doesn't name staged
   files; the bundle "2 changes" count for one edit is confusing. None blocked
   them (commands were given verbatim), but an agent guessing from Git
   knowledge would stumble.

## Verdict

The **core thesis holds**: divergent parallel authorship as first-class data,
signed provenance, and conflict-as-data for disjoint edits all work and feel
better than Git. The **storage/DAG/merge engine is sound** (368 tests).

The gap is **product plumbing, not architecture**. Today an agent fleet can
commit in parallel safely, but the "they all merge cleanly and automatically"
payoff is not delivered end-to-end at the CLI: `checkout` hides concurrent
edits, branch-level merge doesn't exist as a command, and the file merge tool
crashes on common identical-line cases.

### Recommended next steps (in priority order)

1. **Fix the identical-line merge crash** (use a distinct synthetic creator per
   side in the `mos merge` CLI path, or de-dupe colliding `VertexId`s). This is
   a correctness bug in the headline feature.
2. **Wire the merge engine into `checkout`** of a multi-tip frontier: flatten
   by running `three_way_merge` across tips and surface structured conflicts,
   instead of silently picking one tip.
3. **Add `mos clone` and a branch-level `mos merge <branch> --into <branch>`**
   so the distributed flow doesn't require manual bundle plumbing.
4. **Default `bundle create` to include current branch refs.**
5. Alias `commit -m`, and have `add`/`bundle` report clearer counts.

Until #1 and #2 land, hold off on wide distribution — the first thing a new
user does is exactly what broke here (two agents edit one file). Fixing them is
days of work, not a rearchitecture.

---

## Fixes landed (2026-05-22, follow-up)

The two **critical** findings (#1 and #2 above) are now fixed and regression-tested.

1. **Identical-line merge crash — FIXED.** `three_way_merge` now treats an
   insert whose content-addressed `VertexId` already exists (the same line
   added on both sides) as idempotent and drops the duplicate before applying,
   instead of aborting. Identical concurrent additions dedupe to one line;
   differing ones still surface as a `ConcurrentInsert`. The previously-crashing
   `mos merge` now yields a clean result with `shared_line` kept exactly once.
   Tests: `merge_strategies::identical_line_on_both_sides_does_not_crash_and_dedupes`,
   `identical_block_on_both_sides_merges_to_single_copy`.

2. **`checkout` silently dropping concurrent same-file work — FIXED.**
   `branch_snapshot` is now merge-aware: on a multi-tip frontier it materializes
   each tip independently and 3-way merges divergent files (union, against the
   common-ancestor base) instead of last-writer-wins. `checkout` returns
   `MergeNote`s and the CLI prints e.g.
   `merged 2 file(s) across parallel branch tips (no work dropped)` and points
   at `mos resolve` for any conflicts. Re-running the exact dogfood scenario,
   `todo.py` now contains **both** `add` and `list`. Binary/non-UTF-8 files keep
   one tip's bytes and are flagged for manual review. Test:
   `working_copy::checkout_merges_concurrent_same_file_edits_across_tips`.

The remaining gaps (#3 `mos clone`/branch-level merge, #4 bundle-default branch
refs, #5 `commit -m` alias) are ergonomics, not correctness, and are still open.
Test suite: **368** (356 Rust + 6 TS + 6 Python).
