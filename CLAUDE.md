# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Contributing (read before your first commit)

These rules bind every contributor, human or agent, and take precedence over any tool's
defaults, commit attribution lines included.

- The hooks are required (see Git Hooks); agent sessions install them on start. Never
  commit or push with `--no-verify`; if a hook trips on unrelated work in progress, stash
  it and let it run.
- Ask the maintainer first, and never bundle into a larger change: new `unsafe` or
  `#![feature]`; a new dependency, `[patch]` section or `vendor/` directory; wiping keys or
  buffers (`zeroize` was rejected, use a redacting `Debug`); any binary file (test inputs
  are generated in the test, or pinned by URL and checksum as in
  `microthumb/tests/raw_fixtures/`); renaming or rewording anything shipped (public items,
  FFI/JS exports, error messages, callback names and payloads). The diff policy (see Git
  Hooks) blocks, on commit and in CI, added or changed `unsafe`, `[patch]`, `[replace]` or
  `[source]` sections, `vendor/`, git dependencies not pinned to a full `rev` or not under
  the maintainer's GitHub accounts, `zeroize`, and binary files that no `README.md` or
  `generate.sh` in their directory names. A new crates.io dependency is only listed, as a
  new `Cargo.lock` package, and `#![feature]` and renamed or reworded shipped items are
  left to review.
  The workspace lints deny `unsafe_code`: each existing site sits under a commented
  `#[allow(unsafe_code)]`. They deny `clippy::cast_possible_truncation` with no exceptions:
  a cast that could truncate becomes `T::try_from`, or `num_traits::ToPrimitive` from a
  float, and the diff policy blocks an allow of it. Every crate inherits them with
  `[lints] workspace = true`, and the diff policy blocks a `Cargo.toml` with a `[package]`
  table but without it.
- Fixes to pre-existing code and behaviour changes to shipped features are standalone
  commits at the front of the branch. Refactors change no behaviour. One logical change
  per commit, and follow-up fixes are folded into the commit they correct.
- Messages describe the change as an engineer would: cite only hashes that exist, no audit
  or review framing, no claim of a check you did not run, and no attribution trailers,
  session links, agent names or finding IDs (in PRs, tags and branch names too). Never
  commit planning notes, reports or screenshots.
- Without a test account, run `cargo test --lib` and
  `cargo test -p filen-sdk-rs --lib -F cache`, and name the live suites you did not run
  (`[test]` in a commit message or PR title makes `test.yml` run them in CI).
- Run the `house-review` skill over the branch before asking for review.
- The detailed rules are skills in `.claude/skills/`: `clean-rust` and
  `clean-rust-filen-rs` for any Rust, `types-serde-conventions` for API and binding types,
  `commit-work` and `fold-fixups` for history. Agents that do not load skills: `AGENTS.md`.

## Build & Test Commands

```bash
# Build entire workspace
cargo build

# Run all tests (requires env vars for integration tests)
cargo test

# Run tests for a specific crate
cargo test -p filen-sdk-rs
cargo test -p filen-mobile-native-cache

# Run a specific test
cargo test -p filen-sdk-rs --test file_tests test_name

# Unit tests only (no network, no test account needed)
cargo test --lib

# Cache engine: unit tests / live integration tests
cargo test -p filen-sdk-rs --lib -F cache
cargo test -p filen-sdk-rs --test cache_tests -F cache

# Lint
cargo clippy

# Build with HEIF decoder support (needs meson + ninja, and nasm on x86 —
# see Toolchain > Native build prerequisites)
cargo build --features heif-decoder
```

Integration tests require environment variables (can be in `.env`):
```bash
TEST_EMAIL="test@example.com"
TEST_PASSWORD="password"
TEST_SHARE_EMAIL="share@example.com"  # for sharing tests
TEST_SHARE_PASSWORD="password"
```

Test notes:

- Every test in a run shares the single account from your `.env`, and the server grants the
  named `drive-write` resource lock (`Client::lock_drive`) to one holder at a time, so
  drive-mutating tests (notably `cache_tests`) serialize on it. Expect long wall-clock times
  and avoid running several test binaries in one parallel pool — contention starves the
  convergence polls and tests start failing.
