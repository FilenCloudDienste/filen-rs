# The sync-engine baseline

What the engine costs, measured on the code that reads a pass's baseline rows from the store.
`run_p` is the baseline for memory and for every scenario's timing; `run_q` times the store against
the resident tree it replaced, alternated at 1M. `SCENARIOS.md` says what each scenario is for and
what each column means.

Each result file stamps the commit it was measured on, recorded before this branch was rebased.

## The sweep: `run_p` and `run_q`

Schema version 3. Every stamp is clean: the harness refuses to write a `-dirty` or `unknown` stamp
into this directory, and `run_p` was written here directly, under that check.

- `baseline/run_p_sync_bench_all.json` — all thirty-eight scenarios, five samples after two
  discarded warmups, two memory children each. Quiet machine: the live suite had finished and
  nothing else built or measured (load average 1.2 at the start, 2-3 during). 37 minutes.
- `baseline/run_q_sync_bench_1m_{old,new}_{1,2}.json` — the resident tree, before the store round
  (`old`: the baseline held in memory, where the round started), against the store (`new`),
  alternated old/new/old/new on one machine, `twoway_idle_1m`, `twoway_one_file_1m` and
  `twoway_one_percent_1m`, five samples a run and no memory children.
- The code measured and the commit that publishes these files differ only in documentation and
  one doc comment (`Readers::give`).

Every file was written by:

- Harness version 4 (a file written by another version is refused by `compare`, not misread)
- `rustc 1.95.0-nightly (7f99507f5 2026-02-19)`, profile `debug_assertions`
- `celeste.local macos/aarch64`, system allocator (this crate declares no `#[global_allocator]`)

## What a loaded pair costs

Fresh children, MiB, median of two (`run_p`):

```
                          engine open   pair loaded   pair over open   widest over loaded   after the pass
twoway_idle_1k                  16.09         16.23             0.13                 0.32            16.56
twoway_idle_10k                 16.11         16.22             0.11                 0.34            16.57
twoway_idle_100k                16.08         16.17             0.09                 0.34            16.52
twoway_idle_1m                  16.05         16.15             0.10                 0.32            16.48
twoway_one_file_1m              16.12         16.27             0.16                 1.42            17.71
twoway_one_percent_100k         16.17         16.27             0.10                 4.40            20.69
twoway_ten_percent_100k         17.27         17.37             0.10                26.02            43.40
twoway_one_percent_1m           17.22         17.30             0.09                26.46            43.78
twoway_dir_move_100k            16.01         16.12             0.12                 2.94            19.08
twoway_dir_move_1m              16.05         16.15             0.10                 6.84            23.02
twoway_after_upload_100k        16.07         16.18             0.11                 3.38            19.57
twoway_after_upload_1m          16.01         16.11             0.10                17.95            34.08
twoway_top_dir_move_100k        16.05         16.16             0.12                91.48           107.66
```

**The steady state does not depend on the pair's size: 16.23 / 16.22 / 16.17 / 16.15 MiB at
1k / 10k / 100k / 1M.** Loading a pair adds 0.09-0.16 MiB over an engine with nothing loaded, at
every size; the resident tree, before the store round, added 121.67 at 1M. That is the engine's
fixed cost and nothing else: the runtime, the client, the store's two connections and SQLite's own
allocations.

Per term, what the pair's baseline computes itself as (`mem:pair_baseline_term_*`, bytes, at
every size):

```
handle   192   the snapshot handle: a pooled connection, its pair and its counts
edits      0   a pass's moves and confirmations, or the rows frozen for its apply
total    192
```

The resident tree computed itself as 134,995,728 bytes at 1M.

