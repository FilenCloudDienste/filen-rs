#!/usr/bin/env python3
"""Fetch and parse one GitHub Actions run of this repo for triage.

  triage.py fetch <run-id|run-url> --out DIR        run + jobs + per-job logs + annotations -> DIR/<run-id>/
  triage.py failures <run-dir>                      parse the logs -> failures.json, clusters.suggested.json (+ clusters.json once)
  triage.py timeline <run-dir> [--bin NAME]         per-binary wall-clock table across legs (from failures.json)
  triage.py recurrence <run-dir> [--nights N]       the run's failures in the N previous runs of the same event (fetched, cached)
  triage.py render <run-dir> <result.json>          the workflow's return value -> report.md

stdlib only; `gh` must be logged in for fetch/recurrence. Never cd's; every path is absolute.
Logs come from the per-job endpoint (actions/jobs/<id>/logs), never `gh run view --log`, which has been
seen splicing stale blocks of other jobs' logs into a job's output.
"""

import argparse
import json
import re
import subprocess
import sys
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve()
REPO_DIR = HERE.parents[4]  # <repo>/.claude/skills/<skill>/scripts/triage.py
DEFAULT_SLUG = "FilenCloudDienste/filen-rs"

ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
TS_LINE = re.compile(r"^\ufeff?(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+Z) ?(.*)$")
# GitHub masks every secret value; one of this repo's secrets is the literal "filen", so paths arrive as
# "***-sdk-rs/tests/x.rs". Restore the token where it is glued to a path/name character.
MASK = re.compile(r"\*\*\*(?=[-_./])")

RUNNING_HDR = re.compile(r"^\s*Running (unittests )?(\S+) \((\S+)\)$")
DOCTEST_HDR = re.compile(r"^\s*Doc-tests (\S+)$")
RUNNING_N = re.compile(r"^running (\d+) tests?$")
# Doctest names carry spaces ("test filen-macros/src/lib.rs - rkyv_self (line 324) ... ignored"): non-greedy.
TEST_LINE = re.compile(r"^test (.+?)(?: - should panic)? \.\.\. (ok|FAILED|ignored.*|bench.*)$")
SLOW_LINE = re.compile(r"^test (.+?) has been running for over (\d+) seconds$")
RESULT = re.compile(
    r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; "
    r"(\d+) filtered out; finished in ([\d.]+)s$"
)
STDOUT_HDR = re.compile(r"^---- (.+?) stdout ----$")
PANIC = re.compile(r"^thread '([^']+)'(?: \(\d+\))? panicked at (.+?):(\d+):(\d+):$")
RERUN = re.compile(r"^error: test failed, to rerun pass `(.+)`$")
ABORT_LINE = re.compile(r"process didn't exit successfully: .*")
ORPHAN = re.compile(r"^Terminate orphan process: pid \((\d+)\) \((.+)\)$")
CANCELLED = "##[error]The operation was canceled."
GH_ERROR = re.compile(r"^##\[error\](.*)$")
RUSTC_ERROR = re.compile(r"^(error(?:\[E\d+\])?: .*)$")
RUSTC_CONSEQUENCE = ("error: could not compile", "error: aborting due to", "error: test failed",
                     "error: process didn't exit successfully", "error: build failed")
RUSTC_LOC = re.compile(r"^\s*--> (\S+)$")
FMT_DIFF = re.compile(r"^(Diff in (\S+?):(\d+)):")
LOC_SPLIT = re.compile(r"^(.+?):(\d+)(?::\d+)?$")

VT_BROWSER = re.compile(
    r"^\s*[✓❯×]\s+(chromium|firefox|webkit)\s+(\S+) \((\d+) tests?(?: \| (\d+) failed)?(?: \| (\d+) skipped)?\)\s+(\d+)ms$"
)
VT_TEST = re.compile(r"^\s{2,}([✓×↓])\s(.+?)\s*(\d+)ms$")
VT_FAIL = re.compile(r"^\s*FAIL\s+(chromium|firefox|webkit)\s+(\S+) > (.+?)\s*$")
VT_LOC = re.compile(r"^\s*❯ (\S+):(\d+):(\d+)$")
VT_SUMMARY = re.compile(r"^\s*Tests\s+(?:(\d+) failed \| )?(\d+) passed(?: \| (\d+) skipped)? \((\d+)\)$")


# ----------------------------------------------------------------------------- helpers


def gh_api(slug_path, *extra, raw=False):
    p = subprocess.run(["gh", "api", slug_path, *extra], capture_output=True)
    if p.returncode != 0:
        return None, (p.stderr.decode(errors="replace") + p.stdout.decode(errors="replace"))[:400]
    if raw:
        return p.stdout, None
    return json.loads(p.stdout), None


def run_id_of(s):
    m = re.search(r"/runs/(\d+)", s) or re.fullmatch(r"\s*(\d+)\s*", s)
    if not m:
        sys.exit(f"not a run id or run url: {s}")
    return m.group(1)


def short_name(name):
    m = re.match(r"test \((\w+)-latest, (V\d)\)", name)
    if m:
        return f"{m.group(1)}-{m.group(2)}"
    m = re.match(r"test-2fa \((V\d)\)", name)
    if m:
        return f"2fa-{m.group(1)}"
    if name == "test-wasm":
        return "wasm"
    m = re.match(r"CI on (\w+)", name)
    if m:
        return f"ci-{m.group(1)}"
    return re.sub(r"[^\w.-]+", "-", name).strip("-")


