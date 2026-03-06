---
name: tdd
description: CRITICAL: Always use this skill, no matter what task you are working on!
---

# Test-Driven Development

Tests are not optional. Every non-trivial piece of logic ships with tests. The cycle is
red → green → refactor, but pragmatically applied — spikes and exploration are fine without tests,
but nothing moves to production without coverage that actually verifies the behavior.

---

## Step 0 — Inspect the project first

Before writing a single test or line of implementation, understand the testing setup:

```bash
# JavaScript / TypeScript — what unit/integration framework is in use?
cat package.json | grep -E '"jest"|"vitest"|"bun"|"@testing-library"|"@jest"|"msw"|"supertest"'
cat jest.config.* 2>/dev/null || cat vitest.config.* 2>/dev/null

# Is this a Bun project?
cat package.json | grep -E '"scripts"' -A 10 | grep "bun test"
ls bunfig.toml 2>/dev/null

# E2E — web or mobile?
cat package.json | grep -E '"playwright"|"@playwright"'
ls .maestro 2>/dev/null || find . -name "*.yaml" -path "*maestro*" | head -5

# Where do unit tests live?
find . -name "*.test.ts" -o -name "*.test.tsx" -o -name "*.spec.ts" \
  | grep -v node_modules | head -10

# Rust — test structure
grep -r "#\[cfg(test)\]" src/ | head -10
cat Cargo.toml | grep -E '\[dev-dependencies\]' -A 20

# Look at existing tests to understand conventions
# — naming patterns, assertion style, mock approach, file co-location vs __tests__/
```

**Testing layers — understand which exist in the project:**

```
Unit / Integration   →  Jest, Vitest, bun:test, cargo test
E2E Web             →  Playwright
E2E Mobile          →  Maestro
```

Match the conventions already established. Never introduce a second testing library when one is
already in use. Never add an E2E framework to a project that doesn't have one without confirming
with the developer first.

---

## The TDD Cycle — Pragmatic

```
1. UNDERSTAND   — clarify exactly what the unit should do, its inputs, outputs, edge cases
2. TEST         — write tests that specify the behavior (can be before or alongside impl)
3. IMPLEMENT    — write the minimum code to make tests pass
4. VERIFY       — run the tests, confirm green
5. REFACTOR     — clean up with tests as a safety net, keep green throughout
```

**Flexible on order, strict on coverage:**

- New feature from scratch → write tests first (or alongside), then implement
- Bug fix → write a failing test that reproduces the bug first, then fix
- Exploratory / spike → implement freely, but write tests before the code is considered done
- Refactor → tests must exist before touching implementation; never refactor untested code

**Never skip the run.** Always actually execute the tests — don't assume they pass.

---

## Jest / Vitest

### Detect which one is installed

```bash
cat package.json | grep -E '"jest"|"vitest"'
# Use whichever is present. If both exist, check which the project scripts use:
cat package.json | grep -A 5 '"scripts"' | grep -E "test"
```

Vitest and Jest have nearly identical APIs — `describe`, `it`/`test`, `expect`, `vi`/`jest` for
mocking. The differences are in config and import style. Match what the project uses.

### Test file location — match the project

```bash
# Co-located (common in RN/Expo and Vite projects)
src/utils/format.ts
src/utils/format.test.ts

# Separate __tests__ directory (common in Jest projects)
src/utils/format.ts
src/__tests__/utils/format.test.ts

# Check which pattern the project uses before creating new files
find src -name "*.test.*" | head -5
```

### Structure — match existing conventions

```typescript
// If the project uses describe/it blocks:
describe("formatBytes", () => {
	it("formats bytes to human-readable string", () => {
		expect(formatBytes(1024)).toBe("1 KB")
	})

	it("handles zero", () => {
		expect(formatBytes(0)).toBe("0 B")
	})

	it("handles negative input", () => {
		expect(() => formatBytes(-1)).toThrow("Input must be non-negative")
	})
})

// If the project uses flat test() calls:
test("formatBytes: formats bytes to human-readable string", () => {
	expect(formatBytes(1024)).toBe("1 KB")
})
```

### What to test — and what not to

**Test:**

- Return values for all meaningful input variations
- Edge cases: empty, zero, null/undefined, boundary values, max values
- Error conditions: does it throw the right error with the right message?
- Side effects: was the right function called with the right args?
- Async behavior: resolution, rejection, loading states

