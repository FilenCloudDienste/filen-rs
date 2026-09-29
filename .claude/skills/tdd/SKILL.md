---
name: tdd
description: >
    Test-driven development in any language: write the test that specifies the behavior,
    then the code. Use when implementing a feature, fixing a bug (failing test first),
    refactoring, or adding coverage. Covers cargo test, Jest/Vitest/bun:test, pytest,
    go test, swift test, and E2E (Playwright, Maestro). Trigger when the user mentions
    tests, coverage, TDD, spec, red/green, a flaky test, or an E2E flow.
---

# Test-Driven Development

Tests are not optional. Every non-trivial piece of logic ships with tests. Red, green, refactor —
pragmatically applied: spikes may start untested, nothing ships untested.

## Step 0 — Detect the toolchain from the manifest

Read the manifest that is actually present; never assume the stack.

| Manifest (Read it) | Test command | Lint / format |
|---|---|---|
| `Cargo.toml` | `cargo test --lib`, `cargo test -p <crate>` | `cargo clippy`, `cargo fmt --check` (`rustfmt.toml`) |
| `package.json` | whatever `scripts.test` says — `vitest` / `jest` / `bun test` | `eslint`, `tsc --noEmit`, `prettier --check` |
| `pyproject.toml` | `pytest` | `ruff check`, `ruff format --check` |
| `go.mod` | `go test ./...` | `go vet`, `gofmt -l` |
| `Package.swift` | `swift test` | `swift-format lint` |

```
Read(file_path: "/abs/repo/Cargo.toml")        # [dev-dependencies], [[test]], workspace members
Read(file_path: "/abs/repo/package.json")      # scripts.test decides jest vs vitest vs bun test
```

Then find where tests already live and copy their conventions:

```bash
git grep -ln "#\[cfg(test)\]" -- '*.rs'
git ls-files 'tests/*' '*.test.ts' '*.test.tsx' '*_test.go' 'test_*.py' '*Tests.swift'
```

**Search rules (they keep you out of `.env` and out of permission prompts):**

- One known file → the `Read` tool, never `cat`/`head`/`sed -n`.
- A definition, its references, a symbol → LSP (`goToDefinition`, `findReferences`, `workspaceSymbol`).
- Text search → `git grep -n "PATTERN" -- '*.rs' '*.ts'` — **always** with an extension pathspec after `--`.
  Tracked files only, so `target/`, `node_modules/` and any ignored `.env` are skipped for free.
- Untracked files only → `grep -rn "PATTERN" --include='*.rs' /abs/repo/<subtree>`. An absolute subtree
  with no `.env` under it; never the repo root, never `.`.
- Never `cd`; every path absolute. Never read or name `.env*` — if a test needs a value from one, ask.
- `Grep`/`Glob` tools, if available, work for the first three; `git grep` is the reliable fallback.

Match the conventions already established. Never introduce a second test framework when one is in
use, and never add an E2E framework to a project that has none without asking first.

---

## The TDD cycle

1. **UNDERSTAND** — what the unit does: inputs, outputs, edge cases
2. **TEST** — write tests that specify the behavior
3. **IMPLEMENT** — the minimum code that makes them pass
4. **VERIFY** — actually run them; never assume green
5. **REFACTOR** — clean up with the tests as the safety net

**Flexible on order, strict on coverage:** new feature → tests first or alongside. Bug fix → failing
test first. Spike → implement freely, tests before it is "done". Refactor → tests must exist *before*
you touch the implementation; never refactor untested code.

---

## Rust — cargo test

Two homes, pick by what you are testing:

```rust
// Unit tests — inline, can reach private items
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_two_positive_numbers() {
        assert_eq!(add(2, 3), 5);
    }

    // Shared setup is a plain function, not a framework
    fn make_config() -> Config { Config { timeout: 30, retries: 3 } }
}
```

```rust
// Integration tests — tests/<name>.rs, public API only
use my_crate::add;

#[test]
fn add_works_from_outside() { assert_eq!(add(10, 20), 30); }
```

Assertions:

```rust
assert_eq!(result, expected);              // prints both sides on failure
assert!(condition, "message if it fails");

#[test]
#[should_panic(expected = "index out of bounds")]
fn panics_on_out_of_bounds() { let _ = vec![1, 2, 3][99]; }

// Don't unwrap in tests — return Result and use `?`
#[test]
fn parse_succeeds() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(parse_input("valid")?.value, 42);
    Ok(())
}

#[test]
fn parse_fails_on_empty() {
    let err = parse_input("").unwrap_err();
    assert!(err.to_string().contains("empty input"));   // assert the message, not just is_err()
}
```

Naming is snake_case and describes scenario + outcome: `returns_zero_for_empty_input`,
`rejects_negative_values` — not `test1`, not `it_works`.

```bash
cargo test --lib                      # unit tests only, no network
cargo test -p <crate> <substring>     # scope by crate, filter by name
cargo test --test <integration_file>  # one integration target
cargo test -- --nocapture             # show println!/dbg! output
cargo test -- --test-threads=1        # serialize tests sharing state
```

Test in particular: every `Result`/`Option` branch (both arms), numeric boundaries and
overflow-adjacent values, empty / whitespace-only / UTF-8-boundary strings, panic conditions via
`#[should_panic]`, trait impls actually behaving, and **any `unsafe` block — thoroughly**.

---

## JavaScript / TypeScript — Jest, Vitest, bun:test

`scripts.test` in `package.json` decides which; their APIs are near-identical (`describe`, `it`,
`expect`, `vi`/`jest`/`mock`). Use the one already there — never add a second.