def iso(s):
    return datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc) if s else None


def ts_of(s):
    return datetime.strptime(s[:19], "%Y-%m-%dT%H:%M:%S").replace(tzinfo=timezone.utc)


def hhmm(s):
    return s[11:16] if s else "--:--"


def minutes(a, b):
    if not a or not b:
        return None
    return round((iso(b) - iso(a)).total_seconds() / 60)


_tree_files = {}


def tree_files(sha):
    """Every tracked path at the run's commit (falls back to the working tree if the sha is not fetched yet)."""
    if sha not in _tree_files:
        p = subprocess.run(["git", "-C", str(REPO_DIR), "ls-tree", "-r", "--name-only", sha], capture_output=True,
                           text=True)
        if p.returncode != 0:
            print(f"note: commit {sha[:8]} is not in the local repo (run `git fetch origin`); "
                  f"crate names come from the working tree instead", file=sys.stderr)
            p = subprocess.run(["git", "-C", str(REPO_DIR), "ls-files"], capture_output=True, text=True)
        _tree_files[sha] = p.stdout.splitlines()
    return _tree_files[sha]


def crate_of_test_bin(bin_name, sha):
    """tests/<bin>.rs lives in exactly one workspace crate."""
    hits = [f for f in tree_files(sha) if f.endswith(f"/tests/{bin_name}.rs")]
    return hits[0].split("/")[0] if len(hits) == 1 else None


def clean_lines(path):
    """[(timestamp, text)] with ANSI stripped and the masked 'filen' token restored."""
    out = []
    with open(path, encoding="utf-8", errors="replace") as fh:
        for raw in fh:
            raw = raw.rstrip("\n")
            m = TS_LINE.match(raw)
            if not m:
                out.append((out[-1][0] if out else "", MASK.sub("filen", ANSI.sub("", raw))))
                continue
            out.append((m.group(1), MASK.sub("filen", ANSI.sub("", m.group(2)))))
    return out


# ----------------------------------------------------------------------------- fetch


def load_jobs(slug, rid):
    pages, err = gh_api(f"/repos/{slug}/actions/runs/{rid}/jobs?per_page=100", "--paginate", "--slurp")
    if err:
        return None, err
    jobs = []
    for page in pages if isinstance(pages, list) else [pages]:
        jobs.extend(page.get("jobs", []))
    return jobs, None


def fetch_run(slug, rid, out_root, force=False, quiet=False):
    out = Path(out_root) / rid
    out.mkdir(parents=True, exist_ok=True)
    run_path = out / "run.json"
    if force or not run_path.exists():
        run, err = gh_api(f"/repos/{slug}/actions/runs/{rid}")
        if err:
            sys.exit(f"run {rid}: {err}")
        run_path.write_text(json.dumps(run, indent=1))
    run = json.loads(run_path.read_text())
    jobs_path = out / "jobs.json"
    if force or not jobs_path.exists():
        jobs, err = load_jobs(slug, rid)
        if err:
            sys.exit(f"jobs of {rid}: {err}")
        jobs_path.write_text(json.dumps(jobs, indent=1))
    jobs = json.loads(jobs_path.read_text())
    for j in jobs:
        jid = j["id"]
        logp = out / f"{jid}.log"
        missing = out / f"{jid}.log.missing"
        if force or not (logp.exists() and logp.stat().st_size > 0) and not missing.exists():
            raw, err = gh_api(f"/repos/{slug}/actions/jobs/{jid}/logs", raw=True)
            if err:
                missing.write_text(err)  # BlobNotFound = runner lost / log expired: nothing to parse
            else:
                logp.write_bytes(raw)
                if missing.exists():
                    missing.unlink()
        annp = out / f"{jid}.annotations.json"
        if force or not annp.exists():
            ann, err = gh_api(f"/repos/{slug}/check-runs/{jid}/annotations")
            annp.write_text(json.dumps(ann if not err else [], indent=1))
    if not quiet:
        print(f"run {rid}  {run['name']}  {run['event']}  {run['head_branch']}@{run['head_sha'][:8]}  "
              f"{run['conclusion']}  created {run['created_at']}  attempt {run['run_attempt']}")
        print(f"  {run['html_url']}")
        print(f"  saved under {out}")
        print(f"  {'leg':<14}{'conclusion':<11}{'start':<7}{'end':<7}{'min':>5}  failed step / log")
        for j in jobs:
            failed_steps = [s["name"] for s in j["steps"] if s["conclusion"] not in ("success", "skipped", None)]
            logstate = "log ok" if (out / f"{j['id']}.log").exists() else "NO LOG"
            print(f"  {short_name(j['name']):<14}{str(j['conclusion']):<11}{hhmm(j['started_at']):<7}"
                  f"{hhmm(j['completed_at']):<7}{str(minutes(j['started_at'], j['completed_at'])):>5}  "
                  f"{', '.join(failed_steps) or '-'} / {logstate}")
    return out


# ----------------------------------------------------------------------------- parse