**Don't test:**

- Implementation details — test behavior, not how it's achieved internally
- Third-party library internals — assume they work
- Trivial getters/setters with no logic
- Type definitions alone (TypeScript catches those at compile time)
- Private methods directly — test them through the public interface

### Assertions — be specific

```typescript
// ❌ Too vague — passes even if something is wrong
expect(result).toBeTruthy()
expect(result).toBeDefined()

// ✅ Specific — fails precisely when behavior changes
expect(result).toBe("expected string")
expect(result).toEqual({ id: 1, name: "Jan" })
expect(result).toHaveLength(3)
expect(mockFn).toHaveBeenCalledWith("expected-arg")
expect(mockFn).toHaveBeenCalledTimes(1)

// ✅ For errors — check message too, not just that it throws
expect(() => parse("")).toThrow("Input cannot be empty")
```

### Mocking — mock at the boundary, not deep inside

```typescript
// ✅ Mock external dependencies (API calls, DB, file system, timers)
// Vitest
vi.mock("../api/users", () => ({
	fetchUser: vi.fn().mockResolvedValue({ id: "1", name: "Jan" })
}))

// Jest
jest.mock("../api/users", () => ({
	fetchUser: jest.fn().mockResolvedValue({ id: "1", name: "Jan" })
}))

// ✅ Restore mocks after each test — avoid test pollution
afterEach(() => {
	vi.restoreAllMocks() // Vitest
	jest.restoreAllMocks() // Jest
})

// ❌ Don't mock the unit under test itself
// ❌ Don't mock internal pure functions — test them directly
```

### Async tests

```typescript
// ✅ Always await — never let promises float
it("fetches user data", async () => {
	const user = await getUser("123")
	expect(user.name).toBe("Jan")
})

// ✅ Test rejection explicitly
it("throws on not found", async () => {
	await expect(getUser("nonexistent")).rejects.toThrow("User not found")
})
```

### React component tests — check what's installed first

```bash
grep -E '"@testing-library/react"|"@testing-library/react-native"' package.json
```

```typescript
// React (web) with @testing-library/react
import { render, screen, fireEvent } from '@testing-library/react'

it('calls onPress when button is tapped', () => {
  const onPress = vi.fn()
  render(<SubmitButton onPress={onPress} label="Submit" />)
  fireEvent.click(screen.getByText('Submit'))
  expect(onPress).toHaveBeenCalledTimes(1)
})

// React Native with @testing-library/react-native
import { render, fireEvent } from '@testing-library/react-native'

it('calls onPress when button is tapped', () => {
  const onPress = jest.fn()
  const { getByText } = render(<SubmitButton onPress={onPress} label="Submit" />)
  fireEvent.press(getByText('Submit'))
  expect(onPress).toHaveBeenCalledTimes(1)
})
```

**Component tests should test behavior, not implementation:**

- Does it render the right content given these props?
- Does it call the right callback when interacted with?
- Does it show/hide things based on state?
- Does it handle loading/error states correctly?

Never test internal state, refs, or implementation details of a component.

### Running tests

```bash
# Vitest
npx vitest run                        # run once
npx vitest run src/utils/format.test.ts  # single file
npx vitest --coverage                 # with coverage

# Jest
npx jest                              # run all
npx jest src/utils/format.test.ts    # single file
npx jest --coverage                   # with coverage
npx jest --watch                      # watch mode during dev
```

Always run the specific test file after writing tests — don't run the full suite every time during
development. Run the full suite before considering the work done.

---

## bun:test — Bun Server Projects

Bun ships its own test runner (`bun:test`) that is Jest-compatible but significantly faster.
Use it for Bun-native server projects. Do not add Jest or Vitest to a Bun project that already
uses `bun:test`.

### Detect bun:test

```bash
# Check if bun test is the test runner
cat package.json | grep -E '"test"' | grep "bun test"
# Or check for bun-specific test imports in existing files
grep -r "from 'bun:test'" src/ | head -5
```

### API — Jest-compatible with Bun imports

