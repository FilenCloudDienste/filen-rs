---
name: clean-rust-filen-rs
description: Load before writing, editing or reviewing Rust anywhere in this repository (filen-sdk-rs, filen-types, filen-cli, microthumb, tests included), together with the clean-rust skill. The house rules the maintainer enforces in review - what already exists and must be reused, module layout, the job and API shape to copy from fs/copy, error, FFI and wasm, drive-lock, untrusted-input, test and docs rules, and a pre-handoff checklist - plus the pinned toolchain, enabled unstable features, in-repo exemplars and known debt not to copy.
---

**Load the `clean-rust` skill first** (`.claude/skills/clean-rust/SKILL.md`). It carries the generic
rules; this file carries the filen-rs facts and house rules.

## Toolchain

`rust-toolchain.toml` pins `channel = "nightly-2026-02-20"` (`rustc 1.95.0-nightly`),
`components = ["rust-src"]`, every crate on edition 2024. Its comment cites
`higher-ranked-assumptions` (rust-lang/rust#143545) for `Send` inference in `handle_request`.

`higher-ranked-assumptions` is a `-Z` compiler flag, not a `#![feature]` — probing
`#![feature(higher_ranked_assumptions)]` on this pin gives `error[E0635]: unknown feature`. It is
enabled as `-Zhigher-ranked-assumptions` in `.cargo/config.toml` (`[build] rustflags` and
every `[target.*] rustflags` table) and in the workflow env overrides
(`git grep -n Zhigher-ranked-assumptions -- '*.toml' '*.yml' '*.yaml'` lists every site);
keep it in every new target table and override. It is why the crate is on nightly at
all. Because it lives in `build.rustflags`, `cargo +1.95.0 …` run from inside the repo fails with
``the option `Z` is only accepted on the nightly compiler`` — run stable comparisons from a cwd
outside the repo, or with `RUSTFLAGS=""`.

### Already enabled — no ask needed

| Crate root | Gate | One real use site |
|---|---|---|
| `filen-sdk-rs/src/lib.rs` | `min_specialization` | `default fn` in the Serialize tower layer, `auth/http/serialize.rs` |
| | `try_with_capacity` | fallible alloc from an untrusted `Content-Length`, `auth/http/download_body/full.rs` |
| | `ascii_char` | `char::as_ascii()` table lookup in filename validation, `fs/name.rs` |
| | `iter_intersperse` | joining remote path segments with `/`, `io/fs_tree/tree/mod.rs` |
| | `cfg_select` | wasm-vs-native `SpawnTaskHandle::poll` body, `runtime.rs` |
| `filen-types/src/lib.rs` | `min_specialization` | `default fn into_data` overridden for `()`, `api/response.rs` |

`lib.rs` says `cfg_select` is "stable in 1.95"; on this exact pin it still gates. Believe the
compiler, not the comment.

### Compile-verified on this pin — worth a stop-and-ask when they make the code cleaner

| Gate | Stable shape you would otherwise write |
|---|---|
| `try_blocks` | IIFE closure `(\|\| { ...; Ok(y) })()` to scope `?` mid-fn |
| `return_type_notation` | name + box an associated future just to write a `Send` bound |
| `arbitrary_self_types` | a method on a custom smart pointer as `fn f(this: &MyPtr<Self>)` — `self: &Arc<Self>`, `self: Rc<Self>`, `self: Pin<&mut Self>` are stable already, no ask |
| `gen_blocks` / `coroutines` | hand-rolled resumable iterator struct (`FSTreeDFSIteratorWithPath`) |
| `negative_impls` | `PhantomData<*const ()>` to lose an auto trait implicitly |
| `type_alias_impl_trait`, `impl_trait_in_assoc_type` | `Pin<Box<dyn Future + Send>>` associated type |
| `trait_alias` | marker trait + blanket impl |
| `associated_type_defaults` | blanket impl or macro-generated per-type impls |
| `box_patterns` | `match *b { .. }` moves out of the `Box` on stable — ask only for *nested* box patterns |
| `iterator_try_collect`, `try_find`, `iter_map_windows`, `iter_array_chunks` | `collect::<Result<_,_>>()`, manual loop, `collect` + `windows` |
| `map_try_insert` | `match m.entry(k) { Entry::Vacant(v) => v.insert(..), Entry::Occupied(o) => .. }` (single lookup) |
| `btree_cursors` | `range` per step |
| `default_field_values` | `..Default::default()` |
| `never_type` | `std::convert::Infallible` |

Also compile on this pin, rarely worth an ask: `closure_lifetime_binder`, `const_trait_impl`,
`adt_const_params`, `exhaustive_patterns`, `yeet_expr`, `try_trait_v2`, `async_fn_traits`,
`async_iterator`. Internal-only, discouraged even though they compile: `pattern_types`,
`unsized_fn_params`, `rustc_attrs` — prefer a validated newtype (`fs/name.rs`) over `pattern_types`.

### Just use it — stable on 1.95, no gate, no ask

`let_chains` (1.88), `slice_as_chunks` (1.88), `result_flattening` (1.89),
`precise_capturing_in_traits` (1.87), `array_windows` (1.94), `if_let_guard` (1.95). Also
`Option::zip` (1.46 — only `zip_with` is still gated) and moving out of a `Box` with `match *b`.

### Do not use on this pin — pick the stable shape

Removed: `associated_const_equality` (`error[E0557]`, removed in 1.94.0, merged into
`min_generic_const_args`). Compile but are `incomplete_features` — may crash the compiler, not
eligible for an ask: `specialization` (use `min_specialization`), `generic_const_exprs`,
`inherent_associated_types`, `lazy_type_alias`, `non_lifetime_binders`, `deref_patterns`,
`never_patterns`, `explicit_tail_calls`.

## Exemplars — copy these shapes

| Pattern | Exemplar | What to look at |
|---|---|---|
| Validated newtype, one parser | `filen-sdk-rs/src/fs/name.rs` `ValidatedName` | private field; `parse_name` is the only entrance — `Deserialize`, `TryFrom<&str>`, uniffi `try_lift` all route through it. `EntryNameError` is the structured-error shape |
| Mint-only newtype | `filen-types/src/stable_uuid.rs` `StableUuid` | the server mints it, so no constructor: serde, `FromSql`, uniffi lift only, plus the `test-seams` hatch |
| Invariant = "the OS resolved this" | `filen-sdk-rs/src/io/canonical_path.rs` `CanonicalPath` | `create_descendant_path` guards each component before `PathBuf::push` |
| Secret `Debug` | `filen-sdk-rs/src/crypto/rsa.rs` `HMACKey` | hand-written `Debug` prints a digest, never bytes |
| Marker generic | `filen-sdk-rs/src/fs/categories/mod.rs` `Category` | `DirType<'a, Cat: Category + ?Sized>` — the marker threaded through the whole item tree |
| Required + blanket-`Ext` split | `filen-types/src/traits.rs` `CowHelpersExt` | required `CowHelpers` vs blanket ext. Same split at scale: `fs/categories/fs.rs` `CategoryFS` vs `CategoryFSExt` |
| Consuming transition | `filen-sdk-rs/src/fs/file/write.rs` `into_waiting_for_drive_lock_state` | per-phase structs; the drive-lock `Arc` exists only in the phases that must hold it |
| Unconstructible-until-finished | `filen-sdk-rs/src/io/fs_tree/entry/mod.rs` `UnfinalizedDirEntry` | `DirEntry::from_unfinalized` — child ranges unreadable until the subtree finishes |
| One type, borrowed + owned | `filen-types/src/traits.rs` `CowHelpers` (derive at `filen-macros/src/lib.rs`) | derive it instead of writing `FooBorrowed`/`FooOwned`; payoff at `cache/state/event.rs` `CacheEventType<'a>`, persisted as `Arc<CacheEvent<'static>>` (`cache/state/mod.rs`, `Arc` reason in the doc comment) |
| cfg'd `Send`/`Arc` split | `filen-sdk-rs/src/util.rs` | `MaybeSendBoxFuture`/`MaybeSendSync`/`MaybeSend`/`MaybeSendCallback`/`MaybeArc`/`MaybeArcWeak` — the **only** place `target_family = "wasm"` may split `Send`/`Arc` |
| Borrowed parse | `filen-mobile-native-cache/src/ffi/mod.rs` `UuidFfiId<'a>` | `&'a str` slices, no `String` per segment |
| Borrow-first params | `filen-sdk-rs/src/io/canonical_path.rs` `create_descendant_path` | `impl Iterator<Item = &'a str> + Clone`, cloned once to precompute capacity. `auth/mod.rs` `get_avatar_url` — `Arc<str>` cloned to escape a read guard |
| Poll state machine | `filen-sdk-rs/src/fs/file/write.rs` `FileWriterState` | one variant per phase + terminal `Error(&'a str)`; `take_with_err` = `mem::replace`; `poll_close` reinstalls the exact state on `Pending`; `DummyFuture` + `FileWriterDefault` is the default-callback trick |
| pin_project enum future | `filen-sdk-rs/src/auth/http/download_body/full.rs` `DownloadBodyFuture` | `self.set(..)` inside a loop to advance stages in one poll |
| Cancellation without a flag | `filen-sdk-rs/src/socket/thread_handling.rs` `ListenerRegisterGuard` | `PinnedDrop` + `Option::take` as the completion signal |
| Pin without projection | `filen-sdk-rs/src/http_provider/mod.rs` `IdleTimeout` | `S: Unpin` + `get_mut()`/`Pin::new`. `auth/http/logging.rs` `LoggedFuture` — thin delegating wrapper, no state machine |
| Errors / NonZero | `filen-sdk-rs/src/error.rs` `InvalidTypeError` | tiny thiserror struct per failure + `impl_from!` onto an opaque `ErrorKind` (`FilenSdkError`), with `downcast` / `downcast_ref` to get the fields back. `consts.rs` `FILE_CHUNK_SIZE: NonZeroU32` — no downstream `assert!(n > 0)` |

`Category` is a marker-generic exemplar, **not** a sealing one: `#[allow(private_bounds)]
type Client: SharedClient` (`SharedClient` is the `pub(crate)` trait at
`auth/shared_client.rs`) only restricts which client type an impl may pick — `auth::Client` is
`pub` and implements it, so a downstream `impl Category for X { type Client = Client, .. }` compiles.
Seal with the `clean-rust` skill's private-module `Sealed` supertrait if closure ever matters.

## Repo conventions

- **Hard tabs** (`rustfmt.toml: hard_tabs = true`). The editor/agent save-formatter does not match
  this repo's nightly `cargo fmt`; run `cargo fmt -p <crate>` before staging or pre-commit fails.
- The root `Cargo.toml`'s `[workspace.lints]` (every member opts in with
  `[lints] workspace = true`) denies `unsafe_code`, `clippy::dbg_macro`, `clippy::todo` and
  `clippy::self_named_module_files`. There is no `clippy.toml`, and nothing enforces the import or
  allow-comment conventions mechanically; review does.
