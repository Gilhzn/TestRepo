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

### Ergonomics gaps (#3–#5) — also fixed

3. **`mos clone` + branch-level merge — ADDED.** `mos clone <source> [dir]`
   clones into a fresh directory from either a bundle file (init + apply +
   checkout) or a remote URL (init + remote add + pull every branch +
   checkout). `mos branch merge <sources>... --into <target>` unions the source
   frontiers into the target and reports the auto-merge (reusing the merge-aware
   checkout), so combining parallel agents' branches is one command.
4. **`bundle create` default now carries branch refs.** A whole-repo
   `bundle create` (no `-b`) includes every branch advance, so a fresh repo that
   applies it can immediately `checkout` — no more silent zero-file seed.
5. **`commit -m` / `--message` alias + `add` lists files.** Git muscle memory
   works (`-m`/`--message` alias `--intent`/`-i`), and `mos add` now prints the
   staged paths.

Coverage: CLI integration tests (`mosaic-cli/tests/cli.rs`) drive the real `mos`
binary through clone-from-bundle and a two-branch `branch merge`. Bundle clone
and branch merge are verified end-to-end; network clone's non-network steps are
verified (a live end-to-end network clone wasn't run because the sandbox would
not keep a background server alive).

**Every finding from the dogfood run — both criticals and all three
ergonomics gaps — is now addressed.** Test suite: **370** (358 Rust + 6 TS +
6 Python).

---

## Round 2 (2026-05-22): re-run on the fixed flow

Re-ran the multi-agent scenario on the new ergonomics: a `calc` CLI, three
**real autonomous agents** cloning + editing + committing in parallel, then a
central merge. Goal — does the fixed flow *feel* good, and does it hold up?

### The flow feels good now

- `mos clone <bundle> wsX` is **one command** that yields a ready working tree
  (round 1 needed init + apply + checkout, with a silent zero-file footgun).
- All three agents drove `commit -m` / `add .` (now lists staged paths) with
  **zero guessing** and reported the flow as smooth and git-like.
- `mos checkout main` on the 3-tip frontier auto-merged: `ops.py` (parallel)
  clean, `calc.py` (concurrent) merged with both edits, conflict flagged.

### …but it surfaced one more real correctness bug (now fixed)

Two agents each added an `if`-dispatch block to the same spot in `calc.py`.
The line-union merge kept both `if` headers, but their **identical body lines**
(`print(...)`, `return 0`) had been deduped to a single copy (a side effect of
the round-1 crash fix) — leaving each `if` bodyless and the file
**syntactically broken** (`IndentationError`), even though the conflict was
correctly flagged.

**Root cause:** content-addressed line identity under a *single* creator fuses
identical lines from different edits into one vertex. Correct for "both added
`import os`"; wrong when the identical lines are bodies of distinct blocks.

**Fix:** the merge now mints each side's *new* vertices from a distinct creator
(`compile_session_salted`; both sides still anchor on the same base), so
identical lines on the two sides become two vertices and each block keeps its
body. Worst case is now a duplicated line (valid, trivially resolved) instead of
broken code. Re-running the scenario, the merged `calc.py` is valid and runs:
`add 3 4 → 7`, `sub 10 6 → 4`. Tests:
`merge_strategies::concurrent_blocks_with_identical_body_lines_stay_valid`,
`cli::branch_merge_keeps_concurrent_blocks_valid`.

### Open cosmetic nits (agent feedback, not fixed)

All three agents independently flagged: the bundle summary says "N changes / 1
branch advances" where N counts the whole branch history (confusing for "I made
one commit"); commit prints a 16-char hash with no indication it's a prefix; and
there's no `mos status` step prompting before commit. Cosmetic — left as-is.

Test suite after round 2: **371** (359 Rust + 6 TS + 6 Python).
