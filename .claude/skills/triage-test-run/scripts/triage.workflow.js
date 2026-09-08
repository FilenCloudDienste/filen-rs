export const meta = {
  name: 'triage-test-run',
  description: 'Diagnose one red GitHub Actions run: investigate each failure cluster, confirm live, refute adversarially, synthesize report rows',
  phases: [
    { title: 'Investigate', detail: 'one Sonnet investigator per cluster + one whole-run anomaly scan', model: 'sonnet' },
    { title: 'Confirm', detail: 'live confirmation (local rerun / probes), one cluster at a time on the shared account', model: 'opus' },
    { title: 'Refute', detail: 'three Fable refuters per cluster: evidence, fix, classification', model: 'fable' },
    { title: 'Synthesize', detail: 'Opus judge merges everything into the report rows', model: 'opus' },
  ],
}

// args (all paths absolute):
//   run_dir          <scratch>/triage/<run-id>  (run.json, jobs.json, <job>.log, failures.json, recurrence.json)
//   script_dir       the directory holding triage.py (agents run its `timeline` subcommand)
//   clusters         the array from clusters.json after the by-hand pass; only id, label and failures[] are
//                    trusted — legs, failure_count and sites are re-derived from failures[] below
//   repo_dir         the main working tree (where cargo runs; holds the dotenv file the tests need)
//   tree_dir         source at the run's head_sha (repo_dir itself when HEAD == head_sha and clean, else a worktree)
//   head_sha, last_green_sha, baseline_run_dir (last green run, may be null), scratch
//   total_failures   totals.failed_tests from failures.json (number)
//   allow            { local_rerun, readonly_probe, mutating_probe }  (booleans)
//   recurrence_text  the printed recurrence table, pasted
//   notes            free text from the orchestrator: known recurring classes, memory hints, what landed since
// Returns { report, findings_summary, anomalies, dropped_clusters, counted_failures, total_failures,
//           permission_prompts_or_incidents } — every agent's full return value is in journal.jsonl.
const A = args
const clusters = A.clusters
if (!clusters?.length) throw new Error('args.clusters is empty; nothing to triage')
for (const c of clusters) {
  c.failures ??= []
  c.legs = [...new Set(c.failures.map((f) => f.leg))].sort()
  c.failure_count = c.failures.length
  c.sites = [...new Set(c.failures.filter((f) => f.panic_file).map((f) => `${f.panic_file}:${f.panic_line}`))].sort()
  c.tests = [...new Set(c.failures.map((f) => f.test))]
  if (!c.id || !c.label) throw new Error(`cluster without id/label: ${JSON.stringify(c).slice(0, 200)}`)
}
const TOTAL = Number(A.total_failures)
const clusterSum = clusters.reduce((n, c) => n + c.failure_count, 0)
if (clusterSum !== TOTAL) log(`WARNING: clusters hold ${clusterSum} failure records, failures.json counted ${TOTAL}; something is unclustered or double-counted`)