- `dir_tests::size` sleeps ~80 minutes waiting out the backend's throttled size
  recomputation, so it is `#[ignore]`d. Run it explicitly with
  `cargo test -p filen-sdk-rs --test dir_tests size -- --ignored` when you mean to exercise
  the size endpoint.
- `filen-mobile-native-cache` is a UniFFI crate; build/test it with
  `cargo build -p filen-mobile-native-cache`.

## Feature Flags

`filen-sdk-rs` (most flags are additive and off by default; `default = ["multi-threaded-crypto"]`):

| Feature | What it enables |
|---------|-----------------|
| `cache` | SQLite metadata cache (`src/cache/`, `rusqlite`); gates the `cache_tests` / `cache_search_tests` test targets |
| `uniffi` | UniFFI scaffolding for the mobile bindings |
| `http-provider` | Local `axum` server handing out `http://127.0.0.1` URLs for remote files (range requests), so platform media players can stream them |
| `wasm-full` | Browser WASM build (with threads) |
| `service-worker` | WASM service-worker build |
| `multi-threaded-crypto` | `rayon` / `wasm-bindgen-rayon` parallel crypto |
| `malformed` | Test-only seams that put malformed state on the server on purpose (`create_malformed_dir` / `create_malformed_file` write arbitrary metadata, `create_dir_with_name_hash` bypasses the case-insensitive name dedup) — never enable in production |
| `sync-engine` | Local folder <-> remote folder sync engine (`src/sync_engine/`); implies `cache`, adds the `notify` watcher; native only. Gates the `sync_engine_blackbox_tests` / `sync_engine_stress_tests` / `sync_suite` test targets |
| `heif-decoder` | Thumbnail decoding for HEIF/HEIC — and AVIF, which the vendored libheif decodes through the same container path on its dav1d backend |
| `bench-internals` | Exposes `cache::bench_support` for the insertion benchmark only |

The mobile bindings build is `-F uniffi,heif-decoder,http-provider,cache` (see
`filen-sdk-rs/web/ubrn.config.yaml`).

## Git Hooks

Hooks live in `scripts/git-hooks/` and are required. Claude Code sessions install them
automatically (`.claude/hooks/SessionStart.sh`, which also lists any tools the hooks need that
are missing); otherwise run `./scripts/git-hooks/install.sh` once per clone or worktree (it
sets `core.hooksPath`).

The split between them is `heif-decoder`: **nothing pre-commit runs enables it**, so
committing never triggers a vendored C++ build and needs no `meson` / `ninja` / `nasm` /
wasi-sdk. Every `heif-decoder` pass lives in pre-push.

- **pre-commit** — `cargo fmt --all --check`, `cargo fmt` inside `filen-sdk-rs`, `taplo
  lint`/`fmt --check`, `sqlfluff` on staged `.sql`, then five clippy passes: workspace
  (`--exclude heif-decoder --all-targets`), `-p filen-sdk-rs -F uniffi,http-provider`,
  `-p filen-sdk-rs -F sync-engine,malformed --all-targets` (the same command as ci-linux), and
  wasm32 `-F wasm-full,cache` + `--no-default-features -F service-worker` (both run from the
  `filen-sdk-rs` directory). **On a cold cargo cache this takes several minutes** — warm the
  cache by running those clippy invocations first, or the commit may be killed by a tool/CI
  timeout mid-hook. A missing tool stops the commit with an install message instead of
  skipping its check: `taplo`, the wasm32 target, brew LLVM on macOS (the `cache` wasm pass
  cannot run without it, and dropping the feature would report ok on a configuration nobody
  ships), and `sqlfluff` when `.sql` files are staged. It also runs
  `scripts/check-diff-policy.sh` on the staged changes, which blocks what needs the
  maintainer's approval (see Contributing): `[patch]`/`[replace]`/`[source]` sections,
  `vendor/`, git dependencies not pinned to a full `rev` or not in a repository under
  `github.com/Enduriel` or `github.com/FilenCloudDienste`, `zeroize`, added or changed
  `unsafe` code, an allow of `clippy::cast_possible_truncation` (it has no exceptions) or of
  `clippy::pedantic` (which would switch it off), and binary files, added or changed, whose
  name no `generate.sh` or `README.md` in the same directory mentions. It also lists, without
  blocking, new `Cargo.lock` packages and new majors of existing ones (`sha2 0.11`), each to
  be justified in the PR, and every added `#[allow]`/`#[expect]` line, each of which needs a
  comment naming its tradeoff. Commit an approved change with `SKIP_DIFF_POLICY=1`;
  nothing else is a reason to set it. Branch Lint still reports it, in its last step, so the
  PR shows what was approved.
