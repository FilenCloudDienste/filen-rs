//! Reading a byte range of a remote file the cache knows, for a Rust caller that serves partial
//! reads itself (a files-on-demand provider hydrating a placeholder) instead of downloading the
//! whole file into the cache first.
//!
//! Rust-only: nothing here is exported over UniFFI. The bytes come from the SDK's ranged
//! [`FileReader`](filen_sdk_rs::fs::file::read::FileReader), which downloads and decrypts only the
//! chunks the range touches, and the read is cancelled through the crate's
//! [`FfiAbortSignal`].

use std::{num::NonZeroU64, sync::Arc};

use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasUUID,
		file::{
			RemoteFile,
			client_impl::FileReaderSharedClientExt,
			traits::{HasFileInfo, HasRemoteFileInfo},
		},
	},
};
use filen_types::fs::Uuid;
use futures::io::AsyncBufReadExt;
use tracing::debug;

use crate::{
	CacheError,
	abort::{FfiAbortSignal, check_not_aborted, with_abort},
	auth::FilenMobileCacheState,
	ffi::FfiId,
};

/// A non-empty range of bytes, `[offset, offset + len)`, whose end fits in a `u64`.
///
/// The only way to build one is [`ByteRange::new`], so a zero-length range or one whose end
/// overflows cannot reach a read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
	offset: u64,
	len: NonZeroU64,
	// `offset + len`, checked once in `new`
	end: u64,
}

/// Why a [`ByteRange`] could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ByteRangeError {
	/// The range has no bytes, so a read of it would deliver nothing.
	#[error("a range of zero bytes at offset {offset} covers nothing")]
	Empty { offset: u64 },
	/// `offset + len` is past `u64::MAX`.
	#[error("a range of {len} bytes at offset {offset} ends past u64::MAX")]
	Overflow { offset: u64, len: u64 },
}

impl ByteRange {
	/// The `len` bytes starting at byte `offset`.
	pub fn new(offset: u64, len: u64) -> Result<Self, ByteRangeError> {
		let len = NonZeroU64::new(len).ok_or(ByteRangeError::Empty { offset })?;
		let end = offset
			.checked_add(len.get())
			.ok_or(ByteRangeError::Overflow {
				offset,
				len: len.get(),
			})?;
		Ok(Self { offset, len, end })
	}

	/// The first byte of the range.
	pub fn offset(self) -> u64 {
		self.offset
	}

	/// How many bytes the range covers.
	pub fn len(self) -> NonZeroU64 {
		self.len
	}

	/// The byte just past the range: exclusive.
	pub fn end(self) -> u64 {
		self.end
	}

	/// The part of the range inside a file of `size` bytes: its end moved back to `size` when it
	/// lies past it. `None` when the range starts at or past `size`, as nothing of it is left.
	pub fn clamp_to(self, size: u64) -> Option<Self> {
		if self.offset >= size {
			return None;
		}
		let end = self.end.min(size);
		// `end > offset`, as both `self.end` and `size` are past `offset`
		let len = NonZeroU64::new(end - self.offset)?;
		Some(Self {
			offset: self.offset,
			len,
			end,
		})
	}
}

/// What a range read checks the bytes it delivers against, beyond decrypting them.
///
/// Each chunk is decrypted with the file's key whatever this says. For a file encrypted with a v2
/// or v3 key that is AES-256-GCM, which authenticates the chunk's own bytes; a v1 file is AES-CBC,
/// which authenticates nothing. Neither proves that a chunk sits at its index, nor that the file
/// has no missing or extra chunks: only the file's declared BLAKE3 hash does that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeIntegrity {
	/// The range is the whole file and the file's metadata declares a BLAKE3 hash. The SDK's
	/// reader hashes the bytes in order and fails the read with
	/// [`CacheError::ContentHashMismatch`] when they do not match, with the chunk that completes
	/// the file and before that chunk's bytes reach the sink. The bytes of every earlier chunk have
	/// reached the sink by then, so a sink must not treat them as final before the read returns
	/// `Ok`.
	HashChecked,
	/// Not checked against the declared hash: the range is not the whole file, whose hash covers
	/// only all of its bytes, or the file declares none.
	Unchecked,
}

impl RangeIntegrity {
	/// What a read of `range` (already clamped to the file) of a `size`-byte file checks: the SDK's
	/// reader hashes a read that starts at 0 and ends at the file's size, of a file declaring a
	/// hash.
	fn of(range: ByteRange, size: u64, declares_hash: bool) -> Self {
		if declares_hash && range.offset() == 0 && range.end() == size {
			Self::HashChecked
		} else {
			Self::Unchecked
		}
	}
}