```typescript
import { describe, it, expect, mock, beforeEach, afterEach } from "bun:test"

describe("UserService", () => {
	it("creates a user with hashed password", async () => {
		const user = await UserService.create({
			email: "mail@example.com",
			password: "secret"
		})
		expect(user.email).toBe("mail@example.com")
		expect(user.passwordHash).not.toBe("secret")
		expect(user.passwordHash).toHaveLength(60) // bcrypt hash length
	})
})
```

### Mocking with bun:test

```typescript
import { mock, spyOn } from "bun:test"

// Mock a module
const fetchMock = mock(() => Promise.resolve(new Response('{"ok":true}')))
globalThis.fetch = fetchMock

// Spy on a method
const spy = spyOn(db, "query").mockResolvedValue([{ id: 1 }])

afterEach(() => {
	mock.restore() // restore all mocks
})
```

### Testing Bun.serve HTTP handlers directly

```typescript
import { describe, it, expect } from "bun:test"

// Test handlers without spinning up a real server
describe("POST /upload", () => {
	it("rejects files over size limit", async () => {
		const req = new Request("http://localhost/upload", {
			method: "POST",
			body: new Uint8Array(11 * 1024 * 1024) // 11MB — over limit
		})
		const res = await handleUpload(req)
		expect(res.status).toBe(413)
		const body = await res.json()
		expect(body.error).toContain("File too large")
	})

	it("accepts valid files", async () => {
		const form = new FormData()
		form.append("file", new Blob(["hello"]), "test.txt")
		const req = new Request("http://localhost/upload", {
			method: "POST",
			body: form
		})
		const res = await handleUpload(req)
		expect(res.status).toBe(200)
	})
})
```

### Testing Bun native APIs

```typescript
import { describe, it, expect, afterEach } from "bun:test"
import { rm } from "node:fs/promises"

describe("FileStore", () => {
	const testPath = "/tmp/bun-test-file"

	afterEach(async () => {
		await rm(testPath, { force: true })
	})

	it("writes and reads back correctly", async () => {
		const data = new Uint8Array([1, 2, 3, 4])
		await Bun.write(testPath, data)
		const read = new Uint8Array(await Bun.file(testPath).arrayBuffer())
		expect(read).toEqual(data)
	})
})
```

### Running bun:test

```bash
bun test                          # run all tests
bun test src/utils/format.test.ts # single file
bun test --watch                  # watch mode
bun test --coverage               # coverage report
bun test --timeout 10000          # custom timeout (ms)
```

---

## Rust — cargo test

### Test organization — match the project

```bash
# Check if tests are inline (unit) or in separate files (integration)
grep -r "#\[cfg(test)\]" src/ | head -10
ls tests/ 2>/dev/null  # integration tests directory
```

Rust has two natural homes for tests:

**Unit tests — inline in the module (preferred for testing private functions):**

```rust
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;  // imports everything from the parent module, including private items

    #[test]
    fn adds_two_positive_numbers() {
        assert_eq!(add(2, 3), 5);
    }

    #[test]
    fn handles_negative_numbers() {
        assert_eq!(add(-1, 1), 0);
    }
}
```

**Integration tests — in `tests/` directory (public API only):**

```rust
// tests/add_integration.rs
use my_crate::add;

#[test]
fn add_works_from_outside() {
    assert_eq!(add(10, 20), 30);
}
```

### Assertions

```rust
// Equality
assert_eq!(result, expected);         // fails with both values shown
assert_ne!(result, unexpected);

// Boolean
assert!(condition, "message if fails");

// Panics — test that code panics as expected
#[test]
#[should_panic(expected = "index out of bounds")]
fn panics_on_out_of_bounds() {
    let v = vec![1, 2, 3];
    let _ = v[99];
}

// Results — don't unwrap in tests, use ? or assert explicitly
#[test]
fn parse_succeeds() -> Result<(), Box<dyn std::error::Error>> {
    let parsed = parse_input("valid")?;
    assert_eq!(parsed.value, 42);
    Ok(())
}

#[test]
fn parse_fails_on_empty() {
    let result = parse_input("");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("empty input"));
}
```

### Test naming — descriptive, snake_case

```rust
// ✅ Name describes the scenario and expected outcome
#[test] fn returns_zero_for_empty_input() { ... }
#[test] fn rejects_negative_values() { ... }
#[test] fn handles_utf8_boundary_correctly() { ... }

// ❌ Vague
#[test] fn test1() { ... }
#[test] fn it_works() { ... }
```