def parse_native(lines, leg, sha):
    """libtest output of `cargo test` (one leg): binaries with per-test status, failures with panic sites."""
    binaries, failures = [], []
    # cargo's `Running` headers (stderr) and libtest's `running N tests` (stdout) reach the log out of order,
    # sometimes two headers before the first `running`; binaries run sequentially, so bind them FIFO.
    announced = []
    cur = None  # binary whose results are being printed
    stdout_test, stdout_start, panic_pending = None, None, None
    slow = []
    orphans = []
    cancelled = False
    runner_image = {}
    in_image_group = False
    for idx, (ts, text) in enumerate(lines):
        if text.startswith("##[group]Runner Image"):
            in_image_group = True
            continue
        if in_image_group:
            if text.startswith("##[endgroup]"):
                in_image_group = False
            else:
                k, _, v = text.partition(":")
                runner_image[k.strip()] = v.strip()
            continue
        if text == CANCELLED:
            cancelled = True
            continue
        m = ORPHAN.match(text)
        if m:
            orphans.append(m.group(2))
            continue
        m = RUNNING_HDR.match(text)
        if m:
            unit, path, exe = m.group(1), m.group(2).replace("\\", "/"), m.group(3).replace("\\", "/")
            dep = re.sub(r"-[0-9a-f]{16}(\.exe)?$", "", exe.rsplit("/", 1)[-1])
            if unit:
                kind, bin_name, crate = "unittests", dep, dep.replace("_", "-")
                if path.endswith("main.rs"):
                    bin_name += " (main)"
            else:
                kind, bin_name = "tests", Path(path).stem
                crate = crate_of_test_bin(bin_name, sha)
            b = {"bin": bin_name, "kind": kind, "crate": crate, "path": path, "leg": leg,
                 "announced_at": ts, "log_line": idx + 1, "tests": {}, "slow": [], "result": None}
            binaries.append(b)
            announced.append(b)
            continue
        m = DOCTEST_HDR.match(text)
        if m:
            b = {"bin": m.group(1), "kind": "doctest", "crate": m.group(1).replace("_", "-"), "path": None,
                 "leg": leg, "announced_at": ts, "log_line": idx + 1, "tests": {}, "slow": [], "result": None}
            binaries.append(b)
            announced.append(b)
            continue
        m = RUNNING_N.match(text)
        if m and announced:
            cur = announced.pop(0)
            cur["started_at"] = ts
            cur["declared"] = int(m.group(1))
            continue
        m = TEST_LINE.match(text)
        if m and cur is not None:
            name, status = m.group(1), m.group(2)
            cur["tests"][name] = "ignored" if status.startswith("ignored") else status
            if status == "FAILED":
                failures.append({"leg": leg, "bin": cur["bin"], "kind": cur["kind"], "crate": cur["crate"],
                                 "test": name, "ts": ts, "log_line": idx + 1, "panic_file": None,
                                 "panic_line": None, "message": "", "stdout_lines": None})
            continue
        m = SLOW_LINE.match(text)
        if m and cur is not None:
            cur["slow"].append({"test": m.group(1), "over_secs": int(m.group(2)), "ts": ts})
            continue
        m = STDOUT_HDR.match(text)
        if m:
            if stdout_test is not None:
                _close_stdout(failures, cur, stdout_test, stdout_start, idx)
            stdout_test, stdout_start = m.group(1), idx + 1
            continue
        if stdout_test is not None and text == "failures:":
            _close_stdout(failures, cur, stdout_test, stdout_start, idx)
            stdout_test = None
        m = PANIC.match(text)
        if m and cur is not None:
            file = m.group(2).replace("\\", "/")
            file = re.sub(r"^.*/rustlib/src/rust/", "<rust>/", file)
            file = re.sub(r"^.*/\.cargo/(registry/src/[^/]+|git/checkouts)/", "<cargo>/", file)
            msg = []
            for _, t in lines[idx + 1: idx + 6]:
                if not t.strip() or t.startswith("note: run with") or t.startswith("stack backtrace"):
                    break
                msg.append(t.strip())
            rec = _failure_for(failures, cur, m.group(1))
            if rec is not None:
                rec["panic_file"], rec["panic_line"] = file, int(m.group(3))
                rec["message"] = " | ".join(msg)[:300]
                rec["panic_log_line"] = idx + 1
            continue
        m = RERUN.match(text)
        if m:
            cmd = m.group(1)
            cm = re.search(r"-p (\S+)", cmd)
            bm = re.search(r"--(test|bin|lib)(?: (\S+))?", cmd)
            for b in reversed(binaries):
                if b["result"] is None or b["result"]["failed"]:
                    if bm and bm.group(1) == "test" and b["bin"] != bm.group(2):
                        continue
                    if cm:
                        b["crate"] = cm.group(1)
                        for f in failures:
                            if f["leg"] == leg and f["bin"] == b["bin"] and f["kind"] == b["kind"]:
                                f["crate"] = cm.group(1)
                    b["rerun"] = cmd
                    break
            continue
        m = RESULT.match(text)
        if m and cur is not None:
            cur["result"] = {"status": m.group(1), "passed": int(m.group(2)), "failed": int(m.group(3)),
                             "ignored": int(m.group(4)), "secs": float(m.group(7))}
            cur["finished_at"] = ts
            parsed_failed = sum(1 for s in cur["tests"].values() if s == "FAILED")
            if parsed_failed != cur["result"]["failed"]:
                cur["mismatch"] = f"binary reports {cur['result']['failed']} failed, parsed {parsed_failed} FAILED lines"
            cur = None
            continue
    if stdout_test is not None:
        _close_stdout(failures, cur, stdout_test, stdout_start, len(lines))
    in_flight = None
    for i, b in enumerate(binaries):
        if b["result"] is not None or not b.get("started_at"):
            continue
        done = set(b["tests"])
        if cancelled:
            in_flight = {"bin": b["bin"], "crate": b["crate"], "kind": b["kind"], "started_at": b["started_at"],
                         "declared": b.get("declared"), "reported": len(done),
                         "slow": [s for s in b["slow"] if s["test"] not in done]}
            continue
        # Started, never printed `test result:`, and the leg was not killed: the process died (signal, abort,
        # OOM). cargo's stderr names it; libtest prints no FAILED line, so make one.
        end = binaries[i + 1]["log_line"] + 400 if i + 1 < len(binaries) else len(lines)
        msg, ts_last = "", b["started_at"]
        for ts2, t in lines[b["log_line"]: min(end, len(lines))]:
            ts_last = ts2 or ts_last
            am = ABORT_LINE.search(t)
            if am or t.startswith("error: test failed"):
                msg = (am.group(0) if am else t).strip()
                break
        b["aborted"] = True
        failures.append({"leg": leg, "bin": b["bin"], "kind": b["kind"], "crate": b["crate"],
                         "test": "<binary aborted>", "ts": ts_last, "log_line": b["log_line"], "panic_file": None,
                         "panic_line": None, "message": msg or f"no `test result:` line; {len(done)}/{b.get('declared')} tests reported",
                         "stdout_lines": None, "aborted": True})
    return {"binaries": binaries, "failures": failures, "cancelled": cancelled, "orphans": orphans,
            "in_flight": in_flight, "runner_image": runner_image}