/// What a range read covers. The sink gets it before any byte, and the read returns it once
/// every byte of [`RangeRead::range`] has reached the sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeRead {
	/// The uuid of the file version read: the version the cache holds for the id when the read
	/// starts. A file's uuid changes with every content edit, so a caller serving bytes for a
	/// version it recorded earlier compares it in [`RangeSink::on_start`] and refuses a different
	/// one there, before any byte is delivered.
	pub uuid: Uuid,
	/// The file's size in bytes, as the cache holds it.
	pub file_size: u64,
	/// The bytes read: the requested range clamped to [`RangeRead::file_size`].
	pub range: ByteRange,
	/// What the bytes are checked against.
	pub integrity: RangeIntegrity,
}

/// Where a range read delivers its bytes.
pub trait RangeSink: Send {
	/// Why the sink refused the read or its bytes, handed back to the caller in
	/// [`RangeReadError::Sink`].
	type Error: Send;

	/// Called once, before anything is downloaded, with what the read will cover. An error here
	/// ends the read with nothing delivered.
	fn on_start(&mut self, read: &RangeRead) -> Result<(), Self::Error>;

	/// The decrypted bytes starting at file offset `offset`. Calls come in order, without gaps
	/// or overlaps, from [`RangeRead::range`]'s start to its end. Each holds at most one of the
	/// SDK's chunks ([`CHUNK_SIZE_U64`](filen_sdk_rs::consts::CHUNK_SIZE_U64) bytes) and ends at a
	/// chunk boundary or at the range's end, so a range starting on a 4096-byte boundary gets
	/// every call but the last on one too.
	///
	/// An aborted read drops the returned future wherever it is. The read holds no lock of the
	/// cache's while it waits on the future, so the sink may call back into the cache.
	fn on_bytes(
		&mut self,
		offset: u64,
		bytes: &[u8],
	) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Why a range read failed. Whatever reached the sink before the failure is not the range: a read
/// is complete only when it returns `Ok`.
#[derive(Debug, thiserror::Error)]
pub enum RangeReadError<E> {
	/// The range starts at or past the end of the file, so nothing of it exists.
	#[error("a range starting at {offset} is past the end of file {uuid} ({size} bytes)")]
	PastEnd { uuid: Uuid, offset: u64, size: u64 },
	/// The file's chunks ran out at `reached`, before the range's end: the server holds fewer
	/// bytes than the file's size says.
	#[error("file {uuid} ended at byte {reached}, before the range's end at {end}")]
	Truncated { uuid: Uuid, reached: u64, end: u64 },
	/// The sink refused the read or its bytes.
	#[error("the sink refused the range: {0}")]
	Sink(E),
	/// Resolving the id or reading the bytes failed, or the read was aborted
	/// ([`CacheError::Aborted`]).
	#[error("{0}")]
	Cache(CacheError),
}

impl<E> From<CacheError> for RangeReadError<E> {
	fn from(err: CacheError) -> Self {
		Self::Cache(err)
	}
}

impl FilenMobileCacheState {
	/// Reads the bytes of `range` of the file `id` names, handing them to `sink` in order as they
	/// are downloaded and decrypted, without downloading the rest of the file or touching its
	/// cache slot.
	///
	/// The id is resolved from the cache alone, with no server round trip: a `stable/<id>` names
	/// the file's row, and so its current version, and any other id is walked as a location. A
	/// caller that needs the server's latest version refreshes the item first, with
	/// [`FilenMobileCacheState::update_and_query_item`]. A range reaching past the end of the file
	/// is clamped to it, and
	/// [`RangeRead::range`] says what was read; one starting at or past the end fails with
	/// [`RangeReadError::PastEnd`]. See [`RangeIntegrity`] for what the bytes are checked against.
	///
	/// The cache state is locked only while the id is resolved. The bytes are read on the client
	/// the state held then, with no lock of the cache's held, so the sink may call back into the
	/// cache, and an auth.json rewritten during a long read is adopted without waiting for it. A
	/// read started before a sign-out or an account switch goes on with the session it started
	/// with until it ends or is aborted.
	///
	/// `abort` cancels the read: an abort before the call starts fails it before anything is
	/// resolved, and one during it drops the download and the sink's pending
	/// [`RangeSink::on_bytes`], failing with [`CacheError::Aborted`]. A read that has delivered its
	/// last byte when the abort arrives returns `Ok`.
	pub async fn read_file_range<S: RangeSink>(
		&self,
		id: &FfiId,
		range: ByteRange,
		sink: &mut S,
		abort: Option<&Arc<FfiAbortSignal>>,
	) -> Result<RangeRead, RangeReadError<S::Error>> {
		check_not_aborted(abort)?;
		// The state's read guard goes when the closure returns. Held across the transfer, it
		// would keep the auth re-check's writer waiting for the whole read, and on the fair lock
		// every read queued behind that writer too, a sink calling back into the cache included.
		let (client, file) = self
			.async_execute_authed_owned(async |auth_state| {
				let file = RemoteFile::try_from(auth_state.select_file_by_id(id)?)?;
				Ok((Arc::clone(&auth_state.client), file))
			})
			.await?;
		read_range(&client, &file, range, sink, abort).await
	}
}

/// Reads `range` of `file` into `sink` (see [`FilenMobileCacheState::read_file_range`]).
async fn read_range<S: RangeSink>(
	client: &Client,
	file: &RemoteFile,
	range: ByteRange,
	sink: &mut S,
	abort: Option<&Arc<FfiAbortSignal>>,
) -> Result<RangeRead, RangeReadError<S::Error>> {
	let uuid = file.uuid();
	let size = file.size();
	let range = range.clamp_to(size).ok_or(RangeReadError::PastEnd {
		uuid,
		offset: range.offset(),
		size,
	})?;
	let read = RangeRead {
		uuid,
		file_size: size,
		range,
		integrity: RangeIntegrity::of(range, size, file.hash().is_some()),
	};
	debug!(
		"Reading bytes {}..{} of file {uuid} ({size} bytes)",
		range.offset(),
		range.end()
	);
	sink.on_start(&read).map_err(RangeReadError::Sink)?;
	// Dropping the delivery mid-flight is safe: it writes nothing of the cache's, only to the
	// sink, and an aborted read returns no `RangeRead` to present its bytes as the range.
	let reached = with_abort(abort, async {
		Ok::<_, CacheError>(deliver(client, file, range, sink).await)
	})
	.await??;
	if reached < range.end() {
		return Err(RangeReadError::Truncated {
			uuid,
			reached,
			end: range.end(),
		});
	}
	Ok(read)
}

/// Hands the bytes of `range` of `file` to `sink`, one decrypted chunk at a time, and returns the
/// offset it reached: `range.end()`, unless the file's chunks ran out first.
async fn deliver<S: RangeSink>(
	client: &Client,
	file: &RemoteFile,
	range: ByteRange,
	sink: &mut S,
) -> Result<u64, RangeReadError<S::Error>> {
	let mut reader = client.get_file_reader_for_range(file, range.offset(), range.end());
	let mut offset = range.offset();
	while offset < range.end() {
		// Each fill hands out the unread part of one decrypted chunk, in place, ending at the
		// chunk's end or the range's.
		let bytes = reader.fill_buf().await.map_err(CacheError::from)?;
		if bytes.is_empty() {
			break;
		}
		// The reader stops at the range's end; bounded here as well so `offset` never passes it.
		let wanted = usize::try_from(range.end() - offset).unwrap_or(usize::MAX);
		let bytes = bytes.get(..wanted).unwrap_or(bytes);
		let len = bytes.len();
		sink.on_bytes(offset, bytes)
			.await
			.map_err(RangeReadError::Sink)?;
		reader.consume_unpin(len);
		// at most `range.end() - offset`, by the bound above
		offset += u64::try_from(len).unwrap_or(u64::MAX);
	}
	Ok(offset)
}

#[cfg(test)]
mod tests {
	use std::{convert::Infallible, future::ready};