Between passes the store holds NO rows in memory. What it holds is SQLite's, and it is set, not
defaulted: `READER_CACHE_KIB` (8 MiB) on the one idle reader connection the pool keeps, and
`WRITER_CACHE_KIB` (2 MiB) on the writer. Both are ceilings a pass fills, not steady-state costs:
an idle pair sits at the figures above whatever its size. The "after the pass" column is where a
pass leaves the process — the reader's cache warm and the allocator's pages not yet returned: +0.33
MiB after an idle pass at 1M, +26.5 after the one-percent pass. **What SQLite's caches hold is
not itemised** (no `SQLITE_DBSTATUS_CACHE_USED` figure: rusqlite exposes no safe status call and
nothing here reaches for the FFI), so the part of that +26.5 that is page cache rather than
allocator retention is bounded by the 8 + 2 MiB ceilings and not measured.

A pass's own edits are counted too (`mem:pass_baseline_computed_bytes` = handle + edits): 790 B
for a folded leaf move, 746 B for `twoway_top_dir_move_100k`'s move of a directory holding half
the pair, and 1,404,464 B at 1M after an upload — the markers of the one per cent of file rows the
pass confirms. The memory child bounds them (`EDITS_BASE_BYTES` + `EDIT_BYTES` per move or
confirmation), so a fold that wrote the rows it carries fails the run.

The two passes that write to their own view of the baseline copy nothing: at 1M the directory
move's widest point over its loaded pair is 6.84 MiB and the pass after an upload's 17.95, where
the resident tree, which copied itself for both, measured 168.70 and 171.44. The passes that READ
many rows pay for it instead: one-percent 1M 26.46 and ten-percent 100k 26.02 MiB, against 21.24
and 17.30 on the resident tree — the pages and decoded rows of reads the tree had already paid
for.

`twoway_top_dir_move_100k` peaks at 91.48 MiB over its pair, and it is not the baseline (746 B of
edits): its local observation opens and reads the 50,000 files it finds at the new path, the pass
runs 448,942 store statements and reads 949,180 rows, and `observe_local` is 1078 of the pass's
1788 ms.

Commits after this sweep changed some of these figures, and each one's message carries its own:
among them `twoway_top_dir_move_100k`, whose renamed directory's files now take their hashes from
the rows they left instead of being opened, and `twoway_mass_delete_10k`, whose reconcile now
walks the rows once.

## What a pass costs

At 1M, alternated (`run_q`), median of ten samples per side, ms. `tree` is the resident tree,
before the store round; `store` is the code `run_p` measured:

```
                          tree                       store                        change   guardrail
twoway_idle_1m            0.051   [0.047-0.055]      0.056   [0.049-0.058]        +9 %    no-pass, unchanged      met
twoway_one_file_1m        0.169   [0.127-0.182]      0.197   [0.167-0.323]       +17 %    < ~2 ms                 met
twoway_one_percent_1m   322.7     [261.1-497.6]    327.8     [271.1-384.5]      +1.6 %    <= 2x (~850 ms)          met
yardstick (whole) 1M    31016-31433               31144-31723                  +0.4 to +0.9 %  within ~10 %        met
```

The per-run medians agree: tree one-percent 322.2 / 450.4 ms, store 332.3 / 325.1; tree whole pass
30.9-31.5 s, store 31.1-31.8 s. The guardrails are judged against the same-machine alternation
rather than against figures from another session; `run_p`'s own 1M figures (0.054 / 0.192 / 378.6
ms, whole pass 31.0-33.4 s) say the same.

An idle WATCH WAKE is a no-pass: `run_pass` returns before it asks for the pair's rows. The idle
SCENARIO, which runs a pass with nothing to find, reads three statements and no rows
(`walk:baseline_read_statements` / `_rows`).

Below 1M nothing was alternated. Two rows of `run_p` pay for reading rows the tree held in memory:

- The pass after an upload reads every unconfirmed row off the partial index and confirms it
  (`Baseline::confirm_where`): `confirm_pushes` is 27.3 of the pass's 28.9 ms at 1M and 2.44 of
  2.69 at 100k. That is the design.
- A mass delete is a whole pass whose reconcile enumerates the table's rows four times and decodes
  a copy of each, where the tree handed out references: `reconcile_and_screen` is 25.3 of its
  91.3 ms, against 11.1 on the tree. It is not the counting trigger (`baseline_counts`): the
  measured phase stops before the apply, so no row deletion is timed.