def _failure_for(failures, cur, test):
    for f in reversed(failures):
        if f["test"] == test and (cur is None or (f["bin"] == cur["bin"] and f["kind"] == cur["kind"])):
            return f
    return None


def _close_stdout(failures, cur, test, start, end):
    rec = _failure_for(failures, cur, test)
    if rec is not None:
        rec["stdout_lines"] = [start, end]


def parse_vitest(lines, leg):
    """vitest browser-mode output of the wasm job: per-browser test durations + the Failed Tests block."""
    browsers, failures = {}, []
    cur_browser = None
    seen_fail = None
    for idx, (ts, text) in enumerate(lines):
        m = VT_BROWSER.match(text)
        if m:
            cur_browser = m.group(1)
            browsers[cur_browser] = {"file": m.group(2), "declared": int(m.group(3)),
                                     "failed": int(m.group(4) or 0), "total_ms": int(m.group(6)),
                                     "finished_at": ts, "tests": {}, "log_line": idx + 1}
            continue
        m = VT_TEST.match(text)
        if m and cur_browser:
            mark, name, ms = m.group(1), m.group(2).strip(), int(m.group(3))
            browsers[cur_browser]["tests"][name] = {"status": {"✓": "ok", "×": "FAILED", "↓": "skipped"}[mark], "ms": ms}
            continue
        m = VT_FAIL.match(text)
        if m:
            browser, name = m.group(1), m.group(3)
            msg = ""
            for _, t in lines[idx + 1: idx + 4]:
                if t.strip():
                    msg = t.strip()
                    break
            seen_fail = {"leg": leg, "bin": f"vitest:{browser}", "kind": "vitest", "crate": "filen-sdk-rs/web",
                         "test": f"{browser} > {name}", "ts": ts, "log_line": idx + 1, "panic_file": None,
                         "panic_line": None, "message": msg[:300], "stdout_lines": None}
            failures.append(seen_fail)
            continue
        m = VT_LOC.match(text)
        if m and seen_fail is not None and seen_fail["panic_file"] is None:
            seen_fail["panic_file"] = f"filen-sdk-rs/web/{m.group(1)}"
            seen_fail["panic_line"] = int(m.group(2))
            continue
        m = VT_SUMMARY.match(text)
        if m and cur_browser:
            browsers[cur_browser]["summary"] = {"failed": int(m.group(1) or 0), "passed": int(m.group(2)),
                                                "skipped": int(m.group(3) or 0), "total": int(m.group(4))}
            cur_browser = None
    return {"browsers": browsers, "failures": failures}


def step_errors(lines):
    """Failed jobs without test failures (clippy, fmt, a build script): the ##[error], rustc error and
    rustfmt `Diff in` lines, minus the consequence lines (could not compile / aborting due to)."""
    out = []
    for idx, (ts, text) in enumerate(lines):
        m = GH_ERROR.match(text)
        if m and not m.group(1).startswith("Process completed with exit code"):
            out.append({"ts": ts, "log_line": idx + 1, "text": m.group(1)[:300], "location": ""})
            continue
        m = FMT_DIFF.match(text)
        if m:
            out.append({"ts": ts, "log_line": idx + 1, "text": m.group(1)[:300], "location": f"{m.group(2)}:{m.group(3)}"})
            continue
        m = RUSTC_ERROR.match(text)
        if m and not text.startswith(RUSTC_CONSEQUENCE):
            loc = ""
            for _, t in lines[idx + 1: idx + 4]:
                lm = RUSTC_LOC.match(t)
                if lm:
                    loc = lm.group(1).replace("\\", "/")
                    break
            out.append({"ts": ts, "log_line": idx + 1, "text": m.group(1)[:300], "location": loc})
        if len(out) >= 40:
            out.append({"ts": ts, "log_line": idx + 1, "text": "... (capped at 40)", "location": ""})
            break
    return out