```typescript
describe("formatBytes", () => {
	it("formats bytes to a human-readable string", () => {
		expect(formatBytes(1024)).toBe("1 KB")
	})

	it("handles zero", () => {
		expect(formatBytes(0)).toBe("0 B")
	})

	it("throws on negative input", () => {
		expect(() => formatBytes(-1)).toThrow("Input must be non-negative")
	})
})
```

Assertions — be specific:

```typescript
// ❌ passes even when the behavior is wrong
expect(result).toBeTruthy()

// ✅ fails precisely when behavior changes
expect(result).toEqual({ id: 1, name: "Jan" })
expect(mockFn).toHaveBeenCalledWith("expected-arg")
expect(mockFn).toHaveBeenCalledTimes(1)
expect(() => parse("")).toThrow("Input cannot be empty")
```

Mocking — at the boundary only, and restore afterwards:

```typescript
vi.mock("../api/users", () => ({
	fetchUser: vi.fn().mockResolvedValue({ id: "1", name: "Jan" })
}))

afterEach(() => vi.restoreAllMocks())   // jest.restoreAllMocks() / mock.restore() for bun:test

// ❌ never mock the unit under test, or your own pure functions — test those directly
```

Async — always await, and test the rejection path:

```typescript
it("throws on not found", async () => {
	await expect(getUser("nonexistent")).rejects.toThrow("User not found")
})
```

Component tests (`@testing-library/*`) assert behavior: right content for given props, right
callback on interaction, show/hide by state, loading and error states. Never internal state or refs.

```bash
npx vitest run path/to/file.test.ts     # or: npx jest path/to/file.test.ts, bun test path/…
npx vitest run --coverage
```

Run the single file you just wrote while iterating; run the full suite before calling it done.

---

## E2E — Playwright (web), Maestro (mobile)

E2E is for critical journeys that cross pages, real auth, real network or native OS dialogs — not
for logic a unit test can cover, not for every state permutation.

Locator preference is the same everywhere: **role/accessibility id > label > test id > visible
text**. Never CSS selectors, never XPath — they break on every refactor.

```typescript
await page.getByRole("button", { name: "Upload" }).click()
await page.getByLabel("Choose file").setInputFiles("test-assets/document.pdf")
await expect(page.getByText("Upload complete")).toBeVisible()   // assert, never waitForTimeout
```

```yaml
appId: com.example.app
---
- launchApp
- tapOn:
      id: "upload-button"          # matches the accessibility id set in code
- assertVisible:
      text: "document.pdf"
      timeout: 10000               # a timeout on the assertion, not a fixed sleep
- assertNotVisible: "Uploading..."
```

Wait on conditions, never on the clock. Reuse a logged-in session (Playwright `storageState`) rather
than logging in at the top of every test; take credentials from the environment the runner already
provides — do not open `.env` files yourself.

```bash
npx playwright test e2e/upload.spec.ts   # --headed / --ui to debug
maestro test .maestro/flows/upload_file.yaml
```

---

## Bug fix protocol — reproduce first

1. Write a test that reproduces the bug — it **must fail** before the fix
2. Run it, see the red (a test that never failed proves nothing)
3. Fix the bug
4. Run it, see the green
5. Run the full suite — confirm nothing else broke

---

## Coverage

Coverage is a signal, not a goal; 100% with weak assertions is worthless. Aim for: every happy path,
every error path, every boundary. The number follows — don't chase it. If a report shows an untested
branch, either test it or delete it as dead code.

---

## Test data — real over mocked

If the real API/server/DB is reachable in this environment (dev server up, test data seeded,
credentials already in the runner's environment, fast enough), write a genuine integration test
against it. Real data finds bugs mocks never would.

If it is not reachable, **ask before inventing data**: mock the response, add a fixture/seed, or mark
the test integration-only? Never silently fabricate realistic-looking data.

Mocking *is* the right call for: third-party APIs (payments, email, cloud storage — never call the
real one), services unavailable in CI by design, irreversible side effects (sends mail, charges a
card, deletes data), error conditions that are hard to trigger for real (500, timeout, rate limit),
and raw speed when a suite would otherwise make hundreds of network calls.

When you do mock, mirror the real response shape exactly — check the actual shape first from the
API docs, an existing call site, or a recorded fixture:

```bash
git grep -n "mockResolvedValue\|mock_response\|serde_json::json!" -- '*.ts' '*.rs' '*.py'
git ls-files '*fixtures*' '*mocks*'
```

---

## What NOT to do

- **Don't write tests after the fact just to hit a number** — tests written for a coverage metric are weak. Write them to specify behavior.
- **Don't test implementation details** — if a refactor with unchanged behavior breaks the test, the test was wrong.
- **Don't share mutable state between tests** — every test must pass alone, and in any order.
- **Don't loosen assertions to get green** (`any`, `toBeTruthy`, bare `is_ok()`) — a test that always passes is worse than no test.
- **Don't commit `.only` / `#[ignore]` / `t.Skip`** — they silently disable the rest of the suite.
- **Don't tolerate a flaky test** — flaky means broken. Diagnose the root cause and fix it, or delete the test.
- **Don't mock what you own** — mock HTTP, DB, clock, filesystem; not your own modules.
- **Don't skip the run.** Execute the tests and read the output. Never report a result you did not see.

In filen-rs, also follow the Tests section of the `clean-rust-filen-rs` skill: fixture policy, paused time instead of sleeps, no silent skips, live-test hygiene.
