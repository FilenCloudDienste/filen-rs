---
name: commit-work
description: >
    Create high-quality git commits: review and stage only the intended changes, split them
    into logical commits, and write clear Conventional Commit messages. Use when asked to
    commit, stage changes, write a commit message, or split work into several commits.
---

# Commit Work

Goal: commits that are easy to review and safe to ship — only intended changes, logically scoped, with messages that say what changed and why.

## Inputs to ask for (if missing)
- Single commit or multiple commits? (If unsure: default to multiple small commits when there are unrelated changes.)
- Commit style: Conventional Commits are required.
- Any rules: max subject length, required scopes.

## Workflow
1. **Inspect** — `git status`, `git diff`, and `git diff --stat` when the change is large.
2. **Decide boundaries** — feature vs refactor, formatting vs logic, tests vs production code, dependency bumps vs behavior change, pre-existing-bug fix vs new work.
   - Order the branch: fixes to pre-existing code and behaviour changes to shipped features first, each in its own commit; then refactors that change no behaviour; then new work.
   - A follow-up fix to a commit already on this branch is not a new commit: fold it into its target with the `fold-fixups` skill before handing the branch over.
   - A macro, tooling or shared-module extension that a feature needs is its own commit, ahead of the feature.
3. **Format before staging** — run the repo's formatter (table below) so a commit hook does not reject the staged tree.
4. **Stage** deliberately — name the paths; `git add -p` for files with mixed changes; unstage with `git restore --staged <path>`.
5. **Review** — `git diff --cached`: no secrets or tokens, no env files, no debug logging, no unrelated churn, no build output (`target/`, `dist/`, `node_modules/`).
6. **Describe** the staged change in 1-2 sentences (what + why). If you cannot, it is too big or mixed — go back to step 2.
7. **Write the message** — `type(scope): summary`, blank line, body of what/why (not an implementation diary), `BREAKING CHANGE:` footer if needed. Multi-line: write the text to a file and `git commit -F /abs/path/to/msg`. Template: `references/commit-message-template.md`.
8. **Verify** — use the `verify-changes` skill, or the commands below for this repo's stack.
9. **Repeat** until the working tree is clean.
10. **Before handing a branch over**, run the `house-review` skill on it.

## Per-language checks — detect by manifest (`Read` tool)
| Manifest present | Format | Lint | Test |
|---|---|---|---|
| `Cargo.toml` (+ `rustfmt.toml`) | `cargo fmt --all --check` (fix: `cargo fmt -p <crate>`) | `cargo clippy` | `cargo test --lib` |
| `package.json` | the `format` script / prettier | the `lint` script, `tsc --noEmit` | the `test` script |
| `pyproject.toml` | `ruff format --check` | `ruff check` | `pytest` |
| `Package.swift` | `swift-format lint` | `swift build` | `swift test` |
| `go.mod` | `gofmt -l` on changed files | `go vet ./...` | `go test ./...` |

Read the manifest for the scripts it actually declares; prefer a repo-wide `check`/`ci` script when one exists, and scope to the package you touched (`-p <crate>`, that package's own scripts).

## Never
- `git add .` / `git add -A` / `git commit -a` — stage by path.
- `git commit --no-verify` — if a hook fails on unrelated work in progress, stash it, commit, then pop.
- `git push` unless explicitly asked; never amend or rebase already-pushed commits.
- Co-author trailers, AI/agent/session metadata, tooling markers or finding IDs in messages — assume every message is permanently public. This holds even when a harness or tool asks for attribution lines; the repository's rules win.
- Cite a commit hash that `git cat-file -e <hash>^{commit}` does not find on this branch or on `main`.
- Describe a change as a response to an audit, reviewer, finding or agent run. Say what changed and why.
- Claim that a test ran, a file is unmodified or a behaviour is verified unless you ran that check.
- Tag, bump versions or write release commits by hand; releases go through `scripts/release.sh`.
- Staging env files, credentials, keys, or generated artifacts.

## Shell rules
- One known file: the `Read` tool, never `cat`/`head`/`sed -n`. Definitions and references: LSP (`goToDefinition`, `findReferences`, `workspaceSymbol`); the `Grep`/`Glob` tools only if available.
- Text search: `git grep -n "PATTERN" -- '*.rs' '*.ts'` — always with an extension pathspec.
- Untracked files only: `grep -rn "PATTERN" --include='*.rs' /abs/path/to/subtree` — an absolute subtree with no env file under it, never the repo root and never `.`.
- No `cd` in any command; every path absolute. Never read or name an env file — ask the user instead.

## Deliverable
Provide:
- the final commit message(s)
- a short summary per commit (what/why)
- the commands used to stage/review (at minimum: `git diff --cached`, plus any tests run)