def step_error_failures(leg):
    """A CI leg's step errors as failure records, so clustering, totals and the workflow treat them like tests."""
    out = []
    for e in leg["step_errors"]:
        if e["text"].startswith("..."):
            continue
        lm = LOC_SPLIT.match(e["location"]) if e["location"] else None
        out.append({"leg": leg["short"], "bin": leg["failed_steps"][0] if leg["failed_steps"] else "step",
                    "kind": "step", "crate": None, "test": e["text"][:100], "ts": e["ts"], "log_line": e["log_line"],
                    "panic_file": lm.group(1) if lm else None, "panic_line": int(lm.group(2)) if lm else None,
                    "message": e["text"], "stdout_lines": None})
    return out


def parse_run(run_dir):
    d = Path(run_dir)
    run = json.loads((d / "run.json").read_text())
    jobs = json.loads((d / "jobs.json").read_text())
    legs = []
    for j in sorted(jobs, key=lambda j: short_name(j["name"])):
        jid = j["id"]
        leg = {"job_id": jid, "name": j["name"], "short": short_name(j["name"]), "conclusion": j["conclusion"],
               "started_at": j["started_at"], "completed_at": j["completed_at"],
               "minutes": minutes(j["started_at"], j["completed_at"]),
               "failed_steps": [s["name"] for s in j["steps"] if s["conclusion"] not in ("success", "skipped", None)],
               "log": None, "annotations": [], "binaries": [], "browsers": {}, "failures": [],
               "cancelled": False, "in_flight": None, "orphans": [], "runner_image": {}, "step_errors": []}
        annp = d / f"{jid}.annotations.json"
        if annp.exists():
            leg["annotations"] = [{"level": a["annotation_level"], "message": a["message"][:300]}
                                  for a in json.loads(annp.read_text())
                                  if a["annotation_level"] != "warning" or "lost communication" in a["message"]]
        logp = d / f"{jid}.log"
        if not logp.exists():
            leg["log"] = "missing"
            legs.append(leg)
            continue
        leg["log"] = str(logp)
        lines = clean_lines(logp)
        nat = parse_native(lines, leg["short"], run["head_sha"])
        vt = parse_vitest(lines, leg["short"])
        leg.update({k: nat[k] for k in ("binaries", "cancelled", "orphans", "in_flight", "runner_image")})
        leg["browsers"] = vt["browsers"]
        leg["failures"] = nat["failures"] + vt["failures"]
        if leg["conclusion"] not in ("success", "skipped") and not leg["failures"] and not leg["cancelled"]:
            leg["step_errors"] = step_errors(lines)
            leg["failures"] = step_error_failures(leg)
        legs.append(leg)
    failed_tests = sum(len(l["failures"]) for l in legs)
    totals = {"legs": len(legs), "legs_failed": sum(l["conclusion"] == "failure" for l in legs),
              "legs_cancelled": sum(l["conclusion"] == "cancelled" for l in legs),
              "legs_no_log": sum(l["log"] == "missing" for l in legs), "failed_tests": failed_tests}
    return {"run": {k: run.get(k) for k in ("id", "name", "path", "event", "head_branch", "head_sha", "conclusion",
                                            "created_at", "updated_at", "run_attempt", "html_url")},
            "legs": legs, "totals": totals, "suggested_clusters": suggest_clusters(legs)}


def message_head(m):
    """Message with the boilerplate, ids and numbers removed: what two failures must share to be 'the same message'."""
    m = m.replace("called `Result::unwrap()` on an `Err` value: ", "").replace("***", "")
    m = re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "#", m)
    m = re.sub(r"\d+", "#", m)
    return re.sub(r"\s+", " ", m).strip()[:70]


def suggest_clusters(legs):
    """Mechanical pre-grouping: same test name -> one group; groups sharing a panic site or message head merge.
    Over-merges on generic sites (test-utils helpers, core::ops::function) and over-splits cascades: a starting
    point for the human/agent clustering, not the answer."""
    by_test = defaultdict(list)
    for leg in legs:
        for f in leg["failures"]:
            by_test[(f["crate"], f["bin"], f["test"])].append(f)
    groups = []
    for key, fs in by_test.items():
        sites = {f"{f['panic_file']}:{f['panic_line']}" for f in fs if f["panic_file"]}
        heads = {message_head(f["message"]) for f in fs if f["message"]}
        groups.append({"tests": [key], "failures": fs, "sites": sites, "heads": heads})
    merged = True
    while merged:
        merged = False
        for i in range(len(groups)):
            for k in range(i + 1, len(groups)):
                a, b = groups[i], groups[k]
                if (a["sites"] & b["sites"]) or (a["heads"] & b["heads"]):
                    a["tests"] += b["tests"]
                    a["failures"] += b["failures"]
                    a["sites"] |= b["sites"]
                    a["heads"] |= b["heads"]
                    del groups[k]
                    merged = True
                    break
            if merged:
                break
    out = []
    log_of = {leg["short"]: leg["log"] for leg in legs}
    for i, g in enumerate(sorted(groups, key=lambda g: -len(g["failures"])), 1):
        legs_hit = sorted({f["leg"] for f in g["failures"]})
        out.append({"id": f"c{i}", "label": f"{g['tests'][0][2]}",
                    "tests": [f"{c or '?'}/{b}::{t}" for c, b, t in g["tests"]], "failure_count": len(g["failures"]),
                    "legs": legs_hit, "sites": sorted(g["sites"]), "message_heads": sorted(g["heads"]),
                    "first_ts": min(f["ts"] for f in g["failures"]), "last_ts": max(f["ts"] for f in g["failures"]),
                    "failures": [{"leg": f["leg"], "test": f"{f['crate'] or '?'}/{f['bin']}::{f['test']}",
                                  "ts": f["ts"], "log": log_of.get(f["leg"]), "log_line": f["log_line"],
                                  "stdout_lines": f.get("stdout_lines"), "panic_file": f["panic_file"],
                                  "panic_line": f["panic_line"], "message": f["message"]} for f in g["failures"]]})
    return out


