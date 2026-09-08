---
name: triage-test-run
description: Diagnose a red GitHub Actions run of this repo — the nightly `Tests` matrix (six native legs, test-wasm, test-2fa) or `Continuous Integration` — from a run id / URL or "why was last night red". Fetches every leg's log, parses the failures, groups them by root cause, investigates + live-confirms + adversarially refutes each cause with a fixed Sonnet/Opus/Fable workflow, and delivers a table (issue, kind, failures caused, legs, recurrence, confirmed?, suggested fix, complexity, blocks CI / owner) with per-issue details. Diagnosis only — fixes are suggested, never applied.
---

# Triage a test run

The deliverable is `<scratch>/triage/<run-id>/report.md`: one table row per root cause, a details
section per row, then unexplained failures, anomalies, backend asks and the permission prompts the
agents hit — plus a memory file so the next night's triage knows tonight's classes. Nothing in the
repo changes: no fix, no commit, no push. The owner picks fixes from the table
([[feedback_ask_before_fixing_diagnostic_questions]]).

Every step below names absolute paths. `$S` is the session scratchpad, `$R` the repo root,
`$T` = `$R/.claude/skills/triage-test-run/scripts/triage.py` (stdlib Python, needs a logged-in `gh`).

## 0. Ground rules

- **Never `cd`**, not even to an absolute directory, not even alone before a `git` command: after a
  `cd` the permission checker treats every argument (a `sha..HEAD` range included) as a path and prompts.
  `git -C $R …`, `cargo test --manifest-path $R/Cargo.toml …` (subcommand first: the allow rules are
  prefix matches on `cargo test`), `python3 $T …`.
- **Never `grep -r` / `rg` / `find` on `$R` or a worktree** (a dotenv file lives there): `git -C $R grep -n
  PATTERN -- '*.rs' '*.ts'` with a pathspec, or plain grep on `$S/triage/...` / an absolute `src` subtree.
  Git pathspecs for a crate are extension globs (`-- 'filen-sdk-rs/*.rs'`), never a bare directory
  (`filen-sdk-rs/web` holds a dotenv file too).
- Never read, name or copy the dotenv file; `cargo test` loads it itself. Its name is also not a grep
  pattern (auto-denied: search for `dotenv` or `source $(` instead). Read known files with the Read
  tool (offset/limit for log excerpts).
- **Flakiness is a bug** ([[feedback_flakiness_is_a_bug]]): shared-account contention, a timing race,
  a fixed sleep — those are `test` rows with a fix, never "transient". The one exemption is genuine
  server rate limiting (`rate_limited`), a `backend` row.
- The workflow's model split is fixed in the script and matches the owner's rule: investigators and
  the anomaly scan on Sonnet, live confirmation and the judge on Opus, refuters on Fable. Do not
  launch it with inherited models, do not put Fable on bulk work.
- Live confirmation (local reruns, read-only probes, mutating probes with cleanup) is allowed by
  default; the owner can restrict it per run through `args.allow`. Every probe is a throwaway file,
  deleted, with `git status --porcelain` proving it.
- The skill itself is the owner's opt-in for a workflow: invoking it means "run the workflow".

## 1. Pick the run

```bash
gh run list --repo FilenCloudDienste/filen-rs --workflow test.yml --event schedule --limit 10   # nightlies
gh run list --repo FilenCloudDienste/filen-rs --workflow ci.yml --limit 10                      # CI on push / PR
gh run list --repo FilenCloudDienste/filen-rs --workflow test.yml --commit <sha>                # a [test] push
```

"Last night" = the most recent completed scheduled `Tests` run (they start ~00:40 UTC and take up
to 4 h 30 m; a run still `in_progress` has partial logs — say so and wait or triage the finished
legs). A run id or URL from the user wins. `Tests` = 6 native legs (`test (os, V1|V2)`: three OSes ×
two accounts, V1 and V2, plus one share account) + `test-wasm` (chromium then firefox, `&&`-chained)
+ `test-2fa (V1|V2)` after the matrix. `Continuous Integration` = clippy / fmt / unit tests per OS +
wasm; its failures are usually step errors (a clippy `error:`, a rustfmt `Diff in`), which the script
turns into `STEP` failure records and clusters like tests, so the rest of the procedure is the same.

## 2. Fetch and parse

