# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

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

# Build with HEIF decoder support
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
  recomputation, and on `main` it is a normal test — exclude it from sweeps with
  `--skip size` unless you specifically mean to exercise the size endpoint. (Some branches
  carry an `#[ignore]` for it; there, run it with `--ignored` instead.)
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
| `malformed` | Test-only seams that put malformed state on the server on purpose (`create_malformed_dir` / `create_malformed_file` write arbitrary metadata) — never enable in production |
| `heif-decoder` / `avif-decoder` | Thumbnail decoding for HEIF/HEIC and AVIF |
| `bench-internals` | Exposes `cache::bench_support` for the insertion benchmark only |

The mobile bindings build is `-F uniffi,heif-decoder,http-provider,cache` (see
`filen-sdk-rs/web/ubrn.config.yaml`).

## Git Hooks

Hooks live in `scripts/git-hooks/` and are opt-in per clone/worktree:
`./scripts/git-hooks/install.sh` (sets `core.hooksPath`).

- **pre-commit** — `cargo fmt --all --check`, `cargo fmt` inside `filen-sdk-rs`, `taplo
  lint`/`fmt --check`, `sqlfluff` on staged `.sql`, then four clippy passes: workspace
  (`--exclude heif-decoder --all-targets`), `-p filen-sdk-rs -F uniffi,http-provider`, and
  wasm32 `-F wasm-full` + `-F service-worker` (both run from the `filen-sdk-rs` directory).
  **On a cold cargo cache this takes several minutes** — warm the cache by running those
  clippy invocations first, or the commit may be killed by a tool/CI timeout mid-hook.
  Missing `taplo` / `sqlfluff` / the wasm32 target are skipped with a warning.
- **pre-push** — heavier: feature-combination clippy, `clippy --tests`, full `sqlfluff
  lint .`, and `cargo test --lib --no-fail-fast` (`SKIP_TESTS=1` to skip).

The auto-formatter some editors/agents run on save does **not** match this repo's nightly
`cargo fmt` output and will fail the pre-commit gate. Run `cargo fmt -p <crate>` before
staging.

## Toolchain

Uses **nightly** Rust (`nightly-2026-02-20`) via `rust-toolchain.toml`. The nightly channel is required for the `higher-ranked-assumptions` feature. The `rust-src` component is required.

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

The `filen-sdk-rs/web/` directory contains a Node/Yarn project for WASM testing (see `wasm-test.sh`).

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

Incremental builds for `heif-decoder` are broken due to a cmake-rs bug. If only working on SDK/types, exclude it with `--exclude heif-decoder` or avoid enabling the `heif-decoder` feature.
