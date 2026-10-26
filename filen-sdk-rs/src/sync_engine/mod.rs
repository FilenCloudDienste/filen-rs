//! The sync engine: keeps a local folder in sync with a remote Filen folder.
//!
//! # Architecture
//!
//! `remote snapshot (from the cache) ⊕ local scan ⊕ a persisted 3-way baseline → reconcile →
//! ordered action plan → apply → advance the baseline`. The cache (its
//! [`enumerate`](crate::cache) read path + event stream) supplies the remote half; the
//! [`baseline`] store is the new piece — the last-synced snapshot that lets a two-way reconcile
//! tell a local delete apart from a remote add, and that powers the mtime+size fast-path (only
//! re-hash a local file whose `(mtime, size)` diverged from the baseline).
//!
//! # Modes
//!
//! [`SyncMode`] collapses to two knobs — direction and whether source-side deletions propagate.
//! "Backup" modes are mirrors that never delete on the destination.
//!
//! Native-only: it owns local files, a private read connection to the cache DB, and a `notify` FS
//! watcher, none of which the wasm single-connection VFS supports.

mod apply;
mod baseline;
// The named-scenario cost harness, driven by `tests/sync_engine_bench.rs`. Unlike `probe` below it
// drives `SyncEngine::prepare` itself rather than a hand-assembled copy of it, which is why the
// engine carries `step` marks: they are the real pass's own phase boundaries.
#[cfg(feature = "bench-internals")]
pub mod bench;
mod changes;
// The maps a change-scoped pass reconciles from, carried out of the resident baseline.
mod derive;
mod engine;
mod events;
// The path-keyed facts a pass carries between passes, and the paths the next one owes a look at.
mod facts;
mod guard;
mod ignore;
// The JS bindings' surface: the mobile one (UniFFI) and the desktop one (napi).
#[cfg(any(feature = "uniffi", feature = "napi"))]
mod js_impl;
mod mode;
// The local half of a change-scoped pass: re-observe the paths the changelist names.
mod observe;
mod outcome;
// A page of rows read in path order, and the read-ahead a question about one path reads pages with.
mod page;
mod pause;
mod plan;
// The remote half: apply the announced changes to the derived remote view.
mod remote;
mod rows;
// The permanent per-phase cost probe, driven by `tests/sync_engine_probe.rs`. Gated on
// `bench-internals` like `cache::bench_support`, and deliberately not on `cfg(test)`: the `tests/`
// binary links the library compiled WITHOUT `cfg(test)`, so such a seam would be invisible to the
// very file that drives it.
#[cfg(feature = "bench-internals")]
pub mod probe;
mod scan;
// The two sides a pass reconciles, behind the narrowest access each consumer needs.
mod side;
// A pass's scratch SQLite file: its plan, staged for the apply to read a page at a time.
mod stage;
// The resident baseline tree a pass used to read its rows from: now the oracle the store-backed
// reader (`rows`) is held to in the tests, and a structure the probe still sizes.
#[cfg(any(test, feature = "bench-internals"))]
mod tree;
// The local scan's directory walk.
mod walk;
mod watch;

/// Close the pass step ending here, for the benchmark harness.
///
/// Compiles to nothing without `bench-internals`: a shipping build carries neither the call nor a
/// function to call. That is what lets the engine's own code carry the phase boundaries a benchmark
/// times, instead of a second copy of the pass existing somewhere to drift from this one.
#[cfg(feature = "bench-internals")]
use bench::mark as step;

#[cfg(not(feature = "bench-internals"))]
#[inline(always)]
fn step(_name: &'static str) {}

pub use self::ignore::{DEFAULT_IGNORE_PATTERNS, IgnoreLevel, IgnoredPath};
pub use apply::SyncReport;
pub use baseline::{PairId, PairRecord};
pub use changes::FullPassReason;
pub use engine::{
	BaselineInUse, BaselineSchemaMismatch, CONFIRM_TENURE, ConflictResolution,
	PATH_FAILURE_RETRY_INTERVAL, PairOverlap, SyncEngine,
};
pub use events::{SyncEvent, SyncObserver, TransferDirection};
pub use guard::{DeleteGuard, GuardReason};
#[cfg(feature = "uniffi")]
pub use js_impl::{
	JsSyncEngine, JsSyncPlan, JsSyncWatch, SyncEngineListener, SyncEventMessage, SyncPairInfo,
	SyncPassEvent, SyncPassReport, SyncRefusal,
};
#[cfg(feature = "napi")]
pub(crate) use js_impl::{JsSyncEngine, JsSyncPlan, JsSyncWatch, SyncRefusal, sync_refusal};
pub use mode::{Backlog, SyncMode};
pub use outcome::{
	HaltReason, PlanOutcome, PlannedAction, PlannedActionKind, PlannedConflict, PlannedNodeKind,
	RefuseReason, UnsyncablePath, UnsyncableReason,
};
pub use pause::{DEFAULT_CANCEL_AFTER, PauseMode, PauseOptions};
pub use watch::{WatchConfig, WatchHandle, WatchState, WatchStatus};