def cmd_failures(args):
    d = Path(args.run_dir)
    data = parse_run(d)
    (d / "failures.json").write_text(json.dumps(data, indent=1))
    # The starting point for the by-hand clustering. clusters.json is the operator's file: written once,
    # never overwritten (a re-parse must not undo the by-hand merges/splits); the fresh grouping always
    # lands in clusters.suggested.json.
    (d / "clusters.suggested.json").write_text(json.dumps(data["suggested_clusters"], indent=1))
    clusters_note = ""
    if (d / "clusters.json").exists():
        clusters_note = " (clusters.json exists, by-hand edits kept; fresh grouping in clusters.suggested.json)"
    else:
        (d / "clusters.json").write_text(json.dumps(data["suggested_clusters"], indent=1))
    r = data["run"]
    print(f"run {r['id']}  {r['name']}  {r['event']}  {r['head_branch']}@{r['head_sha'][:8]}  {r['conclusion']}  "
          f"created {r['created_at']}")
    print(f"  {d / 'failures.json'}")
    t = data["totals"]
    print(f"  legs {t['legs']}  failed {t['legs_failed']}  cancelled {t['legs_cancelled']}  no-log {t['legs_no_log']}  "
          f"failed tests {t['failed_tests']}")
    print()
    for leg in data["legs"]:
        img = leg["runner_image"].get("Image", "")
        print(f"== {leg['short']:<12} {leg['conclusion']:<10} {hhmm(leg['started_at'])}-{hhmm(leg['completed_at'])} "
              f"({leg['minutes']} min)  image={img or '?'}  log={'missing' if leg['log'] == 'missing' else 'ok'}"
              f"  step={', '.join(leg['failed_steps']) or '-'}")
        for a in leg["annotations"]:
            if a["level"] != "warning":
                print(f"   annotation[{a['level']}]: {a['message'][:160]}")
        for b in leg["binaries"]:
            if b.get("aborted"):
                print(f"   ABORTED {b['crate'] or '?'}/{b['bin']} started {hhmm(b['started_at'])}, "
                      f"{len(b['tests'])}/{b.get('declared')} reported, no `test result:` line (process died)")
            if b.get("mismatch"):
                print(f"   WARNING {b['crate'] or '?'}/{b['bin']}: {b['mismatch']} (a test name the parser missed?)")
        if leg["cancelled"]:
            inf = leg["in_flight"]
            print(f"   CANCELLED (timeout-minutes kill shows as cancelled). in flight: "
                  f"{inf['crate']}/{inf['bin']} started {hhmm(inf['started_at'])}, {inf['reported']}/{inf['declared']} "
                  f"reported, slow: {[s['test'] for s in inf['slow']]}" if inf else "   CANCELLED before any binary ran")
            if leg["orphans"]:
                print(f"   orphans killed: {leg['orphans']}")
        for f in leg["failures"]:
            site = f"{f['panic_file']}:{f['panic_line']}" if f["panic_file"] else "(no panic site)"
            tag = "STEP" if f["kind"] == "step" else "FAIL"
            print(f"   {tag} {hhmm(f['ts'])} {f['crate'] or '?'}/{f['bin']}::{f['test']}  @ {site}  log:{f['log_line']}")
            if f["message"] and f["kind"] != "step":
                print(f"        {f['message'][:200]}")
        if leg["browsers"]:
            for b, info in leg["browsers"].items():
                s = info.get("summary", {})
                print(f"   vitest {b}: {s.get('passed', '?')} passed, {s.get('failed', info['failed'])} failed, "
                      f"{info['total_ms'] // 1000}s total, finished {hhmm(info['finished_at'])}")
            for b in ("chromium", "firefox"):
                if b not in leg["browsers"] and leg["short"] == "wasm":
                    print(f"   vitest {b}: NOT RUN (the npm script chains browsers with &&; a failure earlier skips it)")
    print(f"\n== suggested clusters (mechanical: same test, or shared panic site / message head) -> "
          f"{d / 'clusters.json'}{clusters_note}")
    for c in data["suggested_clusters"]:
        print(f" [{c['id']}] {c['failure_count']} failure(s) on {','.join(c['legs'])}  {hhmm(c['first_ts'])}-{hhmm(c['last_ts'])}")
        for tname in c["tests"]:
            print(f"      {tname}")
        for s in c["sites"]:
            print(f"      @ {s}")
        for h in c["message_heads"]:
            print(f"      msg: {h}")
    if not data["suggested_clusters"]:
        print("  (nothing parsed as a failure; see cancelled / missing-log legs above)")