- `pin_project` (proc-macro, `project = Name`) is used **only** under `filen-sdk-rs/src/auth/http/`;
  everything else (`runtime.rs`, `socket/thread_handling.rs`, `js/managed_futures/*`) uses
  `pin_project_lite`. Follow the neighbourhood; do not mix them in one file.
- Baseline `unsafe`: every existing site sits under an `#[allow(unsafe_code)]` whose comment says
  what the unsafe is for — `git grep -n "allow(unsafe_code)" -- '*.rs'` is the inventory. Two are
  wide: the module-level one on `filen-types/src/serde/str` (fixed-size string newtypes) and the
  crate-level one in `heif-decoder/src/lib.rs` (the C FFI surface); the rest cover one small
  module, item, statement or match arm (`filen-sdk-rs/src/runtime.rs`, `fs/file/chunk.rs`,
  `crypto/{v1,v2,v3}.rs`, `js/**` wasm ABI lifts, `obs/mod.rs`,
  `filen-mobile-native-cache/src/{auth.rs,env.rs}`, a few tests). New unsafe outside those
  scopes fails the lint; adding an allow or widening one needs the ask from the `clean-rust`
  skill and a `// SAFETY:` comment, and `scripts/check-diff-policy.sh` blocks added or changed
  unsafe lines either way.
