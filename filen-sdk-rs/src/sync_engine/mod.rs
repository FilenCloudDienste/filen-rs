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
mod engine;
mod events;
mod guard;
mod ignore;
mod mode;
mod outcome;
mod pause;
mod plan;
// The permanent per-phase cost probe, driven by `tests/sync_engine_probe.rs`. Gated on
// `bench-internals` like `cache::bench_support`, and deliberately not on `cfg(test)`: the `tests/`
// binary links the library compiled WITHOUT `cfg(test)`, so such a seam would be invisible to the
// very file that drives it.
#[cfg(feature = "bench-internals")]
pub mod probe;
mod scan;
mod watch;

pub use self::ignore::{DEFAULT_IGNORE_PATTERNS, IgnoreLevel, IgnoredPath};
pub use apply::SyncReport;
pub use baseline::{PairId, PairRecord};
pub use engine::{
	CONFIRM_TENURE, ConflictResolution, PATH_FAILURE_RETRY_INTERVAL, PairOverlap, SyncEngine,
};
pub use events::{SyncEvent, SyncObserver};
pub use guard::{DeleteGuard, GuardReason};
pub use mode::{Backlog, SyncMode};
pub use outcome::{
	HaltReason, PlanOutcome, PlannedAction, PlannedActionKind, PlannedConflict, PlannedNodeKind,
	RefuseReason, UnsyncablePath, UnsyncableReason,
};
pub use pause::{DEFAULT_CANCEL_AFTER, PauseMode, PauseOptions};
pub use watch::{WatchConfig, WatchHandle, WatchState, WatchStatus};
