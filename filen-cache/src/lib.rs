#![warn(unreachable_pub, unused_qualifications)]

mod error;
mod handle;
mod sql;
mod state;

// `CacheControlMessage` is the INTERNAL worker control protocol — the public API exposes it only
// through `CacheHandle::{add_sync_root, remove_sync_root, shutdown}`, never as a constructible type.
pub(crate) use state::{CacheControlMessage, CacheState};
pub use {
	error::CacheError,
	handle::{CacheHandle, CacheMessage},
	state::{CacheEvent, CacheEventType, DirEvent, FileEvent, GlobalEvent, SyncRootCallback},
};