```bash
python3 $T fetch <run-id|url> --out $S/triage           # run.json, jobs.json, <job>.log (or <job>.log.missing with the API error), <job>.annotations.json
python3 $T failures $S/triage/<run-id>                  # failures.json, clusters.suggested.json, clusters.json (first time only); prints legs, failures, suggested clusters
python3 $T timeline $S/triage/<run-id> [--bin NAME]     # every binary on every leg: wall clock, seconds, F<n>, ABORTED / IN FLIGHT; wasm browsers below
python3 $T recurrence $S/triage/<run-id> --nights 7     # the run's failures in the previous 7 runs of the same event (nightly: schedule; CI: push), fetched + cached
```

Read the leg table first. What a line means:

- `cancelled` at ~270 min = the workflow's `timeout-minutes` kill (GitHub labels a timeout
  *cancelled*, not failure). `failures` names the binary in flight, how many of its tests had reported,
  the `has been running for over 60 seconds` tests, and the orphan processes the runner killed
  (`alloc_ceiling-…`, `curl`). The tests still running are the binary's test list (from a leg where it
  finished) minus the reported ones.
- `log=missing` (`NO LOG` in `fetch`'s table) + annotation *The hosted runner lost communication* =
  machine death: nothing to parse; timing only (the death is 45–70 min before `completed_at`).
  `BlobNotFound` in `<job>.log.missing` also means an expired log (90-day retention).
- `ABORTED <crate>/<bin>` = a test binary that started and never printed `test result:` on a leg that
  was not killed (signal, abort, OOM); it becomes a `<binary aborted>` failure record with cargo's
  `process didn't exit successfully` line as the message.
- `STEP` records = a CI leg's step errors (clippy, rustfmt `Diff in`, `##[error]`) turned into failure
  records, clustered like tests; `step=` in the leg header names the red step.
- wasm: a chromium failure means firefox never ran — `failures` prints `NOT RUN` for it.
- `image=` is the runner image; a difference between two legs of the same OS is a discriminator
  (image rollouts are gradual).
- `WARNING … binary reports N failed, parsed M` = a test name the parser did not match; read that
  binary's block in the log and add the failure by hand.

Then fetch the **baseline**, the most recent green scheduled run, for the anomaly scan
(`gh run list … --event schedule --status success --limit 1`, then `fetch` + `failures` on it), and
look up **CI on the same commit** (`gh run list --workflow ci.yml --commit <sha>`): "blocks CI" is a
column, and a red CI run on the same sha often shares the cause.

`failures.json` holds everything the agents read: per leg `binaries[]` (crate, bin, started/finished,
seconds, per-test status, slow tests), `failures[]` (test, panic site, message, log line, the
captured-stdout line range), `browsers{}` for wasm, `step_errors[]` for CI legs, `in_flight`,
`orphans`, `runner_image`, `annotations`.

## 3. Cluster by hand — the judgement step

`clusters.json` starts as the script's mechanical grouping (same test → one group; groups sharing a
panic site or a message head merge). It over-merges on generic sites and over-splits cascades. Edit
it: move `failures` records between clusters, drop or add clusters, set `id` and `label`. Only those
three fields matter — the workflow re-derives `legs`, `failure_count`, `sites` and `tests` from the
records, so a merged cluster needs no bookkeeping — but every record must keep its fields (agents read
log paths and line numbers from them). `failures` never overwrites `clusters.json` once it exists (a
re-parse writes `clusters.suggested.json`); delete it to start over. Rules:

- One mechanism = one cluster. The same test on several legs is one cluster unless the panic sites
  differ; then two mechanisms hit one test — split by site.
- A setup-helper site is not a cause: `test-utils/src/lib.rs:103` / `:120` (`get_resources` creating
  the test dir), `core/src/ops/function.rs:250` (assert_cmd in the CLI tests). Group those by
  endpoint + error + time window instead: "every `v3/dir/create` TimedOut 04:45–05:02 on windows-V1"
  is one cluster whatever the test names.
- A test that fails only because an earlier test in the same binary broke shared state is a cascade:
  same cluster, `is_cascade_of` in the finding.
- V1 legs and V2 legs use different accounts: a cluster confined to one account's legs points at
  account state or a backend queue, not at code.
- wasm firefox timeouts on upload-bearing tests while the native legs churn the drive-write lock
  are the contention class, not a firefox bug.
- Check the sum: the records across clusters must add up to `totals.failed_tests` (the workflow logs
  a WARNING at launch when they do not).