A whole pass decodes every row it enumerates from the table where the tree handed out references;
at 1M that disappears into the scan (+0.4-0.9 % alternated), and below 1M it is a few per cent.

## The pass, scenario by scenario

`pass_ms` is the median of five samples of the scenario's own pass; `spread` is that row's max
over its min in the same run; `yardstick_ms` is a WHOLE pass over the same fixture in the same run,
taken once per sample after the warmups; `speedup` is the two divided, both measured the same way.
`actions` is what the pass planned, recorded rather than asserted-and-forgotten. `run_p`:

```
scenario	nodes	reps	n	pass_ms	spread	yardstick_ms	speedup	actions
backup_local_one_percent_10k	10824	1	5	2.5305	1.14x	122.7	48.5x	102
backup_remote_one_percent_10k	10824	1	5	2.5830	1.13x	123.4	47.8x	102
pull_idle_10k	10824	8	5	0.0563	1.54x	121.6	2160.4x	0
pull_one_file_10k	10824	8	5	0.1469	1.06x	120.4	819.5x	1
pull_one_percent_10k	10824	1	5	2.5395	1.06x	122.4	48.2x	102
twoway_after_upload_100k	103479	4	5	2.6926	1.04x	1373.1	509.9x	1
twoway_after_upload_1m	1065119	1	5	28.9324	1.52x	32081.0	1108.8x	1
twoway_balanced_100_edits_10k	10824	1	5	2.4685	1.10x	125.0	50.7x	100
twoway_both_changelists_10k	10824	1	5	2.9463	1.20x	125.4	42.6x	102
twoway_deep_narrow_100_edits_10k	12286	1	5	6.6632	1.16x	385.8	57.9x	100
twoway_dir_move_100k	103479	4	5	1.3634	1.12x	1386.1	1016.7x	1
twoway_dir_move_10k	10824	4	5	0.9552	1.09x	126.0	131.9x	1
twoway_dir_move_1m	1065119	1	5	1.9959	1.71x	32245.1	16155.6x	1
twoway_first_sync_100k	103479	1	5	2962.0496	1.02x	2951.4	1.0x	103479
twoway_first_sync_10k	10824	1	5	237.1737	1.03x	237.0	1.0x	10824
twoway_idle_100k	103479	32	5	0.0494	1.69x	1324.4	26789.3x	0
twoway_idle_10k	10824	32	5	0.0478	1.21x	122.1	2553.6x	0
twoway_idle_1k	1364	32	5	0.0414	1.37x	14.4	347.6x	0
twoway_idle_1m	1065119	32	5	0.0541	1.17x	30987.6	573259.9x	0
twoway_large_files_50_edits_512	530	1	5	24.6238	1.02x	31.3	1.3x	50
twoway_long_paths_100_edits_10k	10824	1	5	4.4332	1.12x	359.9	81.2x	100
twoway_mass_delete_10k	10824	1	5	91.3430	1.03x	91.0	1.0x	0
twoway_one_file_100k	103479	8	5	0.1751	1.23x	1325.0	7566.5x	1
twoway_one_file_10k	10824	8	5	0.1582	1.06x	121.8	770.4x	1
twoway_one_file_1k	1364	8	5	0.1324	1.16x	14.4	109.0x	1
twoway_one_file_1m	1065119	8	5	0.1925	2.06x	33352.2	173300.7x	1
twoway_one_percent_100k	103479	1	5	24.5166	1.07x	1353.7	55.2x	982
twoway_one_percent_10k	10824	1	5	2.4383	1.18x	122.5	50.2x	102
twoway_one_percent_1k	1364	4	5	0.3203	1.18x	14.4	45.0x	12
twoway_one_percent_1m	1065119	1	5	378.6409	1.44x	32783.6	86.6x	10130
twoway_one_percent_scattered_10k	10824	1	5	6.6025	1.12x	123.9	18.8x	102
twoway_rename_storm_10k	10824	1	5	3.4925	1.15x	127.1	36.4x	102
twoway_small_delete_10k	10824	1	5	0.2893	1.46x	123.6	427.2x	10
twoway_ten_percent_100k	103479	1	5	259.9147	1.01x	1487.2	5.7x	9826
twoway_ten_percent_10k	10824	1	5	24.3178	1.06x	135.0	5.6x	1024
twoway_top_dir_move_100k	100002	1	5	1788.4286	1.02x	2018.6	1.1x	1
twoway_unicode_100_edits_10k	10824	1	5	2.9338	1.13x	178.1	60.7x	100
twoway_wide_flat_100_edits_10k	10001	1	5	9.5940	1.10x	102.4	10.7x	100
```