const RULES = `HARD RULES (a violation costs the user a permission prompt, or breaks the repo):
- NEVER use cd: not alone, not in a compound command, not as a harmless-looking "cd X;" prefix before a
  heredoc. Every command names absolute paths: git -C <dir> ..., cargo test --manifest-path <dir>/Cargo.toml ...,
  python3 /abs/path, vitest --root /abs/dir.
- NEVER grep -r / rg / find against a repo or worktree root (a dotenv file lives there). Search tracked
  files with: git -C <dir> grep -n PATTERN -- '*.rs' '*.ts' (ALWAYS with a pathspec). Plain grep only on an
  absolute subtree such as <dir>/filen-sdk-rs/src, or on the scratch log dir.
- Read known files with the Read tool (offset/limit for log excerpts), not cat/head/sed -n.
  Never read, name, copy or list the dotenv file; cargo test loads it by itself. Its name must not appear
  in a grep PATTERN either (a search for it is auto-denied): search for "dotenv", "source $(" or "TEST_EMAIL".
- Do not modify tracked files, do not commit, do not push, do not touch git config or worktrees.
- Only the Confirm agent may run cargo, a test binary, filen-cli, wasm tests, or anything else that reaches
  the Filen server or the shared test accounts — and it holds the single serialized slot for that.
  Investigators, the anomaly scan, refuters and the judge read logs and source only; a claim you cannot
  verify by reading is reported as unverified, never re-run.
- Log facts: GitHub masks a secret that happens to be the word "filen" and the braces of Rust Debug output,
  so "***-sdk-rs" is filen-sdk-rs and "Foo *** a: 1 ***" is Foo { a: 1 }. Timestamps at the start of each
  log line are UTC wall clock. Captured stdout (RUST_LOG=debug tracing) appears ONLY for failed tests, and
  tracing from background tasks lands in whichever test created the tokio runtime, so "absent from this
  test's dump" never proves "did not happen".
- Your final answer is data for an orchestrator, not prose for a human. Quote log lines verbatim with the
  log file and line number. Never invent a line, path, timestamp or commit.
- If any command needed user approval, or the user typed a note into an approval dialog, record it
  verbatim in permission_prompts_or_incidents.`

const KINDS = `KINDS (pick exactly one):
  test        the test or harness is wrong for its environment (bad assert, missing lock on the shared account,
              fixed sleep, cross-test state, stale fixture). Shared-account contention is a TEST bug unless
              the server rate-limited: this repo treats flakiness as a bug to fix, never as acceptable.
  ci          the workflow yaml, runner setup steps, caches, timeouts, feature flags of the test command.
  prod        a bug in shipped code (filen-sdk-rs, filen-types, filen-mobile-native-cache, filen-cli, ...) that
              the test correctly caught.
  backend     Filen server behaviour: contract change, queue lag, missing field, rate limiting, purge SLA.
  infra       runner death, DNS/egress blackout, an unreachable third-party host, GitHub API limits, runner image bugs.
  environment toolchain/dependency on the runner: missing tool, cache-restored stale binaries, OS-specific API.
OWNER: us | backend | github | third-party.   COMPLEXITY of the fix: trivial (one line or a flag) | small
(one file, under an hour) | medium (several files or a design choice) | large (redesign, new infrastructure) |
not-ours (the fix is not in this repo).   CONFIDENCE: confirmed (reproduced or probed, or deterministic
across legs with the mechanism read in source) | likely (mechanism read in source, consistent with every
log line, not reproduced) | hypothesis (fits the evidence, alternatives not excluded).`

const clusterText = (c) => JSON.stringify(c, null, 1)

