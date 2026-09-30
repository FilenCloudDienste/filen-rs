---
name: house-review
description: Review your own finished branch against this repository's house rules before asking the maintainer for review - mechanical checks over the whole range (stray files, binaries, dependencies, unsafe/zeroize/vendor/[patch], review markers, commit-message hygiene, cited hashes that do not resolve, unfolded fixups), a sweep for helpers that duplicate what main already has, a walk of every clean-rust-filen-rs house rule over the diff, and a fold plan that maps each self-fix to the commit it corrects so fold-fixups can clean the history. Use when a branch is feature-complete, after a review round, and before handing work over.
---

# House review: check a branch before the maintainer does

Every rule this skill checks is one the maintainer has had to enforce by hand on a real
branch, usually more than once. Run it on your own branch when it is feature-complete, fix
what it finds, and only then ask for review.

It reviews; it does not rewrite. Fixes go in as ordinary commits (or `fixup!`/`amend!`
commits of the commit they correct), and the history is cleaned afterwards with the
`fold-fixups` skill. Never push the result: the maintainer pushes.

## 0. Scope

```bash
git fetch origin main                     # review against the current main, not a stale one
BASE=$(git merge-base origin/main HEAD)
git log --format='%h %s' $BASE..HEAD | cat
git diff --stat $BASE HEAD | cat
```

Everything below uses `$BASE..HEAD` (commits) or `$BASE HEAD` (the net diff).

## 1. Mechanical checks

Apart from the first two, each command should print nothing. Anything it prints is a
finding, or needs a one-line justification in the PR description.

```bash
# Hooks installed for this clone or worktree (prints the hooks path)
git config core.hooksPath                 # expect: scripts/git-hooks

# Stray files: planning notes, screenshots, scratch output. Read the list.
git diff --name-only --diff-filter=A $BASE HEAD | cat

# Diff policy: [patch]/vendor, git dependencies unpinned or outside the maintainer's
# accounts, zeroize, unsafe, and binary files that no README.md or generate.sh beside
# them names, each of which needs the maintainer's sign-off, and any allow of
# clippy::cast_possible_truncation, which has no exceptions. It also lists new Cargo.lock
# packages and majors, each named with its reason in the PR description, and every added
# #[allow]/#[expect] line: read each one; one without a comment naming its tradeoff is a
# finding. The same script runs in pre-commit and in CI.
bash scripts/check-diff-policy.sh $BASE..HEAD

# Dependencies: every added line is a new dependency or feature and needs sign-off.
git diff $BASE HEAD -- '*Cargo.toml' | grep -E '^\+[^+]'

# Review and agent markers in added lines.
git diff -U0 $BASE HEAD | grep -nE '^\+.*(ponytail:|TODO\(agent|F[0-9]{3}[^0-9]|[Aa]udit (note|finding))'

# Commit messages, through the same commit-msg hook CI runs: no attribution, session
# links, finding IDs, or audit or review framing, and every cited hash exists here
# (<repo>@<sha> for another repo).
msg=$(mktemp)
for c in $(git rev-list --no-merges $BASE..HEAD); do
	git log -1 --format=%B $c > "$msg"
	bash scripts/git-hooks/commit-msg "$msg" || git log -1 --format='  in %h %s' $c
done

# Unfolded fix-up commits.
git log --format='%h %s' $BASE..HEAD | grep -E '^[0-9a-f]+ (fixup|squash|amend)!'
```

Then the build and test gates, and say in the PR which ones you ran:

```bash
bash scripts/git-hooks/pre-commit         # fmt, taplo, native + uniffi + both wasm clippy passes
bash scripts/git-hooks/pre-push </dev/null # heif-decoder passes, clippy --tests, cargo test --lib
cargo test -p filen-sdk-rs --lib -F cache
```

Live suites need a test account. If you could not run the ones your change touches, name
them in the PR instead of implying they pass.