- `filen-mobile-native-cache` is UniFFI: build/test with `-p filen-mobile-native-cache`.
- Touching API request/response types or serde? Pair with the `types-serde-conventions` skill.

## Known debt — do not copy

Fixing any of these is its own commit, offered before it is made.

- `fs/dir/meta.rs` `pub name: Cow<'a, str>` — `fs/dir/mod.rs` validates via `ValidatedName`
  then `.into()`s the proof away, and the `pub` field lets anyone skip it. `DirectoryMetaChanges`
  (`meta.rs`) keeps `Option<ValidatedName>` — that is the shape to follow.
- `filen-mobile-native-cache/src/ffi/mod.rs` `FfiId(pub String)` — documented grammar, zero
  enforcement, infallible `From<&str>`; every reader re-parses. Parse once into an enum.
- `filen-cli/src/util.rs` `RemotePath(pub(crate) String)` — crate-visible field bypasses `new()`'s
  leading-slash normalization that every method assumes.
- `fs/file/write.rs` `upload_key: Arc<String>` — read only as `&str`; `Arc<str>` is one allocation.
- `fs/file/write.rs` hand-rolls
  `Arc::try_unwrap(..).unwrap_or_else(|a| (*a).clone())`; `Arc::unwrap_or_clone(x)` is stable std.