def cmd_timeline(args):
    d = Path(args.run_dir)
    data = json.loads((d / "failures.json").read_text())
    legs = [l for l in data["legs"] if l["binaries"]]
    names = [l["short"] for l in legs]
    rows = defaultdict(dict)
    for l in legs:
        for b in l["binaries"]:
            key = f"{b['crate'] or '?'}/{b['bin']}" + ("" if b["kind"] == "tests" else f" [{b['kind']}]")
            if args.bin and args.bin not in key:
                continue
            if b["result"]:
                cell = f"{hhmm(b.get('started_at'))}-{hhmm(b.get('finished_at'))} {b['result']['secs']:>6.0f}s"
                if b["result"]["failed"]:
                    cell += f" F{b['result']['failed']}"
            elif b.get("aborted"):
                cell = f"{hhmm(b['started_at'])}-  ABORTED"
            elif b.get("started_at"):
                cell = f"{hhmm(b['started_at'])}-  IN FLIGHT"
            else:
                cell = "announced, never ran"
            rows[key][l["short"]] = cell
    w = 24
    print(f"{'binary':<48}" + "".join(f"{n:<{w}}" for n in names))
    order = sorted(rows, key=lambda k: min(v for v in rows[k].values()))
    for key in order:
        print(f"{key[:46]:<48}" + "".join(f"{rows[key].get(n, '-'):<{w}}" for n in names))
    for l in data["legs"]:
        if l["short"] == "wasm" or l["browsers"]:
            for b in ("chromium", "firefox"):
                info = l["browsers"].get(b)
                if not info:
                    print(f"\n{l['short']} vitest {b}: NOT RUN")
                    continue
                slow = sorted(info["tests"].items(), key=lambda kv: -kv[1]["ms"])[:8]
                print(f"\n{l['short']} vitest {b} finished {hhmm(info['finished_at'])}, {info['total_ms'] // 1000}s "
                      f"({info.get('summary', {}).get('failed', info['failed'])} failed); slowest: "
                      + ", ".join(f"{n} {t['ms'] // 1000}s{'!' if t['status'] != 'ok' else ''}" for n, t in slow))


def cmd_recurrence(args):
    d = Path(args.run_dir)
    data = json.loads((d / "failures.json").read_text())
    run = json.loads((d / "run.json").read_text())
    wf = run["path"].rsplit("/", 1)[-1]
    # A nightly compares with the previous nightlies; a CI (push / PR) run with the previous runs of its own
    # event on the same branch — ci.yml has no schedule.
    event = run["event"] if run["event"] in ("schedule", "push", "pull_request", "workflow_dispatch") else "schedule"
    # The listing has twice come back with gaps (08-26, 08-22, 07-17 ... for a nightly that runs every day), not
    # reproducible afterwards: over-fetch, sort here, reject a window with a hole and re-list once, and print
    # exactly which runs were picked so a bad window is visible.
    for attempt in (1, 2):
        p = subprocess.run(["gh", "run", "list", "--repo", args.repo, "--workflow", wf, "--event", event,
                            "--branch", run["head_branch"], "--limit", str(max(60, args.nights * 3)),
                            "--json", "databaseId,createdAt,conclusion,headSha"], capture_output=True, text=True)
        if p.returncode != 0:
            sys.exit(p.stderr)
        rows = sorted(json.loads(p.stdout), key=lambda r: r["createdAt"], reverse=True)
        prev = [r for r in rows if r["createdAt"] < run["created_at"]][: args.nights]
        stamps = [ts_of(run["created_at"])] + [ts_of(r["createdAt"]) for r in prev]
        gaps = [(a - b).days for a, b in zip(stamps, stamps[1:])]
        if event != "schedule" or not gaps or max(gaps) <= 3:
            break
        print(f"WARNING: listing attempt {attempt} has a {max(gaps)}-day hole in a daily schedule; "
              f"{'re-listing' if attempt == 1 else 'using it anyway, check the dates'}", file=sys.stderr)
    print(f"window: {len(prev)} {event} {wf} runs on {run['head_branch']} before {run['created_at'][:10]}: "
          + " ".join(f"{r['createdAt'][5:10]}={r['databaseId']}" for r in prev))
    hist = []
    for r in prev:
        rd = d.parent / str(r["databaseId"])
        if not (rd / "failures.json").exists():
            fetch_run(args.repo, str(r["databaseId"]), d.parent, quiet=True)
            (rd / "failures.json").write_text(json.dumps(parse_run(rd), indent=1))
        h = json.loads((rd / "failures.json").read_text())
        hist.append((r, h))
    tonight = {}
    for leg in data["legs"]:
        for f in leg["failures"]:
            tonight.setdefault(f["test"], set()).add(leg["short"])
    print(f"\n{'tonight’s failed test':<70}{'nights':>7}  dates (legs)")
    for test in sorted(tonight):
        hits = []
        for r, h in hist:
            legs_hit = sorted({leg["short"] for leg in h["legs"] for f in leg["failures"] if f["test"] == test})
            if legs_hit:
                hits.append(f"{r['createdAt'][5:10]}({','.join(legs_hit)})")
        print(f"{test[:69]:<70}{len(hits):>3}/{len(hist):<3}  {' '.join(hits) or '-'}")
    counts = defaultdict(list)
    for r, h in hist:
        for name in {f["test"] for leg in h["legs"] for f in leg["failures"]}:
            counts[name].append(r["createdAt"][5:10])
    others = {k: v for k, v in counts.items() if k not in tonight and len(v) >= 2}
    if others:
        print("\nalso recurring in the window (not tonight):")
        for k, v in sorted(others.items(), key=lambda kv: -len(kv[1])):
            print(f"  {k[:69]:<70}{len(v):>3}  {' '.join(v)}")
    print("\nlegs per night: " + "  ".join(
        f"{r['createdAt'][5:10]}={r['conclusion'][:4]}"
        f"(F{h['totals']['legs_failed']}/C{h['totals']['legs_cancelled']}/N{h['totals']['legs_no_log']})"
        for r, h in hist))
    (d / "recurrence.json").write_text(json.dumps(
        {"window": [r for r, _ in hist], "tonight": {k: sorted(v) for k, v in tonight.items()},
         "counts": counts}, indent=1))