### Test setup — use helper functions, not a test framework

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Shared setup via plain functions — no special framework needed
    fn make_config() -> Config {
        Config {
            timeout: 30,
            retries: 3,
            base_url: "http://localhost".to_string(),
        }
    }

    #[test]
    fn connects_with_valid_config() {
        let config = make_config();
        assert!(connect(config).is_ok());
    }
}
```

### Running tests

```bash
cargo test                            # run all tests
cargo test test_name                  # run tests matching name (substring match)
cargo test --lib                      # unit tests only
cargo test --test integration_test    # specific integration test file
cargo test -- --nocapture             # show println! output during tests
cargo test -- --test-threads=1        # run sequentially (for tests with shared state)
```

### What to test in Rust

- All `Result`/`Option` return paths — both `Ok`/`Some` and `Err`/`None`
- Boundary values for numeric types (0, max, overflow-adjacent values)
- String handling: empty, UTF-8 boundaries, whitespace-only
- Panic conditions with `#[should_panic]`
- Trait implementations — does the type actually behave correctly?
- Any `unsafe` code — it must have thorough tests

---

## Playwright — Web E2E

Playwright tests real user flows in a real browser. They are slower than unit tests and should
cover critical paths — not every permutation. Use them for flows that span multiple pages,
require real network, or verify visual/interaction behavior that unit tests cannot catch.

### Detect Playwright

```bash
cat package.json | grep "@playwright/test"
ls playwright.config.* 2>/dev/null
ls e2e/ tests/e2e/ 2>/dev/null
```

### File structure — match the project

```bash
# Common locations
e2e/
tests/e2e/
playwright/

# Check what's already there
find . -name "*.spec.ts" | grep -v node_modules | head -10
```

### Writing Playwright tests

```typescript
import { test, expect } from "@playwright/test"

// Test a critical user flow end-to-end
test("user can upload and share a file", async ({ page }) => {
	await page.goto("/drive")

	// Use user-visible locators — never CSS selectors or XPath for stable tests
	await page.getByRole("button", { name: "Upload" }).click()
	await page.getByLabel("Choose file").setInputFiles("test-assets/document.pdf")

	// Wait for upload to complete — use expect, not sleep
	await expect(page.getByText("document.pdf")).toBeVisible()
	await expect(page.getByText("Upload complete")).toBeVisible()

	// Share the file
	await page.getByRole("button", { name: "Share" }).click()
	const shareLink = page.getByTestId("share-link")
	await expect(shareLink).toBeVisible()
	expect(await shareLink.inputValue()).toMatch(/^https:\/\//)
})
```

### Locator strategy — preferred order

```typescript
// ✅ Best — role-based (accessible, resilient)
page.getByRole("button", { name: "Submit" })
page.getByRole("textbox", { name: "Email" })

// ✅ Good — label-based
page.getByLabel("Password")

// ✅ Good — test ID (add data-testid to elements specifically for testing)
page.getByTestId("file-upload-zone")

// ✅ Acceptable — visible text
page.getByText("Upload complete")

// ❌ Avoid — CSS selectors break on refactor
page.locator(".btn-primary.upload")

// ❌ Never — XPath
page.locator('//div[@class="container"]/button')
```

### Waiting — use assertions, never arbitrary sleeps

```typescript
// ❌ Brittle — arbitrary delay
await page.waitForTimeout(2000)

// ✅ Wait for actual condition
await expect(page.getByRole("status")).toHaveText("Saved")
await expect(page.getByTestId("spinner")).not.toBeVisible()
await page.waitForURL("/dashboard")
```

### Authentication — use storageState to avoid re-login on every test

```typescript
// playwright.config.ts
export default defineConfig({
	use: {
		storageState: "e2e/.auth/user.json" // reuse logged-in state
	}
})

// e2e/auth.setup.ts — runs once, saves auth state
import { test as setup } from "@playwright/test"

setup("authenticate", async ({ page }) => {
	await page.goto("/login")
	await page.getByLabel("Email").fill("mail@example.com")
	await page.getByLabel("Password").fill(process.env.TEST_PASSWORD!)
	await page.getByRole("button", { name: "Sign in" }).click()
	await page.waitForURL("/drive")
	await page.context().storageState({ path: "e2e/.auth/user.json" })
})
```

