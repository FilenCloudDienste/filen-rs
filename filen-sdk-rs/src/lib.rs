#![allow(dead_code)]

pub(crate) mod api;
pub mod auth;
pub mod chats;
pub mod connect;
pub mod consts;
pub mod crypto;
pub mod error;
pub mod fs;
pub mod io;
#[cfg(any(
	all(target_family = "wasm", target_os = "unknown",),
	feature = "uniffi"
))]
pub mod js;
pub mod notes;
pub mod runtime;
pub mod search;
pub(crate) mod serde;
#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
pub mod socket;
pub mod sync;
pub mod thumbnail;
pub mod user;
pub mod util;

pub use error::{Error, ErrorKind};

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

struct TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
	unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
		ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
		unsafe { System.alloc(layout) }
	}

	unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
		ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
		unsafe { System.dealloc(ptr, layout) }
	}
}

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator;

pub fn current_allocation() -> usize {
	ALLOCATED.load(Ordering::Relaxed)
}