	use filen_sdk_rs::consts::CHUNK_SIZE_U64;

	use super::*;

	/// A file of two whole chunks and part of a third, so EOF is not on a chunk boundary.
	const SIZE: u64 = 2 * CHUNK_SIZE_U64 + CHUNK_SIZE_U64 / 2 + 3;

	fn range(offset: u64, len: u64) -> ByteRange {
		ByteRange::new(offset, len).unwrap()
	}

	fn bounds(range: ByteRange) -> (u64, u64, u64) {
		(range.offset(), range.len().get(), range.end())
	}

	#[test]
	fn a_range_ends_at_its_offset_plus_its_length() {
		for (offset, len) in [
			(0, 1),
			(1, 1),
			(CHUNK_SIZE_U64 - 1, 1),
			(CHUNK_SIZE_U64, CHUNK_SIZE_U64),
			(CHUNK_SIZE_U64 + 1, CHUNK_SIZE_U64 - 1),
			(0, u64::MAX),
			(u64::MAX - 1, 1),
		] {
			assert_eq!(bounds(range(offset, len)), (offset, len, offset + len));
		}
	}

	#[test]
	fn a_zero_length_range_cannot_be_built() {
		for offset in [0, 1, CHUNK_SIZE_U64, u64::MAX] {
			assert_eq!(
				ByteRange::new(offset, 0),
				Err(ByteRangeError::Empty { offset })
			);
		}
	}