- Check `recurrence` and Appendix C: a known class gets its pointer in `notes` (below), it still gets
  investigated (the fix may have been on an unpushed branch, or the class has a new trigger).

## 4. Source at the run's commit

```bash
git -C $R fetch origin
git -C $R rev-parse HEAD                                   # equal to run.json head_sha …
git -C $R status --porcelain --untracked-files=no          # … and nothing printed? then tree_dir = $R
git -C $R worktree add --detach $S/triage/tree-<sha8> <sha>   # otherwise; remove it at the end (git -C $R worktree remove …)
```

No cargo in the worktree (cold target dir = the vendored C++ decoders build). Local reruns happen in
`$R`; the confirm stage records `git -C $R diff --stat <sha> -- '<crate>/*.rs'` as the caveat. When
the failing test does not exist in `$R` at all (it landed after HEAD), say so in `notes`: the confirm
agent then needs the worktree with its own `CARGO_TARGET_DIR` under `$S` and, for tests that need the
account, the dotenv linked in with the glob form `ln -s "$R"/.en[v] <tree_dir>/`.

## 5. Run the workflow

```
Workflow({
  scriptPath: "$R/.claude/skills/triage-test-run/scripts/triage.workflow.js",
  args: {
    run_dir: "$S/triage/<run-id>", script_dir: "$R/.claude/skills/triage-test-run/scripts",
    clusters: <the array from clusters.json after step 3>,
    repo_dir: "$R", tree_dir: "$R" | "$S/triage/tree-<sha8>", scratch: "$S/triage",
    head_sha: "<sha>", last_green_sha: "<sha of the baseline run>", baseline_run_dir: "$S/triage/<baseline-run-id>" | null,
    total_failures: <totals.failed_tests>,
    allow: { local_rerun: true, readonly_probe: true, mutating_probe: true },
    recurrence_text: "<the recurrence table>",
    notes: "<see below>"
  }
})
```

`clusters` is the parsed array itself (Read `clusters.json`, paste it): the script cannot read files, and
a launch without it fails at once with `args.clusters is empty; nothing to triage`. `notes` is what
the agents cannot find in the logs: the known classes this run matches (Appendix
C, with branch/commit pointers), `git -C $R log --oneline <last_green>..<sha>` (what landed since the
last green run), the CI run on the same sha and its failing step, owner instructions for tonight
("logs only", "skip the wasm row"), and hints from the memory index (last night's diagnosis, fixes
sitting on unpushed branches).

Shape: `Investigate` (one Sonnet agent per cluster, plus one whole-run anomaly scan against the
baseline) → `Confirm` (Opus, **serialized**: one cluster at a time may touch the shared test account;
skipped when the plan is `none`/`source_only` or the kind is not allowed) → `Refute` (three Fable
lenses per cluster: evidence, fix, classification; default refuted when a load-bearing claim cannot be
verified) → `Synthesize` (Opus judge: merges/splits, priority, one row per mechanism). About five
agents per cluster plus two: the 09-08 dry run (4 clusters) took 22 agents, 76 min and ~3 M tokens,
dominated by the serialized confirms (a 400 s local rerun, a pty probe, a firefox probe, an IMAP probe).
Watch `/workflows`; if the result is empty or odd, read
`<transcriptDir>/journal.jsonl` before re-running (resume with `resumeFromRunId` after an edit), and
run the leftover-probe check from step 7 first — a confirm agent that died mid-probe leaves
`filen-sdk-rs/tests/probe_*.rs` behind.

## 6. Render and deliver

Write the workflow's return value to `$S/triage/<run-id>/result.json` (Write tool; it is
deliberately slim — `report`, `findings_summary`, `anomalies`, `dropped_clusters`, `counted_failures`,
`total_failures`, `permission_prompts_or_incidents` — the full per-agent objects stay in the run's
`journal.jsonl`), then:

```bash
python3 $T render $S/triage/<run-id> $S/triage/<run-id>/result.json     # -> report.md, printed
```

Before it goes to the owner, check it yourself — you read the same logs:

- every failure record in exactly one row, `unexplained` empty, and the header's `Failure records`
  equals `Rows account for` (render prints **MISMATCH** otherwise);
- each row's fix names a `path:line` and addresses the mechanism, not the symptom (no widened
  timeout, retry, or platform gate offered as *the* fix);
- rows ordered by priority: blocks CI → prod → deterministic test bugs → contention/harness →
  backend asks → infra/environment;