const INVESTIGATE_SCHEMA = {
  type: 'object',
  properties: {
    cluster_id: { type: 'string' },
    root_cause: { type: 'string', description: 'mechanism in 2-5 sentences: what happened, why, where in the code' },
    kind: { type: 'string', enum: ['test', 'ci', 'prod', 'backend', 'infra', 'environment'] },
    owner: { type: 'string', enum: ['us', 'backend', 'github', 'third-party'] },
    blocks_ci: { type: 'boolean', description: 'would this also fail ci.yml (clippy/unit tests on push/PR)?' },
    deterministic: { type: 'boolean', description: 'fails on every leg that ran it' },
    is_cascade_of: { type: ['string', 'null'], description: 'another cluster id whose cause explains this one, else null' },
    affected_tests: { type: 'array', items: { type: 'string' } },
    failure_count: { type: 'integer' },
    legs: { type: 'array', items: { type: 'string' } },
    evidence: {
      type: 'array',
      items: {
        type: 'object',
        properties: { where: { type: 'string', description: 'log path:line or source file:line' }, quote: { type: 'string' }, why_it_matters: { type: 'string' } },
        required: ['where', 'quote', 'why_it_matters'],
      },
    },
    timeline: { type: 'string', description: 'UTC wall-clock narrative across legs, incl. what else was on the account' },
    recent_commits_touching_it: { type: 'array', items: { type: 'string' } },
    fix: {
      type: 'object',
      properties: {
        summary: { type: 'string' },
        files: { type: 'array', items: { type: 'string' }, description: 'path:line targets' },
        sketch: { type: 'string', description: 'what the change is, concretely; a diff hunk if short' },
        complexity: { type: 'string', enum: ['trivial', 'small', 'medium', 'large', 'not-ours'] },
        addresses_root_cause: { type: 'boolean' },
      },
      required: ['summary', 'files', 'sketch', 'complexity', 'addresses_root_cause'],
    },
    confidence: { type: 'string', enum: ['confirmed', 'likely', 'hypothesis'] },
    alternatives_considered: { type: 'array', items: { type: 'string' } },
    confirmation_plan: {
      type: 'object',
      properties: {
        kind: { type: 'string', enum: ['none', 'source_only', 'local_rerun', 'readonly_probe', 'mutating_probe'] },
        steps: { type: 'string', description: 'exact commands / probe design; empty when kind is none' },
        expected_if_true: { type: 'string' },
        expected_if_false: { type: 'string' },
      },
      required: ['kind', 'steps', 'expected_if_true', 'expected_if_false'],
    },
    open_questions: { type: 'array', items: { type: 'string' } },
    split_or_merge: { type: 'string', description: 'if the cluster should be split or merged with another, say how; else empty' },
    permission_prompts_or_incidents: { type: 'array', items: { type: 'string' } },
  },
  required: ['cluster_id', 'root_cause', 'kind', 'owner', 'blocks_ci', 'deterministic', 'is_cascade_of', 'affected_tests',
    'failure_count', 'legs', 'evidence', 'timeline', 'recent_commits_touching_it', 'fix', 'confidence',
    'alternatives_considered', 'confirmation_plan', 'open_questions', 'split_or_merge', 'permission_prompts_or_incidents'],
}

const ANOMALY_SCHEMA = {
  type: 'object',
  properties: {
    anomalies: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          what: { type: 'string' },
          evidence: { type: 'string', description: 'numbers + log path:line' },
          related_cluster: { type: ['string', 'null'] },
          worth_a_row: { type: 'boolean', description: 'true if it deserves its own report row even though no test failed' },
        },
        required: ['what', 'evidence', 'related_cluster', 'worth_a_row'],
      },
    },
    green_legs_summary: { type: 'string' },
    permission_prompts_or_incidents: { type: 'array', items: { type: 'string' } },
  },
  required: ['anomalies', 'green_legs_summary', 'permission_prompts_or_incidents'],
}

const CONFIRM_SCHEMA = {
  type: 'object',
  properties: {
    cluster_id: { type: 'string' },
    outcome: { type: 'string', enum: ['confirmed', 'refuted', 'inconclusive', 'skipped'] },
    what_was_done: { type: 'array', items: { type: 'string' }, description: 'each command / probe, with elapsed time' },
    evidence: { type: 'array', items: { type: 'string' }, description: 'decisive output lines, verbatim' },
    revised_root_cause: { type: 'string', description: 'empty if unchanged' },
    revised_fix: { type: 'string', description: 'empty if unchanged' },
    cleanup_verified: { type: 'string', description: 'BOTH the before/after git status comparison AND the post-probe server listing filtered on the probe prefix, verbatim; "n/a" when nothing was written or created' },
    permission_prompts_or_incidents: { type: 'array', items: { type: 'string' } },
  },
  required: ['cluster_id', 'outcome', 'what_was_done', 'evidence', 'revised_root_cause', 'revised_fix', 'cleanup_verified',
    'permission_prompts_or_incidents'],
}