	#[test]
	fn a_range_ending_past_u64_max_cannot_be_built() {
		assert_eq!(
			ByteRange::new(u64::MAX, 1),
			Err(ByteRangeError::Overflow {
				offset: u64::MAX,
				len: 1
			})
		);
		assert_eq!(
			ByteRange::new(1, u64::MAX),
			Err(ByteRangeError::Overflow {
				offset: 1,
				len: u64::MAX
			})
		);
		assert_eq!(
			ByteRange::new(CHUNK_SIZE_U64, u64::MAX - CHUNK_SIZE_U64 + 1),
			Err(ByteRangeError::Overflow {
				offset: CHUNK_SIZE_U64,
				len: u64::MAX - CHUNK_SIZE_U64 + 1
			})
		);
	}

	#[test]
	fn a_range_inside_the_file_is_kept_as_it_is() {
		for (offset, len) in [
			(0, 1),
			(1, 1),
			(CHUNK_SIZE_U64 - 1, 2),
			(CHUNK_SIZE_U64, CHUNK_SIZE_U64),
			(CHUNK_SIZE_U64 + 1, 4096),
			(SIZE - 1, 1),
			(0, SIZE),
		] {
			assert_eq!(range(offset, len).clamp_to(SIZE), Some(range(offset, len)));
		}
	}

	#[test]
	fn a_range_reaching_past_eof_is_clamped_to_it() {
		assert_eq!(
			bounds(range(SIZE - 1, 2).clamp_to(SIZE).unwrap()),
			(SIZE - 1, 1, SIZE)
		);
		assert_eq!(
			bounds(range(0, SIZE + 1).clamp_to(SIZE).unwrap()),
			(0, SIZE, SIZE)
		);
		assert_eq!(
			bounds(
				range(CHUNK_SIZE_U64, u64::MAX - CHUNK_SIZE_U64)
					.clamp_to(SIZE)
					.unwrap()
			),
			(CHUNK_SIZE_U64, SIZE - CHUNK_SIZE_U64, SIZE)
		);
	}

	#[test]
	fn a_range_starting_at_or_past_eof_has_nothing_left() {
		for offset in [SIZE, SIZE + 1, u64::MAX - 1] {
			assert_eq!(range(offset, 1).clamp_to(SIZE), None);
		}
		assert_eq!(range(0, 1).clamp_to(0), None);
	}

	/// A sink that keeps nothing, for checks that never run a read.
	struct Discard;

	impl RangeSink for Discard {
		type Error = Infallible;

		fn on_start(&mut self, _: &RangeRead) -> Result<(), Self::Error> {
			Ok(())
		}

		fn on_bytes(
			&mut self,
			_: u64,
			_: &[u8],
		) -> impl Future<Output = Result<(), Self::Error>> + Send {
			ready(Ok(()))
		}
	}

	fn assert_send<T: Send>(_: &T) {}

	/// Builds a range read's future without polling it. Generic over the sink, so it compiles
	/// only if the future is `Send` for every [`RangeSink`], not just one.
	fn range_read_is_send<S: RangeSink>(cache: &FilenMobileCacheState, id: &FfiId, sink: &mut S) {
		assert_send(&cache.read_file_range(id, range(0, 1), sink, None));
	}

	/// A files-on-demand provider spawns range reads on a multi-threaded runtime, which needs the
	/// read's future to be `Send`. The check is that this compiles; nothing is read.
	#[test]
	fn a_range_read_can_be_spawned_onto_another_thread() {
		let _: fn(&FilenMobileCacheState, &FfiId, &mut Discard) = range_read_is_send;
	}

	#[test]
	fn only_a_whole_file_read_of_a_hashed_file_is_hash_checked() {
		let whole = range(0, SIZE);
		assert_eq!(
			RangeIntegrity::of(whole, SIZE, true),
			RangeIntegrity::HashChecked
		);
		assert_eq!(
			RangeIntegrity::of(whole, SIZE, false),
			RangeIntegrity::Unchecked
		);
		for partial in [range(0, SIZE - 1), range(1, SIZE - 1), range(SIZE - 1, 1)] {
			assert_eq!(
				RangeIntegrity::of(partial, SIZE, true),
				RangeIntegrity::Unchecked
			);
		}
	}
}
