---
name: verify-changes
description: CRITICAL: Always use this skill, no matter what task you are working on!
---

# Verify Changes Skill

After **every** code modification — including small edits, refactors, and single-line fixes — run all available verification checks. A change is not done until checks pass.

---

## Step 1: Detect Available Checks

Run once at the start of any coding session (or when working in a new project). Check which tools are configured:

```bash
# Check package.json scripts
cat package.json 2>/dev/null | grep -A 40 '"scripts"'
```

Then map what's available:

| Check          | Look for                                                         |
| -------------- | ---------------------------------------------------------------- |
| **ESLint**     | `eslint` in scripts, or `eslint.config.*` / `.eslintrc*` present |
| **TypeScript** | `tsc` in scripts, or `tsconfig.json` present                     |
| **Tests**      | `test`, `jest`, `vitest`, `bun test` in scripts, or config files |
| **Bun**        | `bun.lockb` or scripts using `bun`                               |

If none of the three exist, skip verification entirely and note this.

---

## Step 2: Run Checks After Every Change

After each code modification, run all checks that exist. Use this exact priority order:

### 1. ESLint

```bash
# npm / yarn / pnpm
npx eslint <changed-file> --max-warnings=0

# bun
bunx eslint <changed-file> --max-warnings=0

# If a lint script exists, prefer it
npm run lint 2>/dev/null || yarn lint 2>/dev/null || bun run lint 2>/dev/null
```

Prefer linting only the changed file(s) first for speed. If errors reference other files, run the full lint.

**Skip if:** no `eslint` in scripts and no ESLint config file found.

### 2. TypeScript

```bash
# Preferred: noEmit typecheck only
npx tsc --noEmit

# Or via script
npm run typecheck 2>/dev/null \
  || npm run type-check 2>/dev/null \
  || yarn typecheck 2>/dev/null \
  || bun run typecheck 2>/dev/null \
  || npx tsc --noEmit
```

**Skip if:** no `tsconfig.json` found and no typecheck script present.

### 3. Tests

```bash
# Run only tests related to changed files first (fast)
npx jest --testPathPattern=<changed-file-basename> --passWithNoTests 2>/dev/null \
  || npx vitest run <changed-file-basename> 2>/dev/null \
  || bun test <changed-file-basename> 2>/dev/null

# If no related tests found, run full suite
npm test 2>/dev/null \
  || yarn test 2>/dev/null \
  || bun test 2>/dev/null
```

**Skip if:** no test config and no `test` / `spec` files found anywhere in the project.

---

## Step 3: Handle Failures

### ESLint failure

- Read the error output carefully
- Fix each reported issue in the affected file(s)
- Re-run ESLint on the fixed file before moving on
- Do **not** disable rules with `// eslint-disable` unless the rule is provably wrong for this case — if you do, add a comment explaining why

### TypeScript failure

- Read every type error — do not suppress with `@ts-ignore` or `@ts-expect-error` unless genuinely necessary
- If a type error is in a file you didn't touch, check whether your change broke a contract (changed a function signature, narrowed/widened a type, etc.)
- Fix the root cause, not the symptom

### Test failure

- Determine if the test failure is caused by your change or was pre-existing:
    ```bash
    # Check if tests were failing before your change by inspecting what you modified
    # If unsure, check git status
    git stash && npm test 2>/dev/null; git stash pop
    ```
- If your change caused the failure: fix the implementation or update the test if the behavior change was intentional
- Never delete or skip tests to make the suite pass
- If a test was already failing before your change, note it explicitly and do not mask it

---

## Step 4: Report Status

After all checks complete, always report the result before considering the task done:

**All passing:**

```
✅ ESLint: clean
✅ TypeScript: no errors
✅ Tests: 42 passed
```

**With skips:**

```
✅ ESLint: clean
⏭️ TypeScript: skipped (no tsconfig.json)
✅ Tests: 7 passed
```

**With failures (before fixing):**

```
❌ ESLint: 2 errors in src/utils/format.ts
⏭️ TypeScript: skipped
✅ Tests: all passed
```

Do not mark a task complete while any check shows ❌.

---

## Quick Reference: Common Script Names

When looking for scripts in `package.json`, these are the most common names to check:

| Check      | Common script names                               |
| ---------- | ------------------------------------------------- |
| Lint       | `lint`, `lint:check`, `eslint`                    |
| Typecheck  | `typecheck`, `type-check`, `tsc`, `ts`, `check`   |
| Test       | `test`, `test:unit`, `test:run`, `jest`, `vitest` |
| All-in-one | `check`, `verify`, `ci`, `validate`               |

If a combined `check` or `ci` script exists that runs all three, prefer it over running them individually.

---

## Reminders

- **Always run checks after every change** — not just at the end of a multi-step task
- **Scope checks to changed files first** for speed, then widen if needed
- **Never suppress errors** to make checks pass — fix the underlying issue
- **A task is not done** until all available checks are green
