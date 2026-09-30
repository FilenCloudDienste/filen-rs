# The sync-engine scenario matrix

What each scenario is for, and what its number means.

This document is definitional — it should stay true whatever the numbers do.
The figures to set beside it live in `BASELINE.md`, which carries the landing
sweep of the store round (`run_p`) and its alternated 1M comparison with the
resident tree (`run_q`).

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
process. Sub-0.01 ms steps move far more between runs than between builds; do not
read a percentage on one. `BASELINE.md` carries the measured run-to-run resolution
of every published column, taken from two whole runs at one commit.

`compare`'s `(within spread)` marker is a NOISE-FLOOR annotation and not a gate: it
asks whether two medians sit inside each other's in-run range, which at three
samples most rows do, including rows whose medians are tens of per cent apart. The
gate is the line above it — a scenario whose whole-pass yardstick moved more than
ten per cent between the two runs is marked `MACHINE MOVED`, and every percentage
under it carries the machine as well as the code. The gate is only a gate while the
WHOLE pass's code is the same on both sides: between two builds that changed the
whole pass itself (the store round did — a whole pass now reads every row from the
table), a moved yardstick is the code as much as the machine, and only an
alternated same-session run can tell the two apart.

`compare`'s header prints both runs' `mark_overhead_ns`: one step mark timed through
`mark` itself, so it is a second, free reading of how fast the machine was answering
at all. It GATES nothing — the gate is the yardstick above it — and it carries no
calibrated threshold, because the figure has not been pinned down: two whole sweeps
of one binary sat 4.9 % apart (25.2 and 26.4 ns), five back-to-back runs
of one binary on an idle machine spanned 5.8 % (23.5 to 24.9 ns), and observations
taken during review of this harness spanned 12 % and 16 %. Read a LARGE gap as a
reason to distrust a diff the yardstick passed; do not read a small one as a reason to
trust it, and do not threshold on it until someone measures what quiet looks like on
the machine in question.

## How to read any memory row

Every scenario also records what its pass costs in MEMORY, measured in FRESH
PROCESSES (`SYNC_BENCH_MEM_SAMPLES`, default 2, `0` skips). A child opens the
tree its parent already built, loads the pair, runs the scenario's own pass under
the same assertions, and samples its own resident set. An in-process figure
cannot answer an absolute question — by the time the timing samples are done this
process has run warmups, three samples and three whole yardstick passes, and what
the allocator kept is still resident — so no memory figure is taken here.

The names say whose process a figure is and how it was arrived at, because the
misquoting these replace came from names that did not:

- `mem:fresh_process_floor_rss` — the child before it opened anything: the binary
  and the allocator's first pages. The tokio runtime is built AFTER this is taken,
  so its resident cost is part of what the next figure adds, not part of this one.
  Subtract the floor to talk about the engine.
- `mem:fresh_process_pair_loaded_rss` — the pair's baseline loaded into the store
  the engine keeps it in. This is the steady state between passes, and the figure a
  steady-state target is read off. "And nothing else" is not quite true and the
  difference is unmeasured: the child opens its engine on a registry that already
  holds the pair, so `SyncEngine::open` tries to subscribe it to the cache, which
  starts a cache worker and a second connection to the fixture's cache DB before
  the subscription is refused. The parent never does this — it opened on an empty
  registry — so that cost sits inside this child's `pair_loaded − floor` and inside
  every figure taken after it.
- `mem:fresh_process_pass_widest_rss` — the widest of the engine's own step
  boundaries during the pass, excluding `drop_pass`, which is marked once the plan
  and both sides are already freed. It is not a continuous sampler: a spike inside
  one step is not in it. At small sizes this, `peak` and `after_pass` can print the
  same number — that is the finding (a resident set that does not fall), not a
  formatting artefact.
- `mem:fresh_process_pass_widest_over_floor_rss` and
  `mem:fresh_process_pass_widest_over_pair_loaded_rss` — that same widest point,
  measured from the two floors worth measuring it from. The first includes
  opening the engine and loading the pair, neither of which a pass pays again;
  the second is what the PASS added on top of an already-loaded pair, and is the
  figure a per-pass memory target is read off. Since the store round, "loading the
  pair" holds no rows: it opens the store's reader and counts, so this second figure
  now includes every page and row a pass reads, which the resident tree used to
  have paid for before the pass began.
