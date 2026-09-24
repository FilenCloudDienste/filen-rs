# The sync-engine scenario matrix

What each scenario is for, and what its number means.

This document is definitional — it should stay true whatever the numbers do.
There are no published figures to set beside it at present: `BASELINE.md` says
why the previous ones were withdrawn rather than patched.

## How to read any row

Every scenario records, in ONE process and ONE run:

- `yardstick_whole_pass` — a WHOLE pass over the same fixture, through the same
  `prepare`, taken once per SAMPLE and after the warmups. Quote it beside any
  figure from that scenario. Between builds at a million rows nothing here is
  trustworthy to better than ~10 %; the yardstick agreeing is what licenses a
  comparison at all. It is on the same footing as the pass it is divided into —
  same warmth, same sample count — because it used to be one cold unreplicated
  measurement and every published ratio divided a warm median by it.
- `total` — the measured pass. Each rep is timed on its own clock; the sample
  sums them and divides by `reps`, which every record carries.
- `step:<name>` — the engine's own phase boundaries, from inside the real pass.
  The ordered list is asserted against `SCOPED_STEPS` / `WHOLE_STEPS` on EVERY
  pass, because a renamed phase is silently dropped from both sides of a diff.
- `unattributed` — total minus every step. The table closes per sample, asserted.
- `harness_overhead` — the harness's own per-pass work: the announcement, and
  also the assertions, the marks clone and the step merge. NOT part of `total`,
  and not a model of what a real watcher's announcement costs.
- `marks` — how many step marks the pass ran (a count, not a time). A CHANGE-SCOPED
  pass reports 19; a WHOLE pass reports 4, because the engine's phase marks live
  in `prepare_scoped` and only `pass_inputs` is on both paths. So a whole-read
  scenario's breakdown is coarse by construction: nearly all of it lands in
  `prepare_tail`. That is a property of where the marks are, not of where the
  time goes.
- `plan:actions` / `plan:held` / `plan:conflicts` / `plan:dir_moves` — what the
  pass actually planned, so a reader can see the scenario did work without
  taking its expectation on trust.

A single step's cost is only meaningful as an IN-RUN delta between phases of one
process. Sub-0.01 ms steps move ±24 % run to run; do not read a percentage on one.

## Why every scenario asserts a plan

Each row pins four numbers — `actions`, `held`, `conflicts`, `dir_moves` — plus
WHICH read the pass must perform and, for a whole read, exactly why. A benchmark
that quietly stops doing work reports itself fast, and an action count alone
cannot see the difference between:

- a mass delete whose deletions were all HELD by the guard, and a pass that
  planned nothing — both report `actions: 0`;
- a directory move FOLDED into one move carrying its subtree, and one that
  re-uploaded every child — these differ only in `dir_moves`.

`validate()` runs on every scenario a run chooses, not only under `cargo test`,
and rejects any definition that a pass planning nothing whatsoever would satisfy.
The idle floor is the single exemption, and it must expect exactly nothing.

## Group 1 — change classes (balanced 20/3/73 ASCII tree)

| Scenario | What it is for |
|---|---|
| `twoway_idle_*` | The floor: what the machinery costs with an empty changelist. The number to beat for a watch wake that has nothing to do. |
| `twoway_one_file_*` | One changed file — the case change-scoping exists for. Read against the yardstick: the ratio is the whole value of the design. |
| `twoway_one_percent_*` | A realistic working session. The class that refused to collapse when others did, which is why the last round's headline was believable. |
| `twoway_ten_percent_*` | Ten times the change, still under the changelist cap. Says whether scoped cost grows with the change or with the tree. |
| `twoway_one_percent_scattered_10k` | Same count of edits, maximally different locality — one per directory instead of clustered. Isolates locality from volume against the row above. Meaningful only because `prepare_bed` sorts the baseline rows into path order first: in `HashMap` order both rows drew the same random permutation and the pair measured nothing. |
| `twoway_small_delete_10k` | Ten deletions: under the guard's floor, so they are PLANNED. The control for the mass delete below. |
| `twoway_mass_delete_10k` | Sixty per cent deleted. Past the guard's limit (half the tracked set) — and past the changelist cap (a quarter) on the way, which is why a guard-tripping delete is necessarily a WHOLE pass on a watched pair. Asserts `actions: 0` AND `held: one per deleted file`, so "the guard held everything" cannot read as "the pass did nothing". |
| `twoway_rename_storm_10k` | Renames in place, announced as the two-ended event a watcher reports. |
| `twoway_dir_move_*` | One directory renamed, carrying its subtree. The case that cost 63 seconds before it was narrowed. Pins `dir_moves`, so a re-upload of the subtree cannot pass as a fold. |
| `twoway_both_changelists_10k` | The only scenario whose REMOTE changelist is non-empty: one announced remote upsert per local edit, so `observe_remote` is priced against a real delta rather than an empty list. It is NOT a conflict scenario — see below. |
| `twoway_first_sync_*` | No baseline at all. Reads both sides whole for `EmptyBaseline` and plans the ENTIRE tree (`actions: one per node`). The only row that hashes the whole tree — see what is not covered, below. |