const REFUTE_SCHEMA = {
  type: 'object',
  properties: {
    cluster_id: { type: 'string' },
    lens: { type: 'string' },
    refuted: { type: 'boolean', description: 'true if the finding as stated should NOT go in the report unchanged' },
    verdict: { type: 'string', description: 'one paragraph: what holds, what does not' },
    corrections: { type: 'array', items: { type: 'string' }, description: 'concrete edits to root cause / kind / fix / counts' },
    alternative_root_cause: { type: 'string', description: 'empty if none survives your own check' },
    evidence_checked: { type: 'array', items: { type: 'string' }, description: 'log/source locations you read yourself' },
    permission_prompts_or_incidents: { type: 'array', items: { type: 'string' } },
  },
  required: ['cluster_id', 'lens', 'refuted', 'verdict', 'corrections', 'alternative_root_cause', 'evidence_checked',
    'permission_prompts_or_incidents'],
}

const REPORT_SCHEMA = {
  type: 'object',
  properties: {
    rows: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          id: { type: 'string' },
          issue: { type: 'string', description: 'the root cause in at most 25 words, named by mechanism not by test' },
          kind: { type: 'string', enum: ['test', 'ci', 'prod', 'backend', 'infra', 'environment'] },
          owner: { type: 'string', enum: ['us', 'backend', 'github', 'third-party'] },
          blocks_ci: { type: 'boolean' },
          failures_caused: { type: 'integer' },
          tests: { type: 'array', items: { type: 'string' } },
          legs: { type: 'array', items: { type: 'string' } },
          deterministic: { type: 'boolean' },
          recurrence: { type: 'string', description: 'e.g. "3/7 nights (09-03, 09-06)" or "first seen"' },
          confidence: { type: 'string', enum: ['confirmed', 'likely', 'hypothesis'] },
          confirmation: { type: 'string', description: 'how it was confirmed (rerun / probe / determinism + source), or what would confirm it; under 15 words' },
          evidence: { type: 'string', description: 'the one decisive quote with its log/source location' },
          fix: { type: 'string', description: 'suggested fix in one or two sentences, with path:line' },
          complexity: { type: 'string', enum: ['trivial', 'small', 'medium', 'large', 'not-ours'] },
          priority: { type: 'integer', description: '1 = do first' },
          details: { type: 'string', description: 'markdown: mechanism, evidence list, timeline, fix sketch, open questions, backend asks' },
        },
        required: ['id', 'issue', 'kind', 'owner', 'blocks_ci', 'failures_caused', 'tests', 'legs', 'deterministic', 'recurrence',
          'confidence', 'confirmation', 'evidence', 'fix', 'complexity', 'priority', 'details'],
      },
    },
    merges_or_splits_applied: { type: 'array', items: { type: 'string' } },
    unexplained: { type: 'array', items: { type: 'string' }, description: 'failures or legs no row accounts for' },
    anomalies_worth_noting: { type: 'array', items: { type: 'string' } },
    backend_asks: { type: 'array', items: { type: 'string' }, description: 'exact ids/timestamps the backend team can look up' },
    permission_prompts_or_incidents: { type: 'array', items: { type: 'string' } },
  },
  required: ['rows', 'merges_or_splits_applied', 'unexplained', 'anomalies_worth_noting', 'backend_asks',
    'permission_prompts_or_incidents'],
}

const common = `
${RULES}

${KINDS}

PATHS
  run dir (logs, failures.json, recurrence.json): ${A.run_dir}
  source at the run's commit ${A.head_sha}: ${A.tree_dir}
  main working tree (only the Confirm stage runs cargo here): ${A.repo_dir}
  scratch dir for anything you write: ${A.scratch}
  last green scheduled run: ${A.last_green_sha ?? 'unknown'}${A.baseline_run_dir ? `, its logs: ${A.baseline_run_dir}` : ''}
  timeline of every binary on every leg: python3 ${A.script_dir}/triage.py timeline ${A.run_dir} [--bin NAME]

ORCHESTRATOR NOTES
${A.notes ?? '(none)'}
`