- `mem:fresh_process_peak_rss` — `getrusage`'s high-water mark for the whole
  child. The transients of BUILDING what the pass held sit between this and
  `widest`, which is why the two differ and neither is the other.
- `mem:fresh_process_after_pass_rss` — the pass dropped, the engine still open:
  what an idle engine sits on, including pages the allocator kept rather than
  returned.
- `mem:fresh_process_after_everything_dropped_rss` — engine and store gone too.
  The gap from `floor` is retention, not a structure.
- `mem:fresh_process_rss_at_step:<step>` — the resident set at each boundary the
  engine marked, in order. It is the same pass in the sense that matters — the same
  `one_pass`, the same read kind, the same plan and the same ordered step list — but
  not the same EXECUTION: the timing figures come from a hot process after warmups
  and up to 32 reps, and this is one cold pass in a fresh one. Read the two profiles
  as the same shape, not as one run.
- `mem:pair_baseline_computed_bytes`, `mem:pass_{baseline,view,scan}_computed_bytes`
  and `mem:pass_sides_computed_bytes` (view + scan) — what those structures say they
  cost, summed from their own capacities. A COMPUTED figure, never a measured one:
  it can exceed a resident set, because capacity is allocated without necessarily
  being faulted in. The BASELINE terms (`mem:pair_baseline_term_handle` /
  `_edits_computed_bytes`) are the snapshot handle — a pooled reader connection,
  its pair and its counts, 272 bytes, not counting the pages its questions last
  read (up to 1,024 rows each, dropped with the pass) — and the pass's own edits
  over the table:
  its folded directory moves and confirmed pushes, or the rows frozen for its
  apply. Between passes the pair is the handle and nothing else, at every size;
  the store holds no rows in memory. A run asserts that the pass's figure is at
  least the pair's, and that its edits stay within `EDITS_BASE_BYTES` plus
  `EDIT_BYTES` per folded move or confirmed push, so a fold that wrote the rows it
  carries fails the run instead of printing a large widest point. (Before the store
  round these columns were the resident tree's capacities; the two are not
  comparable, and the old terms print as ONLY IN BEFORE.)
- `walk:baseline_read_statements` / `walk:baseline_read_rows` — what the measured
  pass asked the store for its rows, counted in the snapshot: statements run and
  rows decoded. Three statements and no rows for an idle pass, eight for one changed
  file; a scoped pass that decides many paths asks about them in path order and
  reads them a page at a time, so the count follows how the paths cluster:
  `twoway_one_percent_100k` runs 745 statements for its 982 changed paths,
  `twoway_top_dir_move_100k`, whose moved directory holds half the pair, 848.
  A directory move's rows are bounded the way its edits are: each move scenario
  declares the rows its pass may read per file the move carries
  (`read_rows_per_carried_file`), set at the multiple it measured rounded up, so a
  pass that reads the moved subtree once more fails the run. It is 8 for
  `twoway_top_dir_move_100k` (350,072 rows for 50,000 files), and 20, 30 and 37 for
  the leaf moves at 10k, 100k and 1M, set when they read 393, 582 and 738 rows for
  their twenty files; they read 329, 518 and 674 since a directory move proven to
  carry its subtree unchanged stopped deciding it. About half of a leaf's rows are
  the page the fold's cursor reads ahead past the leaf: its first walk of the
  pass's paths reads 190 of the 329 at 10k, for twenty files and their directory.
  `pass_view` is zero for every scenario whose remote changelist is empty, which is
  every scenario here but `twoway_both_changelists_10k`: a carried side owns nothing
  it was not told about. Zero is an answer, which is why that column is printed in
  kibibytes — in mebibytes 411 B and 0 B both print `0.000`.

`summarize` prints TWO attribution ratios, because one answered neither
question. `pair_attributed` is what the pair's baseline computes itself as
over what LOADING it added (`pair_loaded − floor`); the remainder is the
engine's fixed cost — SQLite's page cache and mapped pages, the runtime, the
client. Since the store round the baseline is a 272-byte handle, so this ratio
is ~0 % by design: loading a pair costs the engine's fixed cost and nothing
that scales. `pass_attributed` is what the pass's two SIDES compute themselves as
over what the PASS added on top of an already-loaded pair
(`widest − pair_loaded`); the baseline's rows are not in it, because the pass
reads them from the table rather than holding them.