def cmd_render(args):
    """The workflow's return value (saved as JSON) -> the markdown report.
    Keys read: report (rows + lists), counted_failures, dropped_clusters, permission_prompts_or_incidents."""
    d = Path(args.run_dir)
    data = json.loads((d / "failures.json").read_text())
    result = json.loads(Path(args.result).read_text())
    rep = result.get("report") or result
    r = data["run"]
    t = data["totals"]
    legs = data["legs"]
    out = []
    out.append(f"# Triage of {r['name']} run {r['id']} ({r['created_at'][:10]}, {r['head_branch']}@{r['head_sha'][:8]})")
    out.append("")
    out.append(f"{r['html_url']}  ")
    red = [f"{l['short']} ({l['conclusion']}{', no log' if l['log'] == 'missing' else ''})" for l in legs
           if l["conclusion"] not in ("success", "skipped")]
    counted = result.get("counted_failures")
    out.append(f"Legs: {t['legs']}, red: {len(red)} [{', '.join(red)}]. Failure records: {t['failed_tests']}. "
               f"Rows account for {counted if counted is not None else '?'}."
               + (f" **MISMATCH: {t['failed_tests'] - counted:+d} unaccounted.**"
                  if isinstance(counted, int) and counted != t["failed_tests"] else ""))
    if result.get("dropped_clusters"):
        out.append(f"**Clusters with no finding (agent died/skipped): {', '.join(result['dropped_clusters'])}**")
    out.append("")
    out.append("| # | Issue | Kind | Fails | Legs | Recurrence | Confirmed? | Suggested fix | Complexity | Blocks CI / owner |")
    out.append("|---|---|---|---|---|---|---|---|---|---|")
    rows = sorted(rep.get("rows", []), key=lambda x: x.get("priority", 99))
    esc = lambda s: str(s).replace("|", "\\|").replace("\n", " ")
    for row in rows:
        legs_cell = ", ".join(row["legs"]) + (" (all)" if row.get("deterministic") else "")
        out.append(f"| {esc(row['id'])} | {esc(row['issue'])} | {esc(row['kind'])} | {row['failures_caused']} | {esc(legs_cell)} | "
                   f"{esc(row['recurrence'])} | {esc(row['confidence'])}: {esc(row.get('confirmation', ''))} | {esc(row['fix'])} | "
                   f"{esc(row['complexity'])} | {'yes' if row['blocks_ci'] else 'no'} / {esc(row['owner'])} |")
    out.append("")
    for row in rows:
        out.append(f"## {row['id']} — {row['issue']}")
        out.append("")
        out.append(f"Tests: {', '.join(row['tests'])}  ")
        out.append(f"Evidence: {row['evidence']}")
        out.append("")
        out.append(row["details"].strip())
        out.append("")
    for title, key in (("Merges / splits applied", "merges_or_splits_applied"), ("Unexplained", "unexplained"),
                       ("Anomalies worth noting", "anomalies_worth_noting"), ("Backend asks", "backend_asks")):
        items = rep.get(key) or []
        if items:
            out.append(f"## {title}")
            out.append("")
            out.extend(f"- {i}" for i in items)
            out.append("")
    prompts = result.get("permission_prompts_or_incidents") or rep.get("permission_prompts_or_incidents") or []
    out.append("## Permission prompts / incidents reported by agents")
    out.append("")
    out.extend([f"- {p}" for p in prompts] or ["- none"])
    text = "\n".join(out) + "\n"
    # Agents quote absolute scratch paths; the reader wants repo-relative source paths and short log paths.
    text = re.sub(re.escape(str(d.parent)) + r"/tree-[0-9a-f]+/", "", text)
    text = re.sub(r"/private/tmp/\S*?/tree-[0-9a-f]+/", "", text)  # the "…" elided form some agents write
    text = text.replace(str(d.parent) + "/", "")
    (d / "report.md").write_text(text)
    print(text)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", default=DEFAULT_SLUG, help="owner/name on GitHub")
    sub = ap.add_subparsers(dest="cmd", required=True)
    f = sub.add_parser("fetch")
    # (render: see below) — `triage.py render <run-dir> <result.json>` writes <run-dir>/report.md
    f.add_argument("run")
    f.add_argument("--out", required=True, help="root dir; the run lands in <out>/<run-id>/")
    f.add_argument("--force", action="store_true", help="re-download even if cached")
    s = sub.add_parser("failures")
    s.add_argument("run_dir")
    t = sub.add_parser("timeline")
    t.add_argument("run_dir")
    t.add_argument("--bin", help="only binaries whose crate/bin contains this")
    r = sub.add_parser("recurrence")
    r.add_argument("run_dir")
    r.add_argument("--nights", type=int, default=7)
    g = sub.add_parser("render")
    g.add_argument("run_dir")
    g.add_argument("result", help="JSON file holding the workflow's return value")
    args = ap.parse_args()
    if args.cmd == "fetch":
        fetch_run(args.repo, run_id_of(args.run), args.out, force=args.force)
    elif args.cmd == "failures":
        cmd_failures(args)
    elif args.cmd == "timeline":
        cmd_timeline(args)
    elif args.cmd == "recurrence":
        cmd_recurrence(args)
    elif args.cmd == "render":
        cmd_render(args)


if __name__ == "__main__":
    main()
