//! Copying what is on disk of a file placeholder not all on disk, without asking the provider for
//! the rest: exactly the ranges the platform reports on disk are read, each whole, and every other
//! byte of the copy is left zero. A read that fails or comes back short ends the copy, so no byte
//! reported on disk is ever written as zero or made up; nothing outside those ranges is read, so
//! no read asks the provider for anything.

use crate::os::{ByteRange, LocalError};

/// How many bytes are read at a time, at most.
pub(crate) const WINDOW: u64 = 1 << 20;

/// Why the copy ended without being whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum CopyFailure {
	/// The stop was asked for.
	#[error("the copy was asked to stop")]
	Stopped,
	/// Reading the file or writing the copy failed, or a read came back short.
	#[error(transparent)]
	Local(#[from] LocalError),
	/// The ranges reported on disk add up to another count than the platform says is on disk: the
	/// file changed between the two reads.
	#[error("the ranges on disk hold {copied} bytes where {on_disk} are on disk")]
	Mismatch { copied: u64, on_disk: u64 },
	/// The ranges are not in order within the file, or are not the ones on disk any more.
	#[error("the ranges on disk are not the ones the file has")]
	Ranges,
	/// The copy does not read back as it was written.
	#[error("the copy does not read back as written")]
	ReadBack,
}

/// What [`copy_ranges`] copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Copied {
	/// How many bytes were on disk and copied.
	pub(crate) on_disk: u64,
	/// The ranges not on disk, zeros in the copy, in order, none adjacent to another.
	pub(crate) zeroed: Vec<ByteRange>,
	/// The BLAKE3 hash of the bytes copied, in order.
	pub(crate) hash: [u8; 32],
}

/// The bytes `ranges` hold, if they are in order, none empty or overlapping another, all within a
/// file of `size` bytes.
fn ranges_total(size: u64, ranges: &[ByteRange]) -> Option<u64> {
	let mut end = 0_u64;
	let mut total = 0_u64;
	for range in ranges {
		if range.len == 0 || range.offset < end {
			return None;
		}
		end = range.offset.checked_add(range.len)?;
		if end > size {
			return None;
		}
		total = total.checked_add(range.len)?;
	}
	Some(total)
}

/// The ranges of a file of `size` bytes that `ranges`, in order and disjoint, leave out.
pub(crate) fn zeroed_ranges(size: u64, ranges: &[ByteRange]) -> Vec<ByteRange> {
	let mut zeroed: Vec<ByteRange> = Vec::new();
	let mut at = 0_u64;
	for range in ranges {
		if range.offset > at {
			zeroed.push(ByteRange {
				offset: at,
				len: range.offset - at,
			});
		}
		at = range.offset.saturating_add(range.len);
	}
	if size > at {
		zeroed.push(ByteRange {
			offset: at,
			len: size - at,
		});
	}
	zeroed
}

/// Calls `piece` with each window of at most [`WINDOW`] bytes of `ranges`, in order, and a buffer
/// that long.
fn each_window<E>(
	ranges: &[ByteRange],
	mut piece: impl FnMut(u64, &mut [u8]) -> Result<(), E>,
	too_long: impl Fn() -> E,
) -> Result<(), E> {
	let longest = ranges
		.iter()
		.map(|range| range.len.min(WINDOW))
		.max()
		.unwrap_or(0);
	let mut buffer = vec![0; usize::try_from(longest).map_err(|_| too_long())?];
	for range in ranges {
		let end = range.offset.saturating_add(range.len);
		let mut offset = range.offset;
		while offset < end {
			let len = WINDOW.min(end - offset);
			let window = usize::try_from(len)
				.ok()
				.and_then(|len| buffer.get_mut(..len))
				.ok_or_else(&too_long)?;
			piece(offset, window)?;
			offset += len;
		}
	}
	Ok(())
}

/// Copies the bytes `ranges` hold of a file of `size` bytes, reported on disk, of which the
/// platform says `on_disk` are: `read` fills its buffer from the offset given, whole, or fails;
/// `write` writes the bytes at the offset given to the copy, whose other ranges are left as zeros
/// (the caller sets its length). The ranges are checked before anything is read: in order within
/// the file, and adding up to `on_disk`. Checks the stop before each [`WINDOW`].
pub(crate) fn copy_ranges(
	size: u64,
	ranges: &[ByteRange],
	on_disk: u64,
	mut read: impl FnMut(u64, &mut [u8]) -> Result<(), LocalError>,
	mut write: impl FnMut(u64, &[u8]) -> Result<(), LocalError>,
	stop: &dyn Fn() -> bool,
) -> Result<Copied, CopyFailure> {
	let copied = ranges_total(size, ranges).ok_or(CopyFailure::Ranges)?;
	if copied != on_disk {
		return Err(CopyFailure::Mismatch { copied, on_disk });
	}
	let mut hasher = blake3::Hasher::new();
	each_window(
		ranges,
		|offset, buffer| {
			if stop() {
				return Err(CopyFailure::Stopped);
			}
			read(offset, buffer)?;
			write(offset, buffer)?;
			hasher.update(buffer);
			Ok(())
		},
		|| CopyFailure::Ranges,
	)?;
	Ok(Copied {
		on_disk: copied,
		zeroed: zeroed_ranges(size, ranges),
		hash: *hasher.finalize().as_bytes(),
	})
}

/// Checks the copy reads back, through `read`, over `ranges`, as the bytes `copied` hashed.
pub(crate) fn check_read_back(
	ranges: &[ByteRange],
	copied: &Copied,
	mut read: impl FnMut(u64, &mut [u8]) -> Result<(), LocalError>,
) -> Result<(), CopyFailure> {
	let mut hasher = blake3::Hasher::new();
	each_window(
		ranges,
		|offset, buffer| {
			read(offset, buffer)?;
			hasher.update(buffer);
			Ok(())
		},
		|| CopyFailure::ReadBack,
	)?;
	if *hasher.finalize().as_bytes() == copied.hash {
		Ok(())
	} else {
		Err(CopyFailure::ReadBack)
	}
}

#[cfg(test)]
mod tests;
