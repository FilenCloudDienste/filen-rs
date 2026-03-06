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

## Toolchain

Uses **nightly** Rust (`nightly-2026-02-20`) via `rust-toolchain.toml`. The nightly channel is required for the `higher-ranked-assumptions` feature. The `rust-src` component is required.

## Workspace Structure

| Crate | Purpose |
|-------|---------|
| `filen-sdk-rs` | Core SDK — auth, file ops, crypto, FS abstraction, WebSocket |
| `filen-types` | Shared type definitions and serde utilities for all API types |
| `filen-macros` | Proc-macros: `#[shared_test_runtime]`, `#[js_type]`, derive macros (`HasUUID`, `HasName`, `HasParent`, etc.) |
| `filen-mobile-native-cache` | UniFFI bindings for iOS/Android; SQLite cache; sync between local and remote |
| `anchored-ref` | Utility crate for self-referential / anchored reference types |
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

### Incremental Build Note

Incremental builds for `heif-decoder` are broken due to a cmake-rs bug. If only working on SDK/types, exclude it with `--exclude heif-decoder` or avoid enabling the `heif-decoder` feature.