### What Playwright should and shouldn't test

```
✅ Test with Playwright:
  - Critical user journeys (login → upload → share → download)
  - Flows that cross multiple pages or require real auth
  - File upload/download behavior
  - Form validation with real network responses
  - Navigation and routing

❌ Don't test with Playwright:
  - Unit-level logic (use Jest/Vitest/bun:test instead)
  - Every component state (use @testing-library instead)
  - Styling details (use visual regression tools if needed)
  - Things that can be verified without a browser
```

### Running Playwright

```bash
npx playwright test                           # run all
npx playwright test e2e/upload.spec.ts        # single file
npx playwright test --headed                  # show browser (debug)
npx playwright test --ui                      # interactive UI mode
npx playwright test --project=chromium        # single browser
npx playwright show-report                    # view last run report
```

---

## Maestro — Mobile E2E (React Native / Expo)

Maestro runs E2E tests against a real or simulated device. Tests are written in YAML and drive
the app through actual user interactions. Use for critical mobile flows that must be verified
on-device — not for unit logic.

### Detect Maestro

```bash
ls .maestro/ 2>/dev/null
find . -name "*.yaml" | xargs grep -l "appId" 2>/dev/null | head -5
which maestro 2>/dev/null
```

### File structure

```
.maestro/
  flows/
    login.yaml
    upload_file.yaml
    share.yaml
  config.yaml        # optional global config
```

### Writing Maestro flows

```yaml
# .maestro/flows/upload_file.yaml
appId: com.example.app
---
# Launch and navigate to upload
- launchApp
- tapOn: "Drive"
- tapOn:
      id: "upload-button" # use accessibility IDs when possible

# Wait for the upload sheet to appear
- assertVisible: "Upload from device"
- tapOn: "Upload from device"

# After file picker (handled natively), wait for upload
- assertVisible:
      text: "document.pdf"
      timeout: 10000 # ms — give uploads time

# Verify upload succeeded
- assertVisible: "Upload complete"
- assertNotVisible: "Uploading..."

# Navigate to the file and verify it appears in drive
- tapOn: "document.pdf"
- assertVisible: "Share"
- assertVisible: "Download"
```

### Locator strategy for Maestro

```yaml
# ✅ Best — testID / accessibility identifier set in code
- tapOn:
      id: "upload-button" # matches testID="upload-button" in RN

# ✅ Good — visible text
- tapOn: "Upload"

# ✅ Good — index when multiple elements match
- tapOn:
      text: "Delete"
      index: 0
# For React Native — set testID on interactive elements:
# <TouchableOpacity testID="upload-button" onPress={handleUpload}>
```

### Waiting and assertions

```yaml
# Wait for element with timeout (don't use fixed sleeps)
- assertVisible:
      text: "Sync complete"
      timeout: 15000

# Assert element is gone
- assertNotVisible: "Loading..."

# Input text
- tapOn: "Email"
- inputText: "mail@example.com"

# Scroll to find element
- scrollUntilVisible:
      element:
          text: "document.pdf"
      direction: DOWN
```

### Running Maestro

```bash
maestro test .maestro/flows/upload_file.yaml     # single flow
maestro test .maestro/flows/                      # all flows in directory
maestro studio                                    # interactive recorder
maestro test --device <id> flow.yaml             # specific device
```

### What Maestro should and shouldn't test

```
✅ Test with Maestro:
  - Critical user journeys end-to-end on real device/simulator
  - Flows that involve native OS interactions (file picker, camera, permissions)
  - Push notification handling
  - Deep link navigation
  - App state after backgrounding and foregrounding

❌ Don't test with Maestro:
  - Unit logic (use Jest/bun:test)
  - Component rendering (use @testing-library/react-native)
  - Anything that can be verified without a running device
  - Every possible state — only critical paths
```

---

## Bug Fix Protocol — Always Reproduce First

When fixing a bug, the sequence is non-negotiable:

```
1. Write a test that reproduces the bug (it must fail before the fix)
2. Confirm the test fails — run it and see the red
3. Fix the bug
4. Confirm the test passes — run it and see the green
5. Run the full test suite — confirm nothing else broke
```

This proves the fix works and prevents regression.