function investigatePrompt(c) {
  return `You investigate ONE cluster of failures from a red GitHub Actions run of the filen-rs workspace and
return a structured finding. Diagnosis only; you change nothing.
${common}
CLUSTER
${clusterText(c)}

DO, in this order:
1. For every failure: Read the log (${'`'}log${'`'} path) around ${'`'}log_line${'`'}, and the captured stdout block
   ${'`'}stdout_lines${'`'} [start, end] (Read with offset=start, limit=end-start; it can be long, read it all when it is
   under ~600 lines, otherwise the head, the tail and the lines around the panic). Note the UTC timestamps.
2. Read the source at the panic site and the test body in the source tree; follow the call path into the SDK
   until you can name the mechanism. Use git -C ${A.tree_dir} grep with a pathspec, and
   git -C ${A.tree_dir} log --oneline ${A.last_green_sha ? A.last_green_sha + '..' : '-15'} -- '<crate>/*.rs' '<crate>/*.toml'
   (extension pathspecs, never a bare directory) to see what changed since the last green run.
3. Cross-leg timeline: run the timeline command (above) for the binaries involved and for the same minutes on
   the other legs. The six native legs share two test accounts (V1 legs one account, V2 legs the other, plus
   one share account) and one server-side drive-write lock per account; a stall or a 300 s TimedOut on one leg
   while another leg holds locks or floods the account is a different diagnosis than a code bug. Legs whose
   failures land in the same minutes are a cascade until shown otherwise.
4. Decide root cause, kind, owner, fix, complexity, confidence. Prefer a fix at the shared root over one per
   caller. If your fix would only silence the symptom (widen a timeout, add a retry, cfg-gate a test off a
   platform), say so and give the real fix as well.
5. Confirmation plan: what would settle it, chosen from
   source_only | local_rerun (cargo test -p <crate> --test <bin> <name> in the main tree; needs the shared
   account) | readonly_probe (a throwaway tests/probe_*.rs that only reads server state: listings, events log,
   trash) | mutating_probe (creates/trashes/restores, cleans up). Give exact commands or probe design. Allowed
   by the orchestrator right now: local_rerun=${!!A.allow?.local_rerun}, readonly_probe=${!!A.allow?.readonly_probe},
   mutating_probe=${!!A.allow?.mutating_probe}. Plan it anyway when disallowed, marked as such.
Return the finding as the structured output.`
}

const anomalyPrompt = `You scan a whole GitHub Actions run of the filen-rs workspace for anomalies that no failing test
reports: the green legs' timings, cancelled legs, slow-test warnings, retries, runner images, wasm browser
timings. Diagnosis only; you change nothing.
${common}
CLUSTERS ALREADY UNDER INVESTIGATION (relate anomalies to these ids where they belong)
${clusters.map((c) => `${c.id}: ${c.label} on ${c.legs.join(',')} (${c.failure_count})`).join('\n')}

DO:
1. Read ${A.run_dir}/failures.json: legs[] (conclusion, minutes, cancelled, in_flight, orphans, runner_image,
   annotations, binaries[] with started_at/finished_at/result.secs/slow[], browsers{} for the wasm job).
2. Run the timeline command for this run${A.baseline_run_dir ? ` and for the baseline green run ${A.baseline_run_dir} (python3 ${A.script_dir}/triage.py timeline ${A.baseline_run_dir})` : ''}.
   Flag any binary or vitest test more than 2x slower than its median across legs or than the baseline, any
   "has been running for over 60 seconds" test, any leg over 240 minutes (the kill is at 270), any
   runner image that differs between legs of the same OS, any wasm browser that did not run.
3. For each anomaly, Read the log lines that show it and quote them. Say whether it is explained by a cluster
   under investigation (e.g. lock contention from a sibling leg's binary) or is new.
Return the structured output.`

