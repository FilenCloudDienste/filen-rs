#![allow(dead_code)]
// todo: use this again in the CLI sync command

//! Keeps the memory a long-running command frees from staying with the process on Linux.
//!
//! glibc's malloc gives a thread that finds the arenas busy an arena of its own, up to 8 per core,
//! and keeps what is freed in an arena for that arena's later allocations. A transfer or `sync`
//! allocates on every runtime worker, blocking-pool and rayon thread, so each of them ends up
//! holding freed transfer buffers and pass structures that the OS never gets back. Measured on
//! Linux (glibc 2.43, 10 cores) after a first sync of 1,583 files: 213 MiB of anonymous memory
//! stayed resident with glibc's defaults and 69 MiB with two arenas. mimalloc (217 MiB) and
//! jemalloc (173 MiB) kept it too. With two arenas and a trim after each pass, 38 MiB stayed
//! resident right after the pass and about 50 MiB a minute later.
//!
//! Elsewhere there is nothing to do: macOS's allocator hands freed memory back on its own (43 MiB
//! after the same pass), and musl and Windows have no per-thread arenas.

/// How many malloc arenas glibc may create, in place of its default of 8 per core. With two, a
/// thread that finds both busy waits on one instead of growing a third pool of freed memory.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const MAX_ARENAS: libc::c_int = 2;

/// Caps glibc's malloc arenas at [`MAX_ARENAS`], and answers whether glibc took the setting.
///
/// Call it before the first thread starts: a thread that already has an arena keeps it.
// The one FFI call below, into glibc's malloc tuning.
#[allow(unsafe_code)]
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn limit_arenas() -> bool {
	// SAFETY: mallopt takes two integers and changes nothing but malloc's tuning parameters.
	// M_ARENA_MAX with a positive value is one of the parameters glibc documents.
	unsafe { libc::mallopt(libc::M_ARENA_MAX, MAX_ARENAS) == 1 }
}

/// Without glibc there are no per-thread arenas to cap.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn limit_arenas() -> bool {
	true
}

/// Hands the memory malloc holds free back to the OS.
///
/// Call it once a burst of work has dropped what it held, such as after a sync pass. It walks every
/// arena, so it is not for a hot path.
// The one FFI call below, into glibc's malloc.
#[allow(unsafe_code)]
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn release_free_memory() {
	// SAFETY: malloc_trim takes an integer, the padding to leave at the top of the main heap, and
	// only returns free pages to the OS; no allocation the program holds is moved or freed. Its
	// answer, whether any memory was released, is not an error either way.
	unsafe {
		libc::malloc_trim(0);
	}
}

/// Without glibc the allocator returns freed memory on its own.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn release_free_memory() {}

#[cfg(test)]
mod tests {
	use super::limit_arenas;

	#[test]
	fn the_allocator_takes_the_arena_cap() {
		assert!(limit_arenas());
	}
}
