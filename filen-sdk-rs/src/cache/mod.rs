#![warn(unreachable_pub, unused_qualifications)]

mod error;
mod handle;
// UniFFI-only (the io-module precedent for native-only features): the cache cannot compile to
// wasm yet — deliberately, see lib.rs — so the wasm twin of the FFI layer arrives with the wasm
// port instead of shipping as unbuildable code today.
#[cfg(feature = "uniffi")]
pub mod js_impl;
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