- `filen-mobile-native-cache/src/io/mod.rs` `io_upload_updated_file`'s `name: String` param is only borrowed inside, forcing
  `meta.name.clone()` at `filen-mobile-native-cache/src/remote.rs`. Same fn's `mime: String` is genuinely consumed —
  the contrast is the lesson.
- `fs/file/write.rs` write-after-close answered with `io::Error::other("...")`; its
  cascade needs dead `unreachable!()` arms; `#[allow(clippy::large_enum_variant)]` with no
  stated tradeoff. Do not grow this file's cascade — new phases go behind consuming transitions.
- House answer for poll-after-`Ready` is the typed terminal variant (`FileWriterState::Error(&'a str)`,
  `write.rs`) returned as `Ready(Err(..))`. `fs/file/exif.rs`
  `panic!("poll_finalize called after completion")` is the outlier — convert it when touched; add no
  new panics.
- `filen-mobile-native-cache/src/error.rs` — 15 stringly variants over `ErrorContext(Cow)`;
  `.context()` re-`format!`s and the source type is unrecoverable. `filen-sdk-rs/src/error.rs`
  (boxed source + `downcast`) already has the pattern to copy.
- `fs/file/meta.rs` bare `serde_json::to_string(..).unwrap()`; the sibling at `fs/dir/meta.rs`
  shows the house style (justifying comment + `.expect`).
- No trait here uses the private-module `Sealed` form yet; the three `#[allow(private_bounds)]` sites
  (`filen-types/src/traits.rs`, `fs/categories/mod.rs`, `fs/categories/fs.rs`) suppress the
  lint instead, none of them with a comment naming the tradeoff (`fs.rs` explains only the
  neighbouring `async_fn_in_trait` allow), and `traits.rs`'s blanket `impl<T: CowHelpers> Sealed
  for T` seals nothing. New sealed traits use the `clean-rust` skill's form.
- `#[must_use]` appears **nowhere** in the repo. Do not add it in feature work; if builders or
  fallible constructors need it, propose that as a separate change.

## House rules the maintainer enforces in review

Every rule below was a review correction on a real branch. Apply them while writing, then
walk the checklist at the end before handing work over. The `house-review` skill runs the
same rules over a finished branch.

