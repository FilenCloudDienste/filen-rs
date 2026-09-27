//! A heap meter for the lib's tests: the peak of live bytes allocated on the current thread
//! while measuring. It counts per thread, so tests running in parallel do not see each other's
//! allocations; it sees only Rust allocations, which every codec the archives use makes.

use std::{
	alloc::{GlobalAlloc, Layout, System},
	cell::Cell,
};

struct Meter;

// const-initialized and without destructors: reading them never allocates, which the
// allocator itself relies on
thread_local! {
	static MEASURING: Cell<bool> = const { Cell::new(false) };
	static LIVE: Cell<isize> = const { Cell::new(0) };
	static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn note(delta: isize) {
	let _ = MEASURING.try_with(|measuring| {
		if measuring.get() {
			let _ = LIVE.try_with(|live| {
				let now = live.get() + delta;
				live.set(now);
				let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
			});
		}
	});
}

// A global allocator can only be written as an `unsafe impl GlobalAlloc`; this one exists only in
// the lib test binary, so a test can bound the heap a codec takes.
// SAFETY: every method hands its arguments to `System` unchanged and returns what `System`
// returns, so the allocator keeps `System`'s guarantees; the bookkeeping never allocates.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Meter {
	unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
		note(layout.size() as isize);
		// SAFETY: the caller's layout, under the contract this method was called with.
		unsafe { System.alloc(layout) }
	}

	unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
		note(layout.size() as isize);
		// SAFETY: the caller's layout, under the contract this method was called with.
		unsafe { System.alloc_zeroed(layout) }
	}

	unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
		note(-(layout.size() as isize));
		// SAFETY: `ptr` was allocated by this allocator, so by `System`, with `layout`.
		unsafe { System.dealloc(ptr, layout) }
	}

	unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
		note(new_size as isize - layout.size() as isize);
		// SAFETY: `ptr` was allocated by `System` with `layout`, and `new_size` meets the
		// contract this method was called with.
		unsafe { System.realloc(ptr, layout, new_size) }
	}
}

#[global_allocator]
static METER: Meter = Meter;

/// What `f` returns, and the most bytes it held allocated on this thread at once.
pub(crate) fn peak_bytes<T>(f: impl FnOnce() -> T) -> (T, u64) {
	LIVE.set(0);
	PEAK.set(0);
	MEASURING.set(true);
	let out = f();
	MEASURING.set(false);
	(out, PEAK.get().max(0) as u64)
}