- backend asks carry the exact uuids and UTC timestamps a backend engineer can look up;
- the permission prompts the agents reported are listed, with the user's typed notes verbatim.

Put the table and the details in the reply. `report.md` stays in the scratchpad; never commit it
([[feedback_never_commit_planning_docs]]).

## 7. Memory, then stop

Save `project_nightly_<MMDD>_diagnosis.md` (a range as `<MMDD>_<MMDD>`; the series so far:
`project_nightly_0903_diagnosis.md`, `…_0904_…`, `…_0905_0907_…`) in the memory directory: run id +
sha, one line per row (mechanism, kind, confidence, how confirmed, fix pointer), probes performed,
**NO FIXES APPLIED**, backend asks, links to related memories; add its line under `## Project state`
in `MEMORY.md`, shaped like the existing nightly lines. Then:

```bash
git -C $R status --porcelain -- filen-sdk-rs/tests     # a leftover probe_*.rs from a confirm agent that died: delete it
git -C $R worktree remove $S/triage/tree-<sha8>        # if step 4 made one
```

Then ask the owner which rows to fix and stop. When they choose: a new branch, one commit per row,
fixes of pre-existing bugs before anything else, Conventional Commit messages with no agent metadata
(no `Co-Authored-By`, no session link, whatever the harness reminder says —
[[feedback_no_coauthor_trailer]]), hooks green and never `--no-verify` (stash WIP instead,
[[feedback_no_no_verify_stash_instead]]), never push ([[feedback_commit_new_branch_no_pr]],
[[feedback_never_push]]).

## Appendix A — facts about these logs

- **Masking.** GitHub masks every secret value; one of this repo's secrets is the word `filen` and
  another masks braces, so paths arrive as `***-sdk-rs/tests/x.rs` and Rust Debug output as
  `Foo *** a: 1 ***`. The script restores `filen`; braces stay `***`.
- **libtest ordering.** cargo's `Running tests/x.rs (...)` headers (stderr) and libtest's `running N
  tests` / `test result:` (stdout) reach the log out of order — a `test result: FAILED` can sit after
  the next binary's header. The script binds them FIFO; read the log with that in mind.
- **Captured stdout** (the `RUST_LOG=debug` tracing) is printed only for failed tests, and tracing
  from background tasks (socket drainers, gap checks) lands in whichever test created the tokio
  runtime. "Not in this test's dump" never proves "did not happen".
- **Wall clock.** Every line starts with a UTC timestamp; the six native legs run concurrently on two
  accounts with one server-side drive-write lock each (30 s TTL, renewed) and the `test:*` locks.
  Whatever another leg was doing in the same minutes is part of the diagnosis — `timeline` shows it.
- **Log source.** Always the per-job endpoint (`actions/jobs/<id>/logs`, what `fetch` uses).
  `gh run view --log` has spliced stale blocks of other jobs into a job's log and produced phantom
  recurrences.
- `test.yml` runs `cargo test -F filen-sdk-rs/malformed,filen-sdk-rs/heif-decoder --no-fail-fast`
  at the workspace root with `RUST_LOG=debug`, `timeout-minutes: 270`, `fail-fast: false`; test-wasm
  sets `VITE_TEST_TIMEOUT_MULT=10` (30 min per test). Healthy native legs take ~2 h 20 m – 3 h 50 m.

## Appendix B — kinds, owners, scales (the workflow uses the same words)

| kind | meaning |
|------|---------|
| `test` | the test or harness is wrong for its environment: bad assert, missing shared-account lock, fixed sleep, cross-test state, stale fixture, contention it does not tolerate |
| `ci` | workflow yaml, runner setup steps, caches, `timeout-minutes`, features of the test command |
| `prod` | a bug in shipped code that the test correctly caught |
| `backend` | Filen server behaviour: contract change, write-queue lag, missing field, rate limiting, purge SLA |
| `infra` | runner death, DNS/egress blackout, unreachable third-party host, GitHub API limits, runner-image bugs |
| `environment` | toolchain / dependency on the runner: missing tool, cache-restored stale binaries, OS-specific API |

Owner: `us` / `backend` / `github` / `third-party`. Complexity: `trivial` (a line or a flag),
`small` (one file, under an hour), `medium` (several files or a design choice), `large` (redesign or
new infrastructure), `not-ours`. Confidence: `confirmed` (reproduced or probed, or deterministic
across legs with the mechanism read in source), `likely` (mechanism read in source, consistent with
every log line, not reproduced), `hypothesis` (fits, alternatives not excluded).