- **commit-msg** — rejects `Co-Authored-By` trailers (in any case, indented or spaced),
  generated-by lines and the robot emoji, session links, review finding IDs (`F042`), audit
  or review framing (`auditor`, `audit finding`, `audit follow-up`, `per the audit`,
  `review finding`, `review found`), and cited commit hashes that do not exist in this
  repository; cite another repository's commit as `<repo>@<sha>`.
  `fixup!`/`squash!`/`amend!` subjects pass, so the `fold-fixups` flow works.
- **pre-push** — first refuses to push unfolded `fixup!`/`squash!`/`amend!` commits. Then
  the heavier checks: the `heif-decoder` feature-combination clippy, `clippy --tests`,
  both wasm32 passes on the feature sets `ci-wasm` uses (`-F wasm-full,cache,heif-decoder`
  and `--no-default-features -F service-worker,heif-decoder`, i.e. what `wasm-pack.sh`
  ships), full `sqlfluff lint .`, and `cargo test --lib --no-fail-fast`. This is the hook
  that needs `meson` / `ninja` (/ `nasm` on x86) and `WASI_SDK_PATH`; it stops with an
  install message rather than degrading the feature set. `SKIP_HEAVY_CLIPPY=1` builds no
  vendored C++ at all — it skips the `heif-decoder` passes, native and wasm, *and* adds
  `--workspace --exclude heif-decoder` to the remaining passes, which would otherwise build
  the crate anyway (see below); it is the escape hatch if you have no wasi-sdk; `SKIP_TESTS=1` and `SKIP_SQLFLUFF=1` skip theirs.

Neither hook is CI parity, by design: both run a subset. `ci.yml` additionally lints with
`filen-sdk-rs/cache` in the all-features pass, runs `clippy --tests -F cache`, and
cross-compiles for Android, iOS and Windows — none of which the hooks attempt. `ci.yml`
runs only on `main`, `dev` and pull requests into them. Every other branch push, and every
pull request into `main` or `dev` (from a fork too), runs `branch-lint.yml`, the fast checks
a branch that skipped the hooks would miss: the diff policy and commit-message checks
against the pull request's target branch (`origin/main` for a push), unfolded fix-ups,
`cargo fmt`, `taplo`, and the `-p filen-sdk-rs -F uniffi,http-provider` clippy pass.

The sync engine's live tests run only nightly (or on a `[test]` commit/PR), in `test.yml`'s
`test-sync-engine` job: after the matrix and 2FA legs, Linux, V2 account, one
`!cancelled()` step per binary — `cache_tests`, `cache_search_tests`,
`sync_engine_blackbox_tests` and `sync_suite`, all `-p filen-sdk-rs -F sync-engine,malformed`,
the engine binaries with `--test-threads=1`.

The auto-formatter some editors/agents run on save does **not** match this repo's nightly
`cargo fmt` output and will fail the pre-commit gate. Run `cargo fmt -p <crate>` before
staging.

## Toolchain

Uses **nightly** Rust (`nightly-2026-02-20`) via `rust-toolchain.toml`. The nightly channel is required for the `higher-ranked-assumptions` feature. The `rust-src` component is required.

### Native build prerequisites

Everything below exists because `heif-decoder` compiles vendored C++ (libheif, libde265) and
vendored dav1d from source on *every* target. **You need the first four rows even if you never
touch thumbnails**: `heif-decoder` is a workspace member and the workspace declares no
`default-members`, so *any* cargo command run at the repo root builds it, whatever features
you asked for (`cargo build --unit-graph` at the root plans its `run-custom-build`). Leaving
the feature off does not help; `--workspace --exclude heif-decoder`, or a package-scoped
`-p <crate>` command, is the only way to skip them. Build scripts also run under
`cargo clippy`, not just `cargo build`.