### Reuse before writing

Search before adding a helper, constant, type or dependency: `git grep -n NAME -- '*.rs'`.
What already exists on main:

| Need | Use |
|------|-----|
| Long-running drive job plumbing (control, cancellation, accounting) | `crate::job` (`job.rs`; test helpers in `job::test_support`) |
| Spawning tasks on native and wasm | `crate::runtime` (`spawn_local`, `spawn_async`, `spawn_task_maybe_send`) |
| Chunk size, open-file limit and other shared numbers | `crate::consts` (`CHUNK_SIZE_U64`, `MAX_OPEN_FILES`); tests derive sizes from these |
| Send/Sync bounds and `Arc` that differ on wasm | the `MaybeSend*` / `MaybeArc` aliases in `util.rs` |
| Name validation | `ValidatedName` (`fs/name.rs`) |
| Holding the drive lock | `Client::lock_drive` (`sync/mod.rs`) |
| Client-wide settings a job reads | a `Client` accessor such as `thumbnails()` (`auth/mod.rs`), not a copy in each job's config |
| Listing and creating items from a job | the `fs::dir` / `fs::file` `client_impl` helpers |
| Catching a panic in async work | `AssertUnwindSafe(fut).catch_unwind()` as in `catch_parse_panic` (`fs/file/exif.rs`); native and uniffi only — wasm32 traps before unwinding |
| `Debug` for a type holding a secret | the hashed `Debug` on `FileKey` (`crypto/v1.rs`) |
| Ordered callback delivery on uniffi | `crate::js::spawn_ordered_dispatch` (`js/uniffi.rs`, uniffi-only) |
| Live-test accounts, directories and cleanup | `test-utils` |

Check `std` and the crates already in `Cargo.toml` before writing an algorithm by hand (for
example `cbc` for CBC chaining, `str::floor_char_boundary`). Adding a crate is the
maintainer's decision.

When a second job needs something the first job has, lift it into a shared module
(normally `crate::job`), migrate every caller and delete the original in the same series.
Never copy it and adapt. Code in a shared module is named job-neutrally, and core modules
(`fs`, `auth`, `io`) never import from a feature module such as `fs::copy`.

One type per concept: sibling jobs share their item and error types, and event payloads
embed the entry structs instead of re-declaring their fields. Extract a repeated block on
its second occurrence.

### Module layout and size

- A module with children is `foo/mod.rs`; never `foo.rs` next to `foo/`.
- Split work into named phases (see `fs/copy/plan.rs` and `fs/copy/engine`) instead of one
  long function, and group more than a handful of parameters into an options struct. Do not
  add a new `too_many_arguments` allow.
- Visibility starts private, then `pub(crate)`; `pub` only for items re-exported on the
  crate's public surface. A public module is private with `pub use` of its items, and every
  item has exactly one export path.
- Imports go in one `use` block at the top of the file: no function-local `use`, no aliases
  that shadow a crate module.
- A new module that parses untrusted input or runs a job may copy the header of `cache/mod.rs`,
  the one module that turns extra lints on: `#![warn(unreachable_pub, unused_qualifications)]`.
  Indexing and arithmetic follow the rules under Validation below.

### Public API and job shape: copy `fs/copy`

- A job's public entry point returns `Result<XReport, XFailed>`, where `XFailed` carries
  the partial report (see `copy_items` in `fs/copy/client_impl.rs`). No outcome enums.
- Entry point: `self: Arc<Self>` taking `(.., config, callback, control)` like `copy_items`,
  holding `client.lock_drive()` for the destructive phase (see `fs/copy/backend.rs`).
- Vocabulary: `XConfig`, `XReport`, `XFailed`, `XCallback`. Names must not clash with
  existing `Client` methods.
- Callback trait methods are `on_<event>`; batch variants say so in their name; producer-side
  types never use `on_*` (see `fs/copy/report.rs` and `io/dir_upload.rs`).
- Public types: no single-variant enums, no `Option` that is always `Some`, no positional
  tuples, no request type reused as a response. Do not leak internal generic parameters;
  expose a concrete alias.