## Appendix C — recurring classes seen so far (as of 2026-09-08; verify pointers before quoting)

| signature | class | kind | pointer |
|-----------|-------|------|---------|
| firefox `service worker` → `NetworkError when attempting to fetch resource` | firefox idle-kills the test service worker while the page waits on the contended drive lock; the restarted worker has no state and `respondWith` hides the error (09-08 triage, reproduced) | test | 08-16/17, 09-02, 09-08 |
| `test_updater` → `Failed to parse GitHub releases response` | `updater_tests.rs` step 1 self-replaces the freshly built binary with the published release, so later steps run an old updater without a658b654's token + status check (09-08 triage) | test | 07-31, 08-08, 09-08 (windows-V2) |
| `client_tests.rs:265` `No email received` | 08-16: mailer latency > 600 s; 09-08: `v3/user/password/forgot` answered success and no mail was ever emitted (IMAP probe) — backend ask | infra / backend | 08-16, 09-08 |
| `run_manuel_tests` all 18 recordings `Replay output mismatched` in ~3 s | `manuel_recordings/.bashrc` sources a repo-root dotenv and exits on unset `TEST_AUTH_CONFIG_PATH` before any keystroke (09-08 triage, pty-reproduced) | test | 09-08 |
| CI `clippy --tests`: `unused import: std::time::Duration` in `manuel_tests.rs` | import outside the `cfg(target_os = "linux")` gate; blocks ci.yml on macOS + Windows | test | 09-08 |
| `rate_limited` on `v3/chat/conversations/create` | server-side create budget — the one accepted flake | backend | flakiness audit |
| 300 s `TimedOut` on `v3/dir/create` / `v3/upload/done` in one window, account-scoped | backend write queue executing late (work applied minutes to 1.5 h after the 200) | backend | 09-06 |
| `file_tests.rs:534` `file_trash_empty` | async purge slower than the 300 s sleep | backend | 09-06 |
| `user_tests.rs:302` `folderRestored` never seen | restore applied ~80–100 min late server-side | backend | 09-06 |
| `missing field stableUUID` on `v3/dir/content` (trash) | a versioned-file row without lineage; blocked server-side 09-03 | backend | 09-03 |
| `alloc_ceiling.rs:987` `raw cases measured NOTHING` + 270-min kills | raw.pixls.us unreachable, curl without `--connect-timeout`, no fixture cache | infra + test | 09-05 |
| firefox 180/240 s timeouts on thumbnail tests while native legs run | drive-write contention from the wasm uploads; `VITE_TEST_TIMEOUT_MULT` (09-04) | test | 09-01, 09-03, 09-04 |
| Windows leg: no log, *runner lost communication* | image-correlated runner death | infra | 08-13..08-18 |
| `fusermount3: mount failed: Operation not permitted` | runner image #14516, guarded `rm` step in test.yml | ci | 08-07 |
| Windows `io::tests` `set_times` os error 5 | read-only handle; fix 139f3e7e on `fix/nightly-0903` | test | 09-03 |
| chat `conversation_creation` / `chat_msgs` avatar mismatch | avatar cache vs `upload_avatar` on a sibling leg; fixes 50f97e7a, aa1f7555 | test | 07-30, 09-02, 09-03 |
| db_tests `fell back to the full sweep` | events cutoff millis→seconds (bc99e275, on main) / trash relist after a sibling's trash | prod → test | 09-02, 09-03 |
| `test_search` 240 s zero progress | resync never entered; instrumented 304b0017, cause unknown | test | 08-08, 08-18 |
| `test_websocket_file_edit_versioning_disabled` 6/6 legs | server now re-mints the trashed ghost's stableUUID (intentional); fix branch `fix/versioning-off-trash-stable-id` | backend → prod | 09-04 |

`scripts/`: `triage.py` (fetch / failures / timeline / recurrence / render) and `triage.workflow.js`
(the Workflow script; models and schemas fixed inside). The parser is proven on the 09-03, 09-05,
09-06 and 09-08 nightlies (33 / 4 + 2 cancelled / 20 / 6 failures, clusters matching the by-hand
diagnoses) and on the 09-08 CI run (2 step errors); the whole procedure ran end to end on the 09-08
nightly (report in that session's scratchpad, summary in memory `project_nightly_0908_diagnosis`).