Optional, when every commit should build on its own (a reviewer stepping through the
stack): a rebase onto the unchanged base rewrites nothing and runs the command after each
commit.

```bash
git rebase --exec 'cargo clippy --workspace --exclude heif-decoder --all-targets -- -D warnings' $BASE
```

## 2. Duplicates of what main already has

List every item the branch defines and look for it on main:

```bash
git diff -U0 $BASE HEAD -- '*.rs' \
	| grep -oE '^\+[[:space:]]*(pub(\([a-z]+\))? )?(async )?(unsafe )?(fn|struct|enum|trait|type|const|static|mod) [A-Za-z_][A-Za-z0-9_]*' \
	| awk '{print $NF}' | sort -u \
	| while read -r name; do
		hits=$(git grep -n -w "$name" $BASE -- '*.rs' | head -3)
		[ -n "$hits" ] && printf '== %s\n%s\n' "$name" "$hits"
	done
```

A name that already exists on main is either the same thing (reuse it) or a clash (rename).
Exact names miss most duplicates, so also search by concept: for each new helper, read the
"Reuse before writing" table in `clean-rust-filen-rs` and grep for what it does
(`lock_drive`, `catch_unwind`, `spawn_ordered_dispatch`, `CHUNK_SIZE_U64`,
`MAX_OPEN_FILES`, `MaybeSend`, `ValidatedName`, `Semaphore::MAX_PERMITS`,
`floor_char_boundary`, the sibling job's module). A helper that is a near-copy of a sibling
job's is lifted into a shared module and both callers migrate, in this series.

## 3. Walk the house rules over the diff

Read `clean-rust-filen-rs` (house rules and the review checklist) and the `clean-rust`
review checklist, then go through the diff one lens at a time:

1. Reuse and duplication (section 2 above).
2. Module layout, size and visibility.
3. Public API and job shape against `fs/copy`.
4. Validation, arithmetic and untrusted input.
5. Errors.
6. FFI and wasm, including `filen-sdk-rs/web/main.test.ts`.
7. Compatibility: anything shipped renamed or reworded, sibling semantics.
8. Drive lock and concurrency.
9. Security and dependencies.
10. Tests: can each one fail, no real sleeps, no silent skips.
11. Docs and comments.

For a large branch, give each lens to its own reviewer and run them in parallel.

Verify every finding before you report it. Read the line and confirm that the rule applies.
Then check whether main already does the same thing. A rule main itself breaks is known
debt; it is a finding only if the branch adds a new instance of it.

Report each finding as:

```
<file>:<line> - <rule, with its section> - introduced in <sha> <subject> - fix: <one line>
```

To find the commit that introduced a line, run `git blame -L <line>,<line> HEAD -- <file>`. A
commit inside `$BASE..HEAD` is where the fix belongs. A commit at or below `$BASE` means the
problem is already on main. Report that separately, as a standalone fix.

## 4. History

```bash
python3 .claude/skills/fold-fixups/scripts/fold-plan.py $BASE          # per commit
python3 .claude/skills/fold-fixups/scripts/fold-plan.py $BASE --hunks  # per hunk
```

For every commit, this blames the lines it touches and names the earlier commits they came
from. From that, write the plan:

- **Fix of a branch commit** (`fix`/`refactor`/`test` touching lines from inside the range):
  fold it into the commit it corrects.
- **Fix of something already on main**: a standalone commit, moved to the front of the
  branch.
- **Behaviour change of shipped code**: also a standalone commit at the front, with the
  reason in its body.
- **Commit bundling unrelated changes**: split it.

Output the plan as `commit -> target (fold) | front (standalone) | keep`, then carry it out
with the `fold-fixups` skill. Re-run section 1 afterwards. Folding rewrites messages, and
the new messages must pass the same checks.

## Output

1. The mechanical-check results: each command, and whatever it printed.
2. The verified findings, grouped by the commit they belong to.
3. The fold plan.
4. The gates you ran and the ones you did not, with the reason for each skipped one.