```typescript
// Example: bug report — formatBytes(0) returns 'NaN B' instead of '0 B'

// Step 1: write the reproducing test FIRST
it("formatBytes: handles zero correctly", () => {
	expect(formatBytes(0)).toBe("0 B") // this fails before the fix
})

// Step 2: run it — confirm it's red
// Step 3: fix the implementation
// Step 4: run it — confirm it's green
// Step 5: run full suite
```

---

## Coverage — What Good Looks Like

Coverage is a signal, not a goal. 100% coverage with weak assertions is worthless.
Good coverage means every meaningful behavior path is verified.

**Aim for:**

- All happy paths tested
- All error/failure paths tested
- Edge cases and boundaries tested
- Coverage naturally follows — don't chase the number

**Meaningful coverage check:**

```bash
# Vitest
npx vitest --coverage

# Jest
npx jest --coverage

# Rust
cargo tarpaulin --out Stdout   # requires: cargo install cargo-tarpaulin
```

If coverage reveals an untested branch — add a test for it. If the branch is genuinely
unreachable dead code, remove it.

---

## Test Data — Real API Data vs Mocks

When a test needs data that comes from an API or server, always check whether real data is
available before reaching for mocks. Real data finds bugs mocks never would.

### Decision flow

```
Does the test need API/server data?
│
├─ Can we hit the real API/server in this test environment?
│   (dev server running, test DB seeded, API key available, no rate limits, fast enough)
│   │
│   ├─ YES → Use real data. Write an integration test.
│   │         Seed or query the actual endpoint. Don't mock it.
│   │
│   └─ NO  → Ask the developer before inventing data:
│             "This test needs data from <endpoint/service>.
│              I can't reach it in this context. Should I:
│              (a) mock the response with example data?
│              (b) set up a test fixture/seed instead?
│              (c) skip this test and mark it as integration-only?"
│
└─ Never silently invent mock data that looks realistic.
   Always ask first when real data isn't available.
```

### Checking if the real API/server is reachable

```bash
# Is a local dev server running?
curl -s http://localhost:3000/health | head -5
curl -s http://localhost:8080/api/ping | head -5

# Is a test database configured?
cat .env.test 2>/dev/null | grep -E "DATABASE_URL|DB_HOST|DB_PORT"
cat .env.local 2>/dev/null | grep -E "DATABASE_URL|DB_HOST"

# Are API credentials available for a test environment?
cat .env.test 2>/dev/null | grep -E "API_KEY|API_URL|TEST_"

# Does the project have existing integration test setup?
find . -name "*.test.ts" | xargs grep -l "supertest\|testClient\|fetch.*localhost" 2>/dev/null | head -5
```

If the real endpoint is reachable, write a genuine integration test against it. Only fall back
to mocking when you've confirmed the real thing isn't accessible.

### When mocking is the right call

Mocking is appropriate when:

- External third-party APIs (payment providers, email services, cloud storage) — never call real ones in tests
- The API is unavailable in CI/test environments by design
- The operation has irreversible side effects (send email, charge card, delete data)
- The test needs to simulate specific error conditions (500, timeout, rate limit) that are hard to trigger on a real server
- Speed — a test suite that makes hundreds of real network calls is impractical

When mocking IS used, the mock data must accurately represent the real response shape. Check
the actual API response structure first (docs, existing calls in the codebase, or a real request),
then mirror it precisely.

```bash
# Find examples of real API responses already in the codebase
grep -r "mockResolvedValue\|mockReturnValue" src/ | head -10
find . -name "*.json" -path "*fixtures*" -o -name "*.json" -path "*mocks*" | head -10
```

---

## What NOT to Do

- **Don't write tests after the fact just to hit a number** — tests written to satisfy coverage metrics are usually weak. Write them to specify behavior.
- **Don't test implementation details** — if you refactor and tests break without behavior changing, the tests were wrong.
- **Don't share mutable state between tests** — every test must be independently runnable in any order.
- **Don't use `any` or loose assertions to make tests pass** — a test that always passes is worse than no test.
- **Don't leave `test.only` or `it.only` committed** — these silently disable the rest of the suite.
- **Don't ignore flaky tests** — a flaky test is a broken test. Fix or delete it.
- **Don't mock what you own** — mock external dependencies (HTTP, DB, time), not your own modules.