function confirmPrompt(c, inv) {
  return `You CONFIRM or REFUTE one diagnosed cluster by running things, one cluster at a time (you hold the only
slot that may touch the shared test account, so do not parallelize with yourself).
${common}
CLUSTER
${clusterText(c)}

THE INVESTIGATOR'S FINDING
${JSON.stringify(inv, null, 1)}

ALLOWED NOW: local_rerun=${!!A.allow?.local_rerun}, readonly_probe=${!!A.allow?.readonly_probe}, mutating_probe=${!!A.allow?.mutating_probe}.
(Plans of kind none / source_only and disallowed kinds never reach you. If the plan you were given turns out
to be impossible or pointless, return outcome 'skipped' with the reason.)

HOW TO RUN THINGS
- cargo, always package-scoped and from the main tree, never a workspace-root command (that builds the
  vendored C++ decoders). Subcommand first, manifest after it:
    cargo test --manifest-path ${A.repo_dir}/Cargo.toml -p <crate> --test <bin> <test name> -- --nocapture
  Features: cache_tests / cache_search_tests need -F cache (required-features); tests that call
  create_malformed_* need -F malformed; the nightly itself ran with -F filen-sdk-rs/malformed,filen-sdk-rs/heif-decoder,
  so add -F heif-decoder only when the test decodes HEIF/AVIF (it builds vendored C++). For dir_tests always add
  --skip size. Bash timeout: 600000 ms; if a build or a test needs longer, run it with run_in_background and
  wait for its completion notification, do not poll with sleep.
- The main tree may be at a different commit than the run (${A.head_sha}); say so in evidence if
  git -C ${A.repo_dir} rev-parse HEAD differs, and check
  git -C ${A.repo_dir} diff --stat ${A.head_sha} -- '<crate>/*.rs' '<crate>/*.toml' before trusting a local pass.
- Probes: first snapshot git -C ${A.repo_dir} status --porcelain into ${A.scratch}/status-before-${c.id}.txt. Write
  ${A.repo_dir}/filen-sdk-rs/tests/probe_${c.id}.rs, copying the setup of an existing test in that directory (Read
  one first: they use the shared RESOURCES from test-utils and the server locks). A probe that writes takes
  the same server lock the tests take for that resource (drive-write via lock_drive, or the test:* resources),
  names everything it creates with the prefix probe-${c.id}-, and awaits its own cleanup explicitly inside the
  test body, run whether or not the observation succeeded (gather observations, delete/trash-empty what the
  probe made, THEN assert — TestResources::drop only spawns the cleanup fire-and-forget on a static runtime and
  the process can exit before it lands). Print findings with eprintln! and run with --nocapture. When done:
  delete the probe file, re-run git status --porcelain and show it is identical to the snapshot, and paste a
  read-only listing of the account root and trash filtered on probe-${c.id}- (must be empty) — both go in
  cleanup_verified verbatim.
- wasm (vitest browser-mode) probes: never write into ${A.repo_dir}/filen-sdk-rs/web. Build a scratch vitest
  project under ${A.scratch}/probe-${c.id}/ (symlink the web dir's node_modules, cacheDir under scratch) and run it
  with absolute --root/--config; it needs the existing web build (check for a built package dir first — a
  fresh wasm-pack build needs wasi-sdk and is not yours to start). Kill any vitest/playwright process you
  leave behind, and say so.
- A local pass of a test that failed in CI is weak evidence unless you reproduce the CI condition (contention,
  timing, platform); say which condition you could not reproduce.
Return the structured output.`
}

const LENSES = {
  evidence: `EVIDENCE lens: does the quoted log/source evidence actually support the root cause? Re-read the log
lines and the source yourself (Read tool, git -C grep with pathspec). Check the timeline claims against the
timestamps. Find the strongest alternative explanation and say what evidence would separate the two. A
root cause that fits the failing test but not the passing legs, or not the passing siblings, is refuted.`,
  fix: `FIX lens: is the suggested fix correct, complete and at the root? Read the code it touches and every
caller of the function it changes (git -C grep with pathspec). Would it mask a real bug (timeout widened,
retry added, test gated off a platform, assert weakened)? Is there a smaller fix at a shared root? Is the
complexity honest? Would it break the other platforms or the wasm build?`,
  classification: `CLASSIFICATION lens: kind, owner, blocks_ci, deterministic, failure count, cascade membership. Is a
shared-account contention failure being excused as infra when this repo treats flakiness as a test bug to
fix (only true server rate limiting is exempt)? Should the cluster be split (two mechanisms) or merged with
another (same mechanism, e.g. every 300 s TimedOut on v3/dir/create in the same window)? Count the failures
again from the cluster data.`,
}