- State is an enum, not a set of booleans.
- Every success, failure and cancel path returns a complete report of what was done.

### Validation, arithmetic and untrusted input

- Validate at the boundary and carry the proof in a type (`ValidatedName`). Invalid input
  is rejected, never silently sanitised. FFI entry points validate before the job starts.
- No sentinel values (use `Option`), no `as` casts that can truncate (use `try_from`), no
  unchecked indexing (use `.get()`). `debug_assert!` is not a guard.
- Size limits are exceeded only when `needed > limit`. Use one helper for the comparison and
  test both `max` and `max + 1`.
- Counters and sums over external data use `checked_*` arithmetic, and a loop that
  generates candidate names must provably make progress. Test the `u64::MAX` boundary (see
  `fs/copy/naming.rs` and `cache/state/mod.rs`).
- Bound anything an archive or a server response can amplify: entry count, expanded size,
  path depth. Collision handling stays linear. If a gap is accepted, say so in a plain
  comment at the site (see `io/canonical_path.rs` and `io/dir_download.rs`).
- Hash maps keyed by untrusted data keep the default randomly seeded hasher.

### Errors

- Wrapping an error keeps `server_code()`, `inner_message()`, `kind()` and downcasting
  working through the wrapper; add a test (see `error.rs` and its tests).
- Never rebuild an error from its string (`Error::custom(kind, e.to_string())`); keep the
  source.
- Module errors are typed enums with `thiserror` and `From` impls, reached through the
  `error.rs` downcast helpers. No `anyhow` in `filen-sdk-rs`.
- The same condition maps to the same `ErrorKind` everywhere; invariant violations are
  `ErrorKind::Internal`.
- Never swallow an error: return it, or log it with `warn!` including the cause. No `.ok()?`
  on a listing; when unsure, fail closed.
- No `unwrap()` outside tests. `expect("... (should be impossible)")` is for true
  invariants only. Untrusted input returns `Err` and never panics. No `debug_assert!(false)`.

### FFI and wasm

- Binding types use `#[js_type]`; never hand-write `Serialize`, `Tsify` or uniffi derives
  for them. If the macro needs extending, change `filen-macros` in its own commit.
- Errors cross FFI as `Arc<Error>` on uniffi and `JsValue::from` on wasm; no error DTOs.
- A job's bindings live in one `js_impl.rs` with `uniffi_impl` and `wasm_impl` submodules
  (see `fs/copy/js_impl.rs`).
- Optional JS callbacks are checked with `is_undefined() || is_null()` (see
  `connect/js_impls.rs`), never replaced with `Function::default()`. On uniffi, deliver
  callbacks in order with `spawn_ordered_dispatch`; on wasm, follow `wasm_impl` in
  `fs/copy/js_impl.rs`.
- Gate wasm code on `feature = "wasm-full"` and use the `MaybeSend` aliases. A re-export
  carries the same `cfg` as its users.
- Every native config knob is mirrored in `JsClientConfig` (`auth/http/mod.rs`) in the same
  commit, with the same meaning on every platform.
- Follow the sibling's resolve/reject convention (copy resolves with the report on
  failure). Document any deviation and cover it in `web/main.test.ts`.

### Compatibility and sibling semantics

- Anything shipped (public Rust items, FFI exports, error messages, callback names and
  payloads) is not renamed or reworded without the maintainer's sign-off. If it is
  approved, keep an alias and make the change a standalone commit.
- A new job matches its sibling's semantics (limits, conflict handling, cancellation,
  error states). Document every intended difference at the site.

### Drive lock and concurrency

- Verify-then-act on the drive happens under one `let _lock = client.lock_drive().await?`
  scope, and items are identified by uuid and parent, not by name (see
  `fs/dir/client_impl.rs` and `fs/copy/backend.rs`).
- Never wait without a bound while holding the lock; its lease is about 60 seconds.
- Concurrency limits use `clamp(1, Semaphore::MAX_PERMITS)` (see `auth/http/mod.rs`),
  `consts::MAX_OPEN_FILES`, and a bounded `FuturesUnordered` window.