A change-scoped pass's sides are CARRIED — they hold only what the pass
observed and derive the rest from the baseline rows — so a small
`sides_computed` is the design working, not an accounting failure. Sizing them
as though they were materialised trees would charge a scoped pass for two more
copies of a tree it exists not to hold.

Neither ratio is expected to reach 100 %, in either direction: a map's
`capacity` is allocated without necessarily being faulted in, so a computed
figure can exceed a resident one. What is left over is the plan, the facts, the
overlays' transients and whatever the allocator kept rather than returned. The
columns are there to show the SIZE of what nobody has accounted for, which is
the only honest thing to publish until something accounts for it.

Every RSS figure is the allocator's answer as much as the engine's, so each run
stamps which one it used, and `widest_spread` (that row's max over its min across
its memory samples) is the resolution the row has WITHIN that run. At the default
two memory samples that is a max over a min of two, which can say whether a figure
moves and cannot describe a distribution; and a figure's movement between two runs
is larger than its movement inside one. `BASELINE.md` carries the measured
run-to-run figure, and that is the one to compare against.

## What the harness cannot run on

Windows. `PairChanges::new` marks a pair `LocalEventsDegraded` there, so every pass
falls back to a whole read and every change-scoped scenario fails its read-kind
assertion — in the parent and in the memory children alike. Nothing in the harness
checks for this; it simply fails. Every figure published here was taken on macOS.

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

`validate_fixture` then checks the tree ON DISK, at every end the change class has:
each edited file at the declared size, each announced deletion actually gone, both
ends of each rename (the source gone, the destination there and — for a file — at
the declared size), and ONE file the class did not touch, also at the declared size.
That last one is the population a pass spends most of itself on, and it used to be
checked by node count alone.

One untouched file rather than all of them, deliberately: a stat per node is a walk
of the whole tree per scenario, which at a million rows costs more than the pass
being measured, and every file the generator writes comes off one `content()` call —
so one wrong size is all of them. What remains unchecked is the tree's GEOMETRY on
disk (depth, names, files per leaf are checked as a node count, not walked) and the
file CONTENT: `fixture_hash` covers the shape and the declared sizes, not the bytes,
so a change to the padding byte moves every file and every baseline hash while two
runs still diff as one fixture.

A run writes its result file again after EVERY scenario, so an assertion firing in
the thirtieth scenario of a sweep no longer throws away the twenty-nine already
measured.

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
| `twoway_dir_move_*` | One directory renamed — the DEEPEST one, a leaf of twenty files. The case that cost 63 seconds before it was narrowed. Pins `dir_moves`, so a re-upload of the subtree cannot pass as a fold. Because the moved directory is a leaf, its cost says nothing about how a fold scales with what the directory holds: `twoway_top_dir_move_100k` (group 4) is the other end. |
| `twoway_after_upload_*` | The pass after an upload: one per cent of the files carry the row an unconfirmed push of ours leaves (the content and version this side wrote, the agreed-content marker still on the previous content), each announced long enough ago to confirm, plus ONE edited file so the plan pins `actions: 1`. The run also asserts the pass confirmed every push, so a pass that stopped confirming — and so stopped writing to its copy of the baseline — cannot report itself cheap. It prices that write: the other one, besides a folded move, a change-scoped pass makes to its baseline. |
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

The three headline classes at 1k, 10k, 100k and 1M, plus the two passes that edit
their own view of the baseline — `twoway_dir_move_*` and `twoway_after_upload_*`
— at 100k and 1M.

`twoway_top_dir_move_100k` renames a directory holding HALF the pair: its own shape,
two top-level directories of 50k files each (50000/1/73 ASCII), one of them moved. It
is what a folded move costs as the moved directory grows. The memory child bounds the
pass's baseline edits by what it edited (one move, one confirmation: a fixed number of
bytes each), so a fold that wrote the rows it carries fails there rather than printing
a large widest point. Its local observation of the 50k files it finds at the new path is
per-pass data and still scales with the move — that is the scan, not the baseline.

A figure is only ever read
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