| Tool | Needed for | Install |
|------|-----------|---------|
| submodules | any workspace-root cargo command — `heif-decoder/deps/` are git submodules | `git submodule update --init --recursive` |
| `cmake`, C++ toolchain | any workspace-root cargo command (libheif/libde265) | macOS: Xcode CLT · Ubuntu: `apt install cmake clang libc++-dev libc++abi-dev` |
| `meson`, `ninja` | any workspace-root cargo command — vendored dav1d, all targets | `brew install meson ninja` · `apt install meson ninja-build` · `choco install meson ninja` |
| `nasm` | any workspace-root cargo command on **x86/x86_64 only** (dav1d's SIMD); not used on arm64 or wasm | `brew install nasm` · `apt install nasm` · `choco install nasm` (add `C:\Program Files\NASM` to PATH) |
| brew LLVM | **macOS + wasm32 with `cache`** — Apple clang cannot target wasm32 for the bundled SQLite | `brew install llvm`; `wasm-pack.sh` and the hooks point `CC_wasm32_unknown_unknown` / `AR_wasm32_unknown_unknown` at it |
| `WASI_SDK_PATH` | **wasm32 with `heif-decoder` only** — there is no host C++ runtime for `wasm32-unknown-unknown` | see below |
| wasm32 target | any wasm build | `rustup target add wasm32-unknown-unknown` |

wasi-sdk, as `.github/workflows/ci.yml`'s `ci-wasm` job installs it (release **33**; >= 25 is
the floor, it must carry the `wasm32-wasi-threads` sysroot). Swap the asset for your host:

```bash
curl -sSfL -o /tmp/wasi-sdk.tar.gz \
  https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-33/wasi-sdk-33.0-arm64-macos.tar.gz
mkdir -p ~/wasi-sdk && tar -xzf /tmp/wasi-sdk.tar.gz -C ~/wasi-sdk --strip-components=1
export WASI_SDK_PATH=~/wasi-sdk   # must contain share/wasi-sysroot
```

`filen-sdk-rs/wasm-pack.sh` and the pre-push hook both refuse to run without it, rather than
building a configuration that never ships.

## Workspace Structure

| Crate | Purpose |
|-------|---------|
| `filen-sdk-rs` | Core SDK — auth, file ops, crypto, FS abstraction, WebSocket |
| `filen-types` | Shared type definitions and serde utilities for all API types |
| `filen-macros` | Proc-macros: `#[shared_test_runtime]`, `#[js_type]`, derive macros (`HasUUID`, `HasName`, `HasParent`, etc.) |
| `filen-mobile-native-cache` | UniFFI bindings for iOS/Android; SQLite cache; sync between local and remote |
| `heif-decoder` | HEIF/HEIC decoder built from `libheif`/`libde265` C++ sources (git submodules in `deps/`) |
| `test-utils` | Shared integration test infrastructure (accounts, cleanup, async runtime) |
| `uniffi-bindgen` / `uniffi-bindgen-swift` | Thin wrappers to drive UniFFI codegen for Kotlin and Swift |
| `filen-cli` | CLI tool for interacting with Filen drive |
| `filen-rclone-wrapper` | Rclone integration wrapper |

## Architecture

### Data Flow
```
Mobile Apps (iOS/Android)
    ↕ UniFFI (Kotlin/Swift)
filen-mobile-native-cache   ← SQLite metadata cache, sync logic
    ↕
filen-sdk-rs                ← core SDK: auth, FS ops, crypto, HTTP, sockets
    ↕
filen-types                 ← API request/response types, crypto primitives
    ↕
Filen Backend (HTTPS/JSON)
```

### `filen-sdk-rs` Internal Structure

- **`auth/`** — `Client` struct (the main entry point), HTTP client stack (Tower middleware: rate limiting, retry, bandwidth limits, auth injection), auth versions V1/V2/V3
- **`fs/`** — File system abstraction using a `Category` trait system with three implementations:
  - `Normal` — standard user drive
  - `Shared` — shared-with-me items
  - `Linked` — public link items
  - Generic enums (`DirType`, `NonRootItemType`, `RootItemType`) parameterized over `Category`