- Spawn through `crate::runtime`; do not call `tokio::spawn` directly in code that also
  builds for wasm.

### Security and dependencies

- No `zeroize` and no wiping of keys or buffers (maintainer decision). Secret-holding types
  implement a redacting `Debug` (see `crypto/v1.rs`).
- No new `unsafe` (including `unsafe impl` and custom allocators) without the maintainer's
  permission. Existing `unsafe` carries a `// SAFETY:` comment.
- No `vendor/` and no `[patch]`. A new git dependency is a fork under a
  maintainer-controlled account, pinned by `rev` (precedent: `async_zip` from
  `Enduriel/rs-async-zip` in `filen-sdk-rs/Cargo.toml`). Never modify the vendored libheif
  or libde265.
- A new dependency (after approval) is optional behind the feature that needs it (`dep:`),
  is never a second major version of an existing crate, carries a comment on any pin, and
  passes `taplo fmt`.

### Tests

- Tests assert literal expected values and must be able to fail; a bug-fix test fails
  without the fix. No silent skips.
- Shared helpers live in `test-utils` (Rust) or use `vi.waitFor` (browser tests); do not
  re-implement them.
- No real-time sleeps: use `#[tokio::test(start_paused = true)]` (see
  `fs/copy/client_impl.rs`). A flaky test is a bug to fix, never to retry.
- One scenario per test, deterministic inputs (`Uuid::from_u128`, fixed seeds).
- Failure paths are covered through fakes with `fail_*` switches (see
  `fs/copy/engine/tests.rs`).
- Live tests take their account and directory from `test-utils`
  (`get_resources_with_lock` for drive mutations) and never retry on `rate_limited`.
- Unit tests go in an in-file `mod tests` or a sibling `tests.rs`, with names that read as
  sentences.
- Every new wasm export gets a case in `filen-sdk-rs/web/main.test.ts`. Build the bindings
  with `bash wasm-pack.sh` from `filen-sdk-rs/`, then run `npm --prefix filen-sdk-rs/web test`.
- Test inputs are generated inside the test, or pinned by URL and checksum. Committing a
  binary fixture needs the maintainer's approval; an approved interop set lives beside a
  runnable `generate.sh`.
- Test-only code stays out of production: no `cfg(test)` fields or branches in production
  types; use `test_support` modules; a test allocator lives at the crate root under
  `#[cfg(test)]`.

### Docs and comments

- Docs change in the same commit as the behaviour, and never claim more than the code does.
- Public items and fields are documented, with units. Non-obvious choices, including
  deliberate clones, get a comment explaining why.
- Public docs do not link private items:
  `RUSTDOCFLAGS="-D warnings" cargo doc -p filen-sdk-rs --no-deps`.
- No review-tracking markers in code: finding IDs, "audit" notes, `TODO(agent)`.

### Checklist before handing work over

1. Every new helper, constant and type was searched for first; nothing duplicates main or std.
2. Nothing is `pub` unless re-exported, and every item has one export path.
3. A new job's API matches `fs/copy`: `Result<XReport, XFailed>`, `XConfig`, `on_*` callbacks.
4. Every error is returned or logged with its cause; wrappers keep `server_code`/`kind`.
5. No `unwrap` outside tests, no truncating casts or unchecked indexing on external data,
   and limits compare with `>`.
6. Destructive steps sit under one `lock_drive` scope with no unbounded waits.
7. Bindings use `#[js_type]`, `JsClientConfig` is mirrored, and `web/main.test.ts` is
   updated and was run.
8. New tests fail without the change, use no real sleeps, and skip nothing silently.
9. `bash scripts/git-hooks/pre-commit` passes.
10. Docs are updated in the same commit, and nothing shipped was renamed or reworded
    without sign-off.
11. No zeroize, `unsafe`, `vendor/`, `[patch]`, binary file or new dependency went in
    without the maintainer's approval.