Read `speedup` as what change-scoping buys on that shape. It is 26789x on an idle 100k tree, 1.0x
on a mass delete and a first sync and 1.3x on a tree of megabyte files — the cases where the scoped
pass and the whole pass do the same work, because both must hash or both must read everything —
and 1.1x on `twoway_top_dir_move_100k`, whose pass reads every file of the moved half.

## What a figure here can resolve

Two whole sweeps of one binary at one commit, run back to back on a quiet machine, moved by a
median of 1.5 % per scenario on `total` and 1.3 % on `yardstick_whole_pass`; apart from the one
row the yardstick gate marked, two `total` rows left their spread at all, by 6.4 % and 3.1 %.

**So a difference under about 5 % is not a difference**, whatever the fourth decimal says. The
figures are printed to four decimals because the smallest of them is tens of microseconds, not
because they are known to four significant figures. `compare`'s `(within spread)` marker is a
noise floor, not a gate; the gate is the yardstick (`SCENARIOS.md`).

Against a run of the resident tree, the gate does not hold below 1M either: the store round changed
the whole pass itself (it reads every row from the table), so a moved yardstick is the code as
much as the machine. That is why the comparison above is alternated.

These rows are looser than the rest and should not be quoted tightly even against that 5 %:

| Scenario | median | in-run spread |
|---|---|---|
| `twoway_one_file_1m` | 0.1925 ms | 2.06x |
| `twoway_dir_move_1m` | 1.9959 ms | 1.71x |
| `twoway_idle_100k` | 0.0494 ms | 1.69x |
| `pull_idle_10k` | 0.0563 ms | 1.54x |
| `twoway_after_upload_1m` | 28.9324 ms | 1.52x |
| `twoway_small_delete_10k` | 0.2893 ms | 1.46x |
| `twoway_one_percent_1m` | 378.6409 ms | 1.44x |
| `twoway_idle_1k` | 0.0414 ms | 1.37x |

Every other row's own spread is under 1.25x. Read a direction into a 1M row only from an alternated
run like `run_q`, where the tree's one-percent pass alone spanned 261.1-497.6 ms.

## What is not comparable with what

- `sync_engine_probe` times a hand-assembled copy of a pass; these time the engine's own `prepare`.
  They are different functions — this one includes `pass_inputs`' DB prologue, which the probe
  never had, and excludes the probe's own scaffolding. Neither is a correction of the other.
- `mem:pair_baseline_computed_bytes`, `mem:pass_baseline_computed_bytes` and the
  `mem:pair_baseline_term_*` family were the resident tree's capacities before the store round and
  are the snapshot handle plus the pass's edits since; `compare` prints the tree's terms as ONLY IN
  BEFORE. Read them against `mem:*_rss`, not against a run of the tree.
- Result files written by an earlier harness version are refused by `compare` rather than misread.

## Reproducing it

```sh
SYNC_BENCH_SCENARIO=all SYNC_BENCH_SAMPLES=5 SYNC_BENCH_OUT=/tmp/bench \
  cargo test -p filen-sdk-rs -F sync-engine,bench-internals \
  --test sync_engine_bench -- --ignored --nocapture --exact sync_engine_bench
```

`all` is the sweep `run_p` is; `default` leaves out the 1M rows. `--exact` is not optional. Run it
twice and diff the two with `sync_engine_bench_compare` before quoting anything: one sweep cannot
tell you its own resolution.
