#![warn(unreachable_pub, unused_qualifications)]

// Apply-path surface for the criterion insertion benchmark (`benches/cache_insertion.rs`); gated so
// it never widens the real API.
#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod bench_support;
mod error;
mod handle;
// UniFFI exports on mobile, wasm-bindgen twins on web. The twins share method names, which is
// only sound because the two never compile together (`uniffi` is a native-only dependency).
#[cfg(any(feature = "uniffi", all(target_family = "wasm", target_os = "unknown")))]
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
