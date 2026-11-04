//! The room the local volume has for a pass's downloads, measured only on a retry after a pass that
//! found the local disk full (see [`LocalRoom`]).
//!
//! A first attempt measures nothing: it relies on the filesystem's own answer, a write that fails
//! with `StorageFull` (see the apply's `pass_halt`). Only once a pass has halted on that does the
//! next one ask the volume first, so that every backoff retry does not start the held downloads
//! again, write into the last free bytes until the disk refuses, and delete what it wrote.

use std::path::{Path, PathBuf};

/// How the bytes free on the volume holding a path are read: [`free_space`], outside the tests.
pub(in crate::sync_engine) type FreeSpace = fn(&Path) -> std::io::Result<u64>;

/// The bytes this process may still write to the volume holding `path`: the blocks available to an
/// unprivileged writer, not the ones the filesystem keeps back for root.
#[cfg(unix)]
pub(in crate::sync_engine) fn free_space(path: &Path) -> std::io::Result<u64> {
	let stat = nix::sys::statvfs::statvfs(path)?;
	// Both are C type aliases, `u32` or `u64` by platform, so a widening cast either way.
	Ok((stat.blocks_available() as u64).saturating_mul(stat.fragment_size() as u64))
}

/// The free bytes of the volume holding `path`, from the volume whose mount point is the longest
/// prefix of it. sysinfo reads the volume's total free bytes, not a user's quota: over a quota the
/// retry starts its downloads and the disk refuses them, as on the first attempt. Network drives are
/// not listed, so they fall back to starting the downloads.
#[cfg(windows)]
pub(in crate::sync_engine) fn free_space(path: &Path) -> std::io::Result<u64> {
	use sysinfo::{DiskRefreshKind, Disks};

	let disks = Disks::new_with_refreshed_list_specifics(DiskRefreshKind::nothing().with_storage());
	disks
		.list()
		.iter()
		.filter(|disk| path.starts_with(disk.mount_point()))
		.max_by_key(|disk| disk.mount_point().as_os_str().len())
		.map(sysinfo::Disk::available_space)
		.ok_or_else(|| {
			std::io::Error::new(
				std::io::ErrorKind::NotFound,
				format!("no volume is mounted at or above {}", path.display()),
			)
		})
}

/// Neither: nothing to ask, so a retry starts its downloads as the first attempt did.
#[cfg(not(any(unix, windows)))]
pub(in crate::sync_engine) fn free_space(_path: &Path) -> std::io::Result<u64> {
	Err(std::io::ErrorKind::Unsupported.into())
}

/// What a pass knows of the local volume's room for its downloads.
///
/// The transfers of a pass run concurrently (up to the client's concurrency), so the downloads it
/// starts need the SUM of their sizes, not the largest of them. The volume is measured once, before
/// the first download starts, and each download started draws its size from what was measured; one
/// whose size is more than what is left is held instead, and halts the pass for a full local disk as
/// the filesystem's own refusal would have. One that still fits starts: a pass whose largest file
/// does not fit still lands the ones that do.
pub(super) enum LocalRoom {
	/// Not measured: a first attempt, or a measurement that failed. Every download starts, and a
	/// disk that turns out full refuses the write.
	Unmeasured,
	/// To be measured with this before the first download starts.
	Due(FreeSpace),
	/// The bytes left of the measurement after the downloads started so far.
	Left(u64),
}

impl LocalRoom {
	/// The room of a pass: measured with `free_space` on a retry after a full local disk, and
	/// [`Unmeasured`](Self::Unmeasured) on a first attempt (`None`).
	pub(super) fn new(free_space: Option<FreeSpace>) -> Self {
		free_space.map_or(Self::Unmeasured, Self::Due)
	}

	/// Whether a download of `size` bytes into `root` may start, drawing them from the room left.
	/// Measures the volume first if it is due, off the runtime thread (a network volume can take a
	/// while to answer), and falls back to starting it when that fails.
	pub(super) async fn admits(&mut self, root: &Path, size: u64) -> bool {
		if let Self::Due(free_space) = *self {
			let root: PathBuf = root.to_path_buf();
			*self = match tokio::task::spawn_blocking(move || free_space(&root)).await {
				Ok(Ok(free)) => Self::Left(free),
				Ok(Err(error)) => {
					tracing::warn!(
						"apply: the local volume's free space could not be read ({error}); the \
						 held downloads start as they did before"
					);
					Self::Unmeasured
				}
				Err(error) => {
					tracing::warn!(
						"apply: reading the local volume's free space panicked ({error}); the held \
						 downloads start as they did before"
					);
					Self::Unmeasured
				}
			};
		}
		match self {
			Self::Unmeasured | Self::Due(_) => true,
			Self::Left(left) => match left.checked_sub(size) {
				Some(rest) => {
					*left = rest;
					true
				}
				None => false,
			},
		}
	}
}
