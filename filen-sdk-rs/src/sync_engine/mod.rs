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
mod mode;
mod plan;
mod scan;
mod watch;

pub use apply::SyncReport;
pub use engine::SyncEngine;
pub use events::{SyncEvent, SyncObserver};
pub use mode::SyncMode;
pub use watch::{WatchHandle, WatchStatus};