function refutePrompt(c, inv, conf, lens) {
  return `You are an adversarial reviewer of ONE finding from a CI-run triage. Your job is to REFUTE it; default
to refuted=true when you cannot verify a load-bearing claim yourself. Diagnosis only; you change nothing.
${common}
${LENSES[lens]}
You verify by READING (logs, source, the timeline command) only. Never run cargo, a test binary, filen-cli or
anything that reaches the Filen server: the Confirm agent holds the only slot for that, and running a test in
parallel with it corrupts both results. A claim you cannot verify by reading is "unverified", not "refuted
because I could not rerun it".

CLUSTER
${clusterText(c)}

FINDING
${JSON.stringify(inv, null, 1)}

LIVE CONFIRMATION RESULT
${JSON.stringify(conf, null, 1)}

OTHER CLUSTERS IN THIS RUN (for split/merge questions)
${clusters.map((x) => `${x.id}: ${x.label} on ${x.legs.join(',')} (${x.failure_count}) sites=${x.sites.join(' ')}`).join('\n')}

Return the structured output with lens="${lens}".`
}

function judgePrompt(findings, anomalies) {
  return `You are the judge that turns per-cluster investigation, live confirmation and adversarial review into
the final rows of a triage report for one GitHub Actions run of the filen-rs workspace. You change nothing.
${common}
RUN TOTALS: ${TOTAL} failure records across the legs (failed tests, aborted binaries, CI step errors). Every one must be accounted for by exactly one row
(rows may carry a cascade: a setup failure that hit N tests counts N). List anything left in "unexplained".

RECURRENCE (previous scheduled nights): ${A.recurrence_text ?? '(not computed)'}

FINDINGS (investigator + confirm + three refuters each)
${JSON.stringify(findings, null, 1)}

WHOLE-RUN ANOMALY SCAN
${JSON.stringify(anomalies, null, 1)}

RULES OF JUDGEMENT
- A refuter's correction wins over the investigator when the refuter quotes evidence; when refuters
  disagree with each other, read the evidence yourself (Read tool, git -C grep with pathspec) and decide.
- Confidence goes DOWN one notch when any refuter with evidence refuted a load-bearing claim and it was
  not answered; it is "confirmed" only with a reproduction, a probe, or determinism across legs plus the
  mechanism read in source.
- Merge rows that share a mechanism (say so in merges_or_splits_applied); split rows that hold two.
- issue = the mechanism in one sentence (not the test name); fix = concrete, with REPO-RELATIVE path:line
  (filen-cli/tests/x.rs:12, never the worktree or scratch prefix); complexity per the scale; legs = the
  short names failures.json uses (ubuntu-V1, wasm, ci-macos); priority: what blocks ci.yml first, then prod
  bugs, then deterministic test bugs, then contention/test-harness bugs, then backend asks, then
  infra/environment. A cause that failed no test in THIS run but blocks CI on the same commit gets a row
  with failures_caused = 0.
- details is the per-row markdown section a maintainer reads: mechanism, evidence (log path:line quotes),
  timeline, fix sketch, what would confirm further, open questions, exact backend asks (ids, timestamps).
Return the structured output.`
}

// ---- run -------------------------------------------------------------------------------------

log(`triage: ${clusters.length} cluster(s), ${TOTAL} failure records, run ${A.run_dir}`)

let chain = Promise.resolve()
const serialized = (fn) => {
  const p = chain.then(fn, fn)
  chain = p.catch(() => {})
  return p
}

const anomalyTask = agent(anomalyPrompt, { label: 'anomaly-scan', phase: 'Investigate', model: 'sonnet', schema: ANOMALY_SCHEMA })