- **`api/v3/`** — thin wrappers around each Filen API endpoint (mirrors `filen-types/src/api/v3/`)
- **`crypto/`** — AES-GCM file encryption (v1/v2/v3), RSA, PBKDF2/Argon2 key derivation
- **`socket/`** — WebSocket event listener (native via `tokio-tungstenite`, WASM via `web-sys`)
- **`io/`** — local filesystem tree operations for sync
- **`sync/`** — drive locking (`ResourceLock`) and sync state
- **`sync_engine/`** (`sync-engine`, native only) — keeps a local folder in sync with a remote folder: cache snapshot + local `scan` + persisted SQLite `baseline` → `plan` → `guard` (mass-delete screen) → `apply`, plus `watch` (the continuous `notify` loop), `pause` and the `outcome`/`events` reporting types

### `filen-types` Internal Structure

Types mirror the API surface: `src/api/v3/{dir,file,user,auth,...}/`. Custom serde in `src/serde/` handles API-specific formats (hex, timestamps, RSA keys, parent UUIDs).

### Encryption Versions

- **V1**: Legacy (MD5/SHA1-based)
- **V2**: PBKDF2 + AES-GCM, master keys
- **V3**: Argon2 + AES-GCM, DEK (Data Encryption Key) model

The `Client` dispatches to the correct version at runtime via `AuthInfo` enum.

### `filen-macros` Key Macros

- `#[js_type(import, export, wasm_all)]` — generates WASM/UniFFI bindings for types
- `#[shared_test_runtime]` — wraps async test functions with a shared Tokio runtime
- `#[derive(HasUUID, HasName, HasParent, HasRemoteInfo, HasMeta, CowHelpers)]` — derive traits used throughout `fs/`

### WASM / Platform Targets

`filen-sdk-rs` compiles to three targets:
- **Native** (default) — uses Tokio multi-threaded runtime, `tokio-tungstenite`, file system access
- **WASM** (`target_family = "wasm"`) — uses `wasm-bindgen`, `web-sys` WebSocket, `wasm-bindgen-rayon`
- **UniFFI** (`feature = "uniffi"`) — generates FFI scaffolding for mobile; used by `filen-mobile-native-cache`

The `filen-sdk-rs/web/` directory contains the npm project for the browser WASM tests
(`web/main.test.ts`). Build the bindings with `bash wasm-pack.sh` run from `filen-sdk-rs/`
(the script uses relative paths and needs `WASI_SDK_PATH`), then run
`npm --prefix filen-sdk-rs/web test`.

### Mobile Consumers

The mobile app repo (`filen-ts`) consumes this repo **twice**, in two different ways:

- `packages/filen-mobile/filen-rs` is a real **git submodule** (declared in `.gitmodules`,
  pointing at this repo's GitHub URL) — a separate checkout, normally detached at the
  recorded commit. Both mobile platforms build `filen-mobile-native-cache` (the Drive cache)
  from *that* checkout, via the expo prebuild plugins `plugins/withAndroidRustBuild.ts` and
  `plugins/withFileProvider.ts`, which run cargo in `<projectRoot>/filen-rs` (release,
  `-F heif-decoder`). Testing a branch on device means getting the branch into the submodule;
  landing it means a submodule pointer bump.
- `@filen/sdk-rs` — the `ubrn` (uniffi-bindgen-react-native) build of `filen-sdk-rs` that
  powers Notes/Chats and transfers — is consumed as an ordinary **published npm dependency**
  pinned in `packages/filen-mobile/package.json`. Rebuilding `filen-sdk-rs/web` locally does
  **not** affect the app unless you deliberately override that install (link/symlink it).

See the `mobile-build` skill in `.claude/skills/` for the build commands and gotchas.

### Incremental Build Note

Incremental builds for `heif-decoder` are broken due to a cmake-rs bug. If only working on SDK/types, exclude it with `--workspace --exclude heif-decoder`. Not enabling the `heif-decoder` feature is **not** enough — it is a workspace member, so a workspace-root cargo command builds it regardless.