## Group 2 — shapes (10k nodes, 100 edits)

Every row changes the same NUMBER of files, control included, so what varies
across the group is the tree. A per-mille does not work here: the shapes hold
different file populations (10,240 balanced, 4,096 deep-narrow, 10,000
wide-flat), so one per cent of each is 102, 40 and 100 edits, and the group used
to invite reading a 2.1x difference as geometry when most of it was volume. A
unit test holds every row to the same change class.

What still differs besides the geometry, and cannot be held fixed: the NODE
count, because the shape rounds up to a whole branching factor (balanced 10,824,
deep-narrow 12,286, wide-flat 10,001), and with it the directory fraction — 67 %
of a deep-narrow tree is directories against 5 % of a balanced one. Every record
carries the nodes its fixture held; read the rows per node as well as per pass.

| Scenario | What it is for |
|---|---|
| `twoway_balanced_100_edits_10k` | The control: 100 edits on the 20/3/73 ASCII tree every figure recorded before this harness existed was taken on. |
| `twoway_wide_flat_100_edits_10k` | One directory holding every file: where the per-directory children vector's O(children) insert and the folded-order comparison are worst. |
| `twoway_deep_narrow_100_edits_10k` | Twelve levels of binary branching: what ancestor walks and path materialisation pay for. |
| `twoway_long_paths_100_edits_10k` | Names padded to 100 bytes a segment, so paths run past 300 characters. |
| `twoway_unicode_100_edits_10k` | Mixed-case non-ASCII names, including the pairs `collision_key` exists for — `char::to_lowercase` is nearly free for lowercase ASCII and allocates per character otherwise. |

## Bytes rather than nodes

| Scenario | What it is for |
|---|---|
| `twoway_large_files_50_edits_512` | A megabyte a file, so re-hashing a CHANGED file dominates instead of vanishing. Deliberately small in nodes: this shape costs half a gigabyte of disk. NOT part of the shape group — 530 nodes and 50 edits — and not comparable with it. `edit_files` rewrites at the declared size and `validate_fixture` asserts that on disk, because this row shipped once measuring 32-byte files: `fs::write` truncates, and every guard here looked at the plan, which was identical either way. |

## Group 3 — modes (10k)

What a pair's DIRECTION costs. Not the `RuleFiles::Only` scan, whatever an
earlier version of this document said: `rule_file_rows()` iterates the rule-file
index, one entry per `.filenignore`, and these fixtures hold none — the phase
measures a few hundred nanoseconds on every mode row, and a pull-only idle pass
comes out cheaper than a two-way one. Pricing that scan needs a fixture with
rule files in it, which is listed below as not covered.

| Scenario | What it is for |
|---|---|
| `pull_idle_10k` | The floor for a pair whose remote is authoritative. |
| `pull_one_file_10k` / `pull_one_percent_10k` | The same edits on a pair where the remote is authoritative. |
| `backup_local_one_percent_10k` | Pushes, never deletes. |
| `backup_remote_one_percent_10k` | Pulls, never deletes. |

## Group 4 — sizes

The three headline classes at 1k, 10k, 100k and 1M. A figure is only ever read
per node against the tree it was taken on, which is why every record carries the
node count the fixture actually held.

## What this matrix does NOT cover

Stated plainly, because an uncovered case that looks covered is worse than a gap.

- **Conflict-heavy is not covered.** `twoway_both_changelists_10k` diverges both
  sides — it edits files locally and announces a remote upsert for each, carrying
  a content matching neither the baseline row nor the local edit. Measured at 10k
  it plans **102 actions and 0 conflicts**. Announcing a remote change through
  `note_owed_remote` does not by itself make the reconcile call those paths
  both-sides-changed. The scenario was renamed rather than left promising
  conflicts it does not produce, and its zero conflict count is PINNED so the gap
  stays visible instead of drifting. A real conflict scenario needs a way to
  diverge the remote side that the reconcile actually reads as a remote change;
  that is unfinished work, not a measurement.
- **Mass delete is only covered as a whole pass.** On a watched pair a
  guard-tripping delete necessarily overflows the changelist first (the cap is a
  quarter of the tree, the guard's limit half), so "a scoped pass that trips the
  guard" does not exist to be measured.
- **No first-sync-against-a-populated-destination**, no partially-ignored tree,
  and no scenario exercising `.filenignore` rule files — a rule-file change
  collapses the pass to a whole read by design, so it is a different measurement.
  This is also why group 3 cannot price `RuleFiles::Only`.
- **Content hashing is priced by two rows only.** `scan::fast_path_hash` reuses
  a baseline row's hash while `(size, mtime)` still match, on the scoped path and
  the whole one alike, so on a converged fixture nothing is re-hashed except what
  a change class actually rewrote. That leaves `twoway_first_sync_*` (no
  baseline, so the whole tree is hashed) and `twoway_large_files_50_edits_512`
  (50 MiB of it). Every other row in the matrix, yardstick included, hashes at
  most 73 bytes a changed file.
