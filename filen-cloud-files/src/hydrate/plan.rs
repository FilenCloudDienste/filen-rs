//! Turns a `FETCH_DATA` request into the bytes the core transfers for it: the required range,
//! clamped to the file and aligned the way cfapi accepts, sometimes widened toward the optional
//! range.

use filen_mobile_native_cache::range::ByteRange;
use filen_sdk_rs::consts::CHUNK_SIZE_U64;

use crate::{
	os::{AlignedChunk, RequestedRange},
	placeholder::{FileSize, NegativeSize},
};

/// cfapi's transfer granularity, in bytes: every transfer starts at a multiple of it, and ends at
/// one or at the end of the file.
pub(crate) const TRANSFER_ALIGN: u64 = 4096;

// The cache hands the bytes over in pieces that end on a chunk boundary or at the range end, so a
// chunk boundary must be a transfer boundary too.
const _: () = assert!(CHUNK_SIZE_U64.is_multiple_of(TRANSFER_ALIGN));

/// The bytes one `FETCH_DATA` transfers: `[start, end)`, `start` a multiple of
/// [`TRANSFER_ALIGN`], `end` a multiple of it or the file size, containing the aligned required
/// range, never empty. Only [`plan`] builds one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransferWindow {
	start: u64,
	end: u64,
	file_size: FileSize,
}

/// Why a request has no window to transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PlanError {
	/// The OS gave a negative file size.
	#[error(transparent)]
	Size(#[from] NegativeSize),
	/// The placeholder has no bytes to transfer.
	#[error("the file is empty")]
	EmptyFile,
	/// The required range holds no byte at or after offset 0.
	#[error("{len} bytes at {offset} leave nothing inside the file")]
	NothingRequired { offset: i64, len: i64 },
	/// The required range's end is past `i64::MAX`.
	#[error("{offset} + {len} overflows")]
	Overflow { offset: i64, len: i64 },
	/// The required range starts at or past the end of the file.
	#[error("the request starts at {offset}, at or past EOF {size}")]
	PastEnd { offset: u64, size: u64 },
}

/// Why a piece of bytes cannot be transferred for a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ChunkError {
	/// The piece does not start at a multiple of [`TRANSFER_ALIGN`].
	#[error("offset {offset} is not a multiple of 4096")]
	Misaligned { offset: u64 },
	/// The piece ends neither at a multiple of [`TRANSFER_ALIGN`] nor at the end of the file.
	#[error("a piece ending at {end} is neither aligned nor at EOF")]
	UnalignedTail { end: u64 },
	/// Some of the piece's `len` bytes at `offset` lie outside the window.
	#[error("{len} bytes at {offset} are outside the window")]
	OutsideWindow { offset: u64, len: usize },
	/// The piece holds no byte.
	#[error("the piece is empty")]
	Empty,
}

/// `value` rounded down to a multiple of [`TRANSFER_ALIGN`].
fn align_down(value: u64) -> u64 {
	value - value % TRANSFER_ALIGN
}

/// The window for `range`, under these rules, in order:
///
/// 1. A negative size is refused, and so is an empty file.
/// 2. A required range of no bytes, or one whose end overflows, is refused.
/// 3. A negative required offset is clamped to 0: Windows was measured once asking for
///    `[-4096, +8192)` of a 10,000-byte file when the provider's own process appended to it, and
///    the measured probe clamps (recorded default). A range left with no byte at or after 0 is
///    refused.
/// 4. A range starting at or past the end of the file is refused, and its end clamped to it.
/// 5. The start rounds down to a multiple of [`TRANSFER_ALIGN`], the end up to one or to the end of
///    the file.
/// 6. The end may widen toward the optional range (see [`widened_end`]).
pub(crate) fn plan(range: RequestedRange) -> Result<TransferWindow, PlanError> {
	let file_size = FileSize::from_i64(range.file_size())?;
	let size = file_size.get();
	if size == 0 {
		return Err(PlanError::EmptyFile);
	}

	let (offset, len) = (range.required_offset(), range.required_len());
	if len <= 0 {
		return Err(PlanError::NothingRequired { offset, len });
	}
	let end = offset
		.checked_add(len)
		.ok_or(PlanError::Overflow { offset, len })?;
	if end <= 0 {
		return Err(PlanError::NothingRequired { offset, len });
	}
	// Both are non-negative here, so the absolute value is the value.
	let start = offset.max(0).unsigned_abs();
	let end = end.unsigned_abs();

	if start >= size {
		return Err(PlanError::PastEnd {
			offset: start,
			size,
		});
	}
	let end = end.min(size);

	let window_start = align_down(start);
	let aligned_end = end
		.checked_next_multiple_of(TRANSFER_ALIGN)
		.filter(|aligned| *aligned < size)
		.unwrap_or(size);
	let window_end = widened_end(aligned_end, end, size, range).unwrap_or(aligned_end);

	Ok(TransferWindow {
		start: window_start,
		end: window_end,
		file_size,
	})
}

/// The window's end widened toward the optional range, or `None` when it is not widened
/// (recorded default). It widens only toward an optional range that starts at or before
/// `window_end`, and only up to the end of the cache chunk the required range ends in:
/// the SDK downloads and decrypts that whole chunk anyway, and widening is the provider's choice
/// (measured). It never passes the optional range's end, rounded down to a multiple of
/// [`TRANSFER_ALIGN`] unless that end is the end of the file.
fn widened_end(
	window_end: u64,
	required_end: u64,
	size: u64,
	range: RequestedRange,
) -> Option<u64> {
	let optional = range.optional()?;
	// A negative start is before the window's end too.
	if u64::try_from(optional.offset()).is_ok_and(|start| start > window_end) {
		return None;
	}
	let optional_end = u64::try_from(optional.offset().checked_add(optional.length())?)
		.ok()?
		.min(size);
	let optional_cap = if optional_end == size {
		size
	} else {
		align_down(optional_end)
	};
	let chunk_end = required_end
		.checked_next_multiple_of(CHUNK_SIZE_U64)
		.unwrap_or(size);
	Some(window_end.max(chunk_end.min(optional_cap).min(size)))
}

impl TransferWindow {
	/// The size of the file the window lies in.
	pub(crate) fn file_size(self) -> FileSize {
		self.file_size
	}

	/// What the cache's range read is asked for: `[start, end)`.
	pub(crate) fn byte_range(self) -> ByteRange {
		ByteRange::new(self.start, self.end - self.start)
			.expect("a window is never empty (should be impossible)")
	}

	/// `bytes` at `offset` as a piece cfapi accepts: not empty, starting at a multiple of
	/// [`TRANSFER_ALIGN`], inside the window, and ending at a multiple of it or at the end of the
	/// file.
	pub(crate) fn chunk<'a>(
		&self,
		offset: u64,
		bytes: &'a [u8],
	) -> Result<AlignedChunk<'a>, ChunkError> {
		if bytes.is_empty() {
			return Err(ChunkError::Empty);
		}
		if !offset.is_multiple_of(TRANSFER_ALIGN) {
			return Err(ChunkError::Misaligned { offset });
		}
		let outside = ChunkError::OutsideWindow {
			offset,
			len: bytes.len(),
		};
		if offset < self.start {
			return Err(outside);
		}
		let end = u64::try_from(bytes.len())
			.ok()
			.and_then(|len| offset.checked_add(len))
			.filter(|end| *end <= self.end)
			.ok_or(outside)?;
		if !end.is_multiple_of(TRANSFER_ALIGN) && end != self.file_size.get() {
			return Err(ChunkError::UnalignedTail { end });
		}
		Ok(AlignedChunk::new(offset, bytes))
	}
}