const perCluster = await pipeline(
  clusters,
  (c) => agent(investigatePrompt(c), { label: `investigate:${c.id}`, phase: 'Investigate', model: 'sonnet', schema: INVESTIGATE_SCHEMA }),
  (inv, c) => {
    if (!inv) return null
    const kind = inv.confirmation_plan?.kind
    const allowed = kind === 'local_rerun' ? A.allow?.local_rerun : kind === 'readonly_probe' ? A.allow?.readonly_probe : kind === 'mutating_probe' ? A.allow?.mutating_probe : false
    if (!allowed) {
      log(`${c.id}: confirmation '${kind}' not run (${kind === 'none' || kind === 'source_only' ? 'nothing live to do' : 'not allowed'})`)
      return { inv, conf: { cluster_id: c.id, outcome: 'skipped', what_was_done: [], evidence: [], revised_root_cause: '', revised_fix: '', cleanup_verified: '', permission_prompts_or_incidents: [] } }
    }
    return serialized(() => agent(confirmPrompt(c, inv), { label: `confirm:${c.id}`, phase: 'Confirm', model: 'opus', schema: CONFIRM_SCHEMA }))
      .then((conf) => ({ inv, conf }), (err) => { log(`${c.id}: confirm threw (${String(err).slice(0, 120)}); continuing without it`); return { inv, conf: null } })
  },
  (r, c) => {
    if (!r) return null
    return parallel(Object.keys(LENSES).map((lens) => () =>
      agent(refutePrompt(c, r.inv, r.conf, lens), { label: `refute:${c.id}:${lens}`, phase: 'Refute', model: 'fable', schema: REFUTE_SCHEMA })))
      .then((refs) => ({ cluster: c, investigation: r.inv, confirmation: r.conf, refutations: refs.filter(Boolean) }))
  },
)

const anomalies = await anomalyTask
const findings = perCluster.filter(Boolean)
const dropped = clusters.filter((c) => !findings.some((f) => f.cluster.id === c.id)).map((c) => c.id)
if (dropped.length) log(`WARNING: no finding for cluster(s) ${dropped.join(', ')} (agent skipped or died); the judge will list them as unexplained`)

phase('Synthesize')
const report = await agent(judgePrompt(findings, anomalies), { label: 'judge', phase: 'Synthesize', model: 'opus', schema: REPORT_SCHEMA })

const counted = (report?.rows ?? []).reduce((n, r) => n + (r.failures_caused || 0), 0)
if (report && counted !== TOTAL) log(`WARNING: rows account for ${counted} failures, the run had ${TOTAL}; check 'unexplained' and the cascade counts`)
const prompts = [
  ...findings.flatMap((f) => [
    ...(f.investigation?.permission_prompts_or_incidents ?? []),
    ...(f.confirmation?.permission_prompts_or_incidents ?? []),
    ...f.refutations.flatMap((r) => r.permission_prompts_or_incidents ?? []),
  ]),
  ...(anomalies?.permission_prompts_or_incidents ?? []),
  ...(report?.permission_prompts_or_incidents ?? []),
]
if (prompts.length) log(`permission prompts / incidents reported by agents: ${prompts.length}`)

// Slim return value: the orchestrator has to write it to result.json by hand for `triage.py render`.
// The full per-agent objects live in the run's journal.jsonl.
const findings_summary = findings.map((f) => ({
  cluster: f.cluster.id,
  investigator: { kind: f.investigation.kind, confidence: f.investigation.confidence, root_cause: f.investigation.root_cause, plan: f.investigation.confirmation_plan?.kind },
  confirmation: f.confirmation ? { outcome: f.confirmation.outcome, evidence: (f.confirmation.evidence ?? []).slice(0, 3) } : null,
  refuters: f.refutations.map((r) => ({ lens: r.lens, refuted: r.refuted, verdict: r.verdict })),
}))
return { report, findings_summary, anomalies, dropped_clusters: dropped, counted_failures: counted, total_failures: TOTAL, permission_prompts_or_incidents: prompts }
