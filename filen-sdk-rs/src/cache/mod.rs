#![warn(unreachable_pub, unused_qualifications)]

mod error;
mod handle;
pub mod search;
mod sql;
mod state;

// `CacheControlMessage` is the INTERNAL worker control protocol — the public API exposes it only
// through `Client::{configure_cache, add_sync_root, flush_cache}` and
// `SyncRootHandle::{evict, update_list_dir_recursive}` + its `Drop`, never as a constructible type.
pub(crate) use handle::CacheSlot;
pub(crate) use state::{CacheControlMessage, CacheState};
pub use {
	error::CacheError,
	handle::{CacheMessage, ResyncProgress, SyncRootHandle},
	search::{
		Search, SearchConfig, SearchItemType, SearchResult, SearchSnapshot, SearchWindowCallback,
		SearchWindowHandle,
	},
	state::{CacheEvent, CacheEventType, DirEvent, FileEvent, GlobalEvent, SyncRootCallback},
};
