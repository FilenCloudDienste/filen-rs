use std::{
	io::{Cursor, Read},
	num::NonZeroU32,
};

use futures::{StreamExt, stream::FuturesOrdered};

use crate::{
	api,
	auth::unauth::UnauthClient,
	consts::{CHUNK_SIZE_U64, FILE_CHUNK_SIZE, FILE_CHUNK_SIZE_EXTRA},
	crypto::shared::DataCrypter,
	error::{Error, ErrorKind, MetadataWasNotDecryptedError},
	util::{MaybeSendBoxFuture, MaybeSendCallback},
};

use super::{chunk::Chunk, traits::File};

pub struct FileReader<'a> {
	file: &'a dyn File,
	client: &'a UnauthClient,
	index: u64,
	limit: u64,
	next_chunk_idx: u64,
	curr_chunk: Option<Cursor<Chunk<'a>>>,
	futures: FuturesOrdered<MaybeSendBoxFuture<'a, Result<Cursor<Chunk<'a>>, Error>>>,
	allocate_chunk_future: Option<MaybeSendBoxFuture<'a, Chunk<'a>>>,
	max_buffer_size: u64,
	// Reports downloaded bytes as each chunk STREAMS in (delta-converted), not at read-out: chunks
	// download concurrently, and the ordered reader would otherwise hold completed chunks behind a
	// slow head-of-line chunk and release them in a burst. See `push_fetch_next_chunk`.
	progress: Option<MaybeSendCallback<'a, u64>>,
}

pub struct FileReaderBuilder<'a> {
	client: &'a UnauthClient,
	file: &'a dyn File,
	start: Option<u64>,
	end: Option<u64>,
	max_buffer_size: Option<u64>,
	progress: Option<MaybeSendCallback<'a, u64>>,
}

impl<'a> FileReaderBuilder<'a> {
	pub fn new(client: &'a UnauthClient, file: &'a dyn File) -> FileReaderBuilder<'a> {
		FileReaderBuilder {
			client,
			file,
			start: None,
			end: None,
			max_buffer_size: None,
			progress: None,
		}
	}

	/// Sets a callback fired with each chunk's plaintext byte count as it finishes downloading.
	pub fn with_progress_callback(mut self, progress: Option<MaybeSendCallback<'a, u64>>) -> Self {
		self.progress = progress;
		self
	}

	pub fn with_start(mut self, start: u64) -> Self {
		self.start = Some(start);
		self
	}

	pub fn with_end(mut self, end: u64) -> Self {
		self.end = Some(end);
		self
	}

	pub fn with_max_buffer_size(mut self, max_buffer_size: u64) -> Self {
		self.max_buffer_size = Some(max_buffer_size);
		self
	}

	/// Builds the reader. If the file's advertised chunk count cannot describe its advertised
	/// size, the returned reader yields an error on read instead of attempting the download.
	pub fn build(self) -> FileReader<'a> {
		let size = self.file.size();
		let limit = self.end.unwrap_or(size).min(size);
		let index = self.start.unwrap_or(0).min(limit);
		let chunks_consistent = chunks_consistent_with_size(self.file.chunks(), size);
		let mut new = FileReader {
			file: self.file,
			client: self.client,
			index,
			limit,
			curr_chunk: None,
			futures: FuturesOrdered::new(),
			next_chunk_idx: index / CHUNK_SIZE_U64,
			allocate_chunk_future: None,
			max_buffer_size: self.max_buffer_size.unwrap_or(size),
			progress: self.progress,
		};

		if chunks_consistent {
			// allocate memory and prefetch chunks
			while let Some(chunk) = new.try_allocate_next_chunk() {
				new.push_fetch_next_chunk(chunk);
			}
			new.allocate_chunk_future = new.allocate_next_chunk();
		}

		new
	}
}

/// Downloads chunk `chunk_idx` of `file` and decrypts it in place, charged to the memory
/// reservation `out_data` holds (its buffer is not reused; the permits move to the result).
///
/// `progress` receives plaintext byte deltas while the chunk streams in, clamped to the chunk's
/// plaintext length so the encryption overhead is never counted.
async fn fetch_decrypted_chunk<'a>(
	client: &UnauthClient,
	file: &dyn File,
	chunk_idx: u64,
	out_data: Chunk<'a>,
	progress: Option<MaybeSendCallback<'_, u64>>,
) -> Result<Chunk<'a>, Error> {
	let (_, permits) = out_data.into_parts();
	let data = fetch_decrypted_chunk_data(client, file, chunk_idx, progress).await?;
	Ok(Chunk::from_parts(data, permits))
}

/// [`fetch_decrypted_chunk`] for a caller that accounts for the chunk's memory itself.
pub(crate) async fn fetch_decrypted_chunk_data(
	client: &UnauthClient,
	file: &dyn File,
	chunk_idx: u64,
	progress: Option<MaybeSendCallback<'_, u64>>,
) -> Result<Vec<u8>, Error> {
	let plaintext_len = chunk_plaintext_len(file.size(), chunk_idx);
	// Report bytes as the chunk streams in (clamped, converted to deltas) instead of only
	// at completion — otherwise a heavily-parallel download shows nothing for seconds while
	// every in-flight chunk fills together, then jumps.
	// High-water mark of bytes already reported for this chunk. A mid-body retry restarts
	// `bytes_so_far` at 0, so we keep the max (not the latest) — `fetch_max` never lowers
	// it — and only forward genuine forward progress, otherwise a retried chunk would
	// re-report the bytes of every failed attempt.
	let reported = std::sync::atomic::AtomicU64::new(0);
	let on_bytes = |bytes_so_far: u64, _content_length: Option<u64>| {
		if let Some(progress) = &progress {
			let clamped = bytes_so_far.min(plaintext_len);
			let prev = reported.fetch_max(clamped, std::sync::atomic::Ordering::Relaxed);
			if clamped > prev {
				progress(clamped - prev);
			}
		}
	};
	let mut data =
		api::download::download_file_chunk(client, file, chunk_idx, Some(&on_bytes)).await?;
	file.key()
		.ok_or(MetadataWasNotDecryptedError)?
		.decrypt_data(&mut data)
		.await?;
	Ok(data)
}

/// Whether a file's advertised chunk count can be produced from its advertised size: the last
/// chunk must not start past the end of the file and its plaintext must fit within one chunk.
/// Remote metadata violating this would drive the chunk-size math out of range, so such
/// readers refuse to read. A chunk count of zero is only consistent with a zero size (legacy
/// empty files); with a nonzero size it would silently read as truncated-to-empty.
pub(crate) fn chunks_consistent_with_size(chunks: u64, size: u64) -> bool {
	match chunks.checked_sub(1) {
		None => size == 0,
		Some(last_chunk_idx) => last_chunk_idx
			.checked_mul(CHUNK_SIZE_U64)
			.and_then(|last_chunk_start| size.checked_sub(last_chunk_start))
			.is_some_and(|last_chunk_len| last_chunk_len <= CHUNK_SIZE_U64),
	}
}

/// [`chunks_consistent_with_size`], as the error to report for a file that fails it.
pub(crate) fn check_chunks_consistent(chunks: u64, size: u64) -> Result<(), Error> {
	if chunks_consistent_with_size(chunks, size) {
		return Ok(());
	}
	Err(Error::custom(
		ErrorKind::Response,
		format!("file chunk count ({chunks}) is inconsistent with file size ({size})"),
	))
}

/// Bytes chunk `chunk_idx` of `file` takes to download (its plaintext plus the AES tag and
/// nonce), or `None` past its last chunk.
fn encrypted_chunk_size(file: &dyn File, chunk_idx: u64) -> Option<NonZeroU32> {
	let last_chunk_idx = file.chunks().checked_sub(1)?;
	if chunk_idx < last_chunk_idx {
		Some(FILE_CHUNK_SIZE.saturating_add(FILE_CHUNK_SIZE_EXTRA.get()))
	} else if chunk_idx == last_chunk_idx {
		let size: u64 = chunk_idx
			.checked_mul(u64::from(FILE_CHUNK_SIZE.get()))
			.and_then(|chunk_start| file.size().checked_sub(chunk_start))?
			.saturating_add(u64::from(FILE_CHUNK_SIZE_EXTRA.get()));
		let size: u32 = size.try_into().ok()?;
		NonZeroU32::new(size)
	} else {
		None
	}
}

/// Bytes the first chunk of `file` takes to download, or `None` when the file has no bytes to
/// download.
pub(crate) fn first_chunk_size(file: &dyn File) -> Option<NonZeroU32> {
	if file.size() == 0 {
		return None;
	}
	encrypted_chunk_size(file, 0)
}

/// Downloads and decrypts the first chunk of `file`, charged to the shared memory budget for as
/// long as the returned chunk lives. `None` when the file has no bytes to download.
pub(crate) async fn fetch_first_chunk<'a>(
	client: &'a UnauthClient,
	file: &dyn File,
) -> Result<Option<Chunk<'a>>, Error> {
	check_chunks_consistent(file.chunks(), file.size())?;
	let Some(chunk_size) = first_chunk_size(file) else {
		return Ok(None);
	};
	let reservation = Chunk::acquire(chunk_size, client.state()).await;
	fetch_decrypted_chunk(client, file, 0, reservation, None)
		.await
		.map(Some)
}

/// Plaintext length of chunk `index` of a `size`-byte file.
pub(crate) fn chunk_plaintext_len(size: u64, index: u64) -> u64 {
	size.saturating_sub(index * CHUNK_SIZE_U64)
		.min(CHUNK_SIZE_U64)
}

impl<'a> FileReader<'a> {
	pub(crate) fn new(file: &'a dyn File, client: &'a UnauthClient) -> Self {
		FileReaderBuilder::new(client, file).build()
	}

	pub(crate) fn new_for_range(
		file: &'a dyn File,
		client: &'a UnauthClient,
		start: u64,
		end: u64,
	) -> Self {
		FileReaderBuilder::new(client, file)
			.with_start(start)
			.with_end(end)
			.build()
	}

	/// Adds memory the caller already holds to the read-ahead, as the reservation for the next
	/// chunk. With it the reader progresses whatever else waits on the shared budget. Released
	/// at once when no chunk is left to fetch.
	pub(crate) fn add_reservation(&mut self, reservation: Chunk<'a>) {
		self.push_fetch_next_chunk(reservation);
	}

	fn next_chunk_size(&self) -> Option<NonZeroU32> {
		// Once the read position reaches the range limit no further bytes are ever
		// wanted — without this an empty range (start == end mid-chunk) still fetches
		// the chunk containing that position.
		if self.index >= self.limit {
			return None;
		}
		// A chunk starting at or past the range limit contains no wanted bytes. Every
		// fetch decision funnels through here, so without this bound a ranged reader
		// keeps downloading (and decrypting) chunks to EOF after the range is exhausted.
		if self
			.next_chunk_idx
			.checked_mul(CHUNK_SIZE_U64)
			.is_none_or(|chunk_start| chunk_start >= self.limit)
		{
			return None;
		}
		encrypted_chunk_size(self.file, self.next_chunk_idx)
	}

	/// Whether reserving one more `chunk_size`-byte chunk keeps the in-flight pipeline (already
	/// queued fetches plus any pending blocking acquire) within `max_buffer_size`. Gates both the
	/// eager non-blocking fill and the blocking acquire, so a single stream's read-ahead can never
	/// grow past its budget and pin the whole shared memory budget.
	fn within_buffer_budget(&self, chunk_size: NonZeroU32) -> bool {
		let current_allocated = (self.allocate_chunk_future.is_some() as u64
			+ self.futures.len() as u64)
			* CHUNK_SIZE_U64;
		current_allocated + u64::from(chunk_size.get()) <= self.max_buffer_size
	}

	fn try_allocate_next_chunk(&self) -> Option<Chunk<'a>> {
		let chunk_size = self.next_chunk_size()?;
		if !self.within_buffer_budget(chunk_size) {
			return None;
		}

		Chunk::try_acquire(chunk_size, self.client.state())
	}

	fn allocate_next_chunk(&self) -> Option<MaybeSendBoxFuture<'a, Chunk<'a>>> {
		let chunk_size = self.next_chunk_size()?;
		// Re-arm the blocking acquire while the pipeline stays within the read-ahead budget, so a
		// single stream's pipeline never grows to the whole shared memory budget. Always keep at
		// least one chunk in flight, though: a reader whose entire budget is below one *encrypted*
		// chunk (the default budget for a sub-1-MiB file is the file size, one AES tag+nonce short
		// of a full chunk) would otherwise prime nothing and return a premature EOF. The blocking
		// acquire draws from the global memory semaphore, not `max_buffer_size`, so priming one
		// chunk here is always satisfiable.
		let pipeline_empty = self.futures.is_empty() && self.allocate_chunk_future.is_none();
		if !pipeline_empty && !self.within_buffer_budget(chunk_size) {
			return None;
		}
		Some(Box::pin(Chunk::acquire(chunk_size, self.client.state()))
			as MaybeSendBoxFuture<'a, Chunk<'a>>)
	}

	/// Pushes the future to fetch the next chunk.
	///
	/// Requires that `out_data` have the necessary capacity to store the entire chunk returned from the server
	fn push_fetch_next_chunk(&mut self, out_data: Chunk<'a>) {
		// Funnel through next_chunk_size so the range-limit bound applies here too:
		// read_next_chunk recycles the previous chunk's buffer into this call without
		// consulting chunk sizing first.
		if self.next_chunk_size().is_none() {
			return;
		}
		let chunk_idx = self.next_chunk_idx;
		self.next_chunk_idx += 1;

		let first_chunk = self.index / CHUNK_SIZE_U64 == chunk_idx;
		let index = self.index;
		let client = self.client;
		let file = self.file;
		let progress = self.progress.clone();
		self.futures.push_back(Box::pin(async move {
			let chunk = fetch_decrypted_chunk(client, file, chunk_idx, out_data, progress).await?;

			Ok(if first_chunk {
				let mut cursor = Cursor::new(chunk);
				cursor.set_position(index % CHUNK_SIZE_U64);
				cursor
			} else {
				Cursor::new(chunk)
			})
		}));
	}

	/// Reads into `buf` from `self.curr_chunk` and returns the number of bytes read
	/// if `curr_chunk` is `None`, it returns 0
	///
	/// If `curr_chunk` is not `None`, it will read from it and return the number of bytes read.
	/// If the whole chunk was read, it will fetch the next chunk
	fn read_next_chunk(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		// take the chunk out of curr_chunk
		match self.curr_chunk.take() {
			Some(mut cursor) => {
				let max_read = match usize::try_from(self.limit - self.index) {
					Ok(v) => v.min(buf.len()),
					Err(_) => buf.len(),
				};
				let read = cursor.read(&mut buf[..max_read])?;
				self.index += u64::try_from(read).unwrap();
				if (cursor.position()) < u64::try_from(cursor.get_ref().as_ref().len()).unwrap()
					&& self.index < self.limit
				{
					// didn't read the whole chunk, put it back and return
					self.curr_chunk = Some(cursor);
				} else {
					// read the whole chunk, so we need to fetch the next one
					self.push_fetch_next_chunk(cursor.into_inner());
				}
				Ok(read)
			}
			None => Ok(0),
		}
	}
}

impl FileReader<'_> {
	/// Queues as many chunk fetches as the read-ahead budget allows and polls the pending
	/// allocation. Returns whether that allocation is still pending with chunks left to fetch,
	/// in which case the reader must wait for it rather than report EOF.
	fn poll_read_ahead(&mut self, cx: &mut std::task::Context<'_>) -> bool {
		// first try to queue more chunks
		while let Some(chunk) = self.try_allocate_next_chunk() {
			self.push_fetch_next_chunk(chunk);
		}

		// then see if our allocation future is ready
		let Some(mut fut) = self.allocate_chunk_future.take() else {
			return false;
		};
		match fut.as_mut().poll(cx) {
			std::task::Poll::Ready(chunk) => {
				self.push_fetch_next_chunk(chunk);
				self.allocate_chunk_future = self.allocate_next_chunk();
				false
			}
			std::task::Poll::Pending => {
				// allocation is still pending, we can't read anything yet
				if self.next_chunk_size().is_some() {
					// we have more chunks to allocate, so we put the future back
					self.allocate_chunk_future = Some(fut);
					return true;
				}
				// if we don't have more chunks to allocate, we can drop the future
				false
			}
		}
	}
}

/// The bytes of `cursor` still to be read by a reader at `index` whose range ends at `limit`.
fn unread<'c>(cursor: &'c Cursor<Chunk<'_>>, index: u64, limit: u64) -> &'c [u8] {
	let data = cursor.get_ref().as_ref();
	let start = usize::try_from(cursor.position()).unwrap_or(usize::MAX);
	let wanted = usize::try_from(limit.saturating_sub(index)).unwrap_or(usize::MAX);
	let end = start.saturating_add(wanted).min(data.len());
	data.get(start..end).unwrap_or_default()
}

/// Hands out each decrypted chunk in place, so a caller that only forwards the bytes copies them
/// once instead of through an intermediate buffer.
impl futures::io::AsyncBufRead for FileReader<'_> {
	fn poll_fill_buf(
		self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<&[u8]>> {
		let this = self.get_mut();
		if let Err(error) = check_chunks_consistent(this.file.chunks(), this.file.size()) {
			return std::task::Poll::Ready(Err(std::io::Error::other(error)));
		}

		let should_pend = this.poll_read_ahead(cx);

		// `curr_chunk` only ever holds a chunk with bytes left to read
		let cursor = loop {
			if let Some(cursor) = this.curr_chunk.take() {
				break cursor;
			}
			match this.futures.poll_next_unpin(cx) {
				std::task::Poll::Ready(Some(Ok(cursor))) => {
					if !unread(&cursor, this.index, this.limit).is_empty() {
						break cursor;
					}
					this.push_fetch_next_chunk(cursor.into_inner());
				}
				std::task::Poll::Ready(Some(Err(e))) => {
					return std::task::Poll::Ready(Err(std::io::Error::other(e)));
				}
				std::task::Poll::Ready(None) => {
					if should_pend {
						return std::task::Poll::Pending;
					}
					return std::task::Poll::Ready(Ok(&[]));
				}
				std::task::Poll::Pending => return std::task::Poll::Pending,
			}
		};
		let cursor = this.curr_chunk.insert(cursor);
		std::task::Poll::Ready(Ok(unread(cursor, this.index, this.limit)))
	}

	fn consume(self: std::pin::Pin<&mut Self>, amt: usize) {
		let this = self.get_mut();
		let Some(mut cursor) = this.curr_chunk.take() else {
			return;
		};
		let amt = amt.min(unread(&cursor, this.index, this.limit).len());
		let amt = u64::try_from(amt).unwrap_or(u64::MAX);
		cursor.set_position(cursor.position().saturating_add(amt));
		this.index = this.index.saturating_add(amt);
		if unread(&cursor, this.index, this.limit).is_empty() {
			this.push_fetch_next_chunk(cursor.into_inner());
		} else {
			this.curr_chunk = Some(cursor);
		}
	}
}

impl futures::io::AsyncRead for FileReader<'_> {
	fn poll_read(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
		buf: &mut [u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		if let Err(error) = check_chunks_consistent(self.file.chunks(), self.file.size()) {
			return std::task::Poll::Ready(Err(std::io::Error::other(error)));
		}

		let should_pend = self.poll_read_ahead(cx);

		// then see if we have a stored chunk
		let mut read = self.read_next_chunk(buf)?;
		if read >= buf.len() {
			// we've filled the buffer
			return std::task::Poll::Ready(Ok(read));
		}

		loop {
			// loop through futures
			match self.futures.poll_next_unpin(cx) {
				std::task::Poll::Ready(Some(Ok(cursor))) => {
					// we have a new chunk, make a cursor and read from it
					self.curr_chunk = Some(cursor);
					read += self.read_next_chunk(&mut buf[read..])?;
					if read >= buf.len() {
						// we've filled the buffer
						return std::task::Poll::Ready(Ok(read));
					}
				}
				std::task::Poll::Ready(Some(Err(e))) => {
					return std::task::Poll::Ready(Err(std::io::Error::other(e)));
				}
				std::task::Poll::Ready(None) => {
					if should_pend && read == 0 {
						// if we were waiting for allocation and we haven't read anything,
						// we need to pend
						return std::task::Poll::Pending;
					}
					return std::task::Poll::Ready(Ok(read));
				}
				std::task::Poll::Pending => {
					if read > 0 {
						// we have read some data, return it
						return std::task::Poll::Ready(Ok(read));
					}
					return std::task::Poll::Pending;
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{borrow::Cow, io::Write};

	use chrono::{DateTime, Utc};
	use filen_types::{crypto::Blake3Hash, fs::Uuid};
	use futures::{
		executor::block_on,
		io::{AsyncBufReadExt, AsyncReadExt},
	};

	use super::*;
	use crate::{
		auth::http::ClientConfig,
		consts::CHUNK_SIZE,
		crypto::file::FileKey,
		fs::{
			HasMeta, HasName, HasRemoteInfo, HasUUID,
			file::traits::{HasFileInfo, HasRemoteFileInfo},
		},
	};

	struct FakeFile {
		uuid: Uuid,
		size: u64,
		chunks: u64,
	}

	impl FakeFile {
		fn new(size: u64, chunks: u64) -> Self {
			Self {
				uuid: Uuid::default(),
				size,
				chunks,
			}
		}
	}

	impl HasUUID for FakeFile {
		fn uuid(&self) -> Uuid {
			self.uuid
		}
	}

	impl HasName for FakeFile {
		fn name(&self) -> Option<&str> {
			Some("fake")
		}
	}

	impl HasMeta for FakeFile {
		fn get_meta_string(&self) -> Option<Cow<'_, str>> {
			None
		}
	}

	impl HasRemoteInfo for FakeFile {
		fn favorited(&self) -> bool {
			false
		}

		fn timestamp(&self) -> DateTime<Utc> {
			DateTime::<Utc>::UNIX_EPOCH
		}
	}

	impl HasFileInfo for FakeFile {
		fn mime(&self) -> Option<&str> {
			None
		}

		fn created(&self) -> Option<DateTime<Utc>> {
			None
		}

		fn last_modified(&self) -> Option<DateTime<Utc>> {
			None
		}

		fn size(&self) -> u64 {
			self.size
		}

		fn chunks(&self) -> u64 {
			self.chunks
		}

		fn key(&self) -> Option<&FileKey> {
			None
		}
	}

	impl HasRemoteFileInfo for FakeFile {
		fn region(&self) -> &str {
			""
		}

		fn bucket(&self) -> &str {
			""
		}

		fn hash(&self) -> Option<Blake3Hash> {
			None
		}
	}

	impl File for FakeFile {}

	fn test_client() -> UnauthClient {
		UnauthClient::from_config(ClientConfig::default()).unwrap()
	}

	fn read_error_kind(file: &FakeFile) -> ErrorKind {
		let client = test_client();
		let mut reader = FileReaderBuilder::new(&client, file).build();
		let mut buf = [0u8; 16];
		let err = block_on(reader.read(&mut buf)).expect_err("read should fail");
		err.get_ref()
			.and_then(|inner| inner.downcast_ref::<Error>())
			.map(|e| e.kind())
			.expect("expected an sdk error")
	}

	#[test]
	fn too_many_chunks_for_size_errors_instead_of_panicking() {
		let file = FakeFile::new(CHUNK_SIZE_U64, 3);
		assert_eq!(read_error_kind(&file), ErrorKind::Response);
	}

	#[test]
	fn oversized_single_chunk_errors_instead_of_panicking() {
		let file = FakeFile::new(5 * 1024 * CHUNK_SIZE_U64, 1);
		assert_eq!(read_error_kind(&file), ErrorKind::Response);
	}

	#[test]
	fn zero_chunks_with_nonzero_size_errors_instead_of_truncating() {
		let file = FakeFile::new(10 * 1024 * 1024 * 1024, 0);
		assert_eq!(read_error_kind(&file), ErrorKind::Response);
	}

	#[test]
	fn empty_file_reads_eof() {
		let client = test_client();
		let file = FakeFile::new(0, 0);
		let mut reader = FileReaderBuilder::new(&client, &file).build();
		let mut buf = [0u8; 8];
		assert_eq!(block_on(reader.read(&mut buf)).unwrap(), 0);
	}

	#[test]
	fn wellformed_file_computes_chunk_sizes() {
		let client = test_client();
		let file = FakeFile::new(2 * CHUNK_SIZE_U64 + 512 * 1024, 3);
		let mut reader = FileReaderBuilder::new(&client, &file)
			.with_max_buffer_size(0)
			.build();

		let full = FILE_CHUNK_SIZE.get() + FILE_CHUNK_SIZE_EXTRA.get();
		assert_eq!(reader.next_chunk_size().map(NonZeroU32::get), Some(full));
		reader.next_chunk_idx = 1;
		assert_eq!(reader.next_chunk_size().map(NonZeroU32::get), Some(full));
		reader.next_chunk_idx = 2;
		assert_eq!(
			reader.next_chunk_size().map(NonZeroU32::get),
			Some(512 * 1024 + FILE_CHUNK_SIZE_EXTRA.get())
		);
		reader.next_chunk_idx = 3;
		assert_eq!(reader.next_chunk_size(), None);
	}

	#[test]
	fn ranged_reader_does_not_prefetch_past_limit() {
		let client = test_client();
		let file = FakeFile::new(10 * CHUNK_SIZE_U64, 10);
		// range [0, 1.5 MiB): only chunks 0 and 1 contain wanted bytes
		let reader = FileReaderBuilder::new(&client, &file)
			.with_end(CHUNK_SIZE_U64 + CHUNK_SIZE_U64 / 2)
			.build();
		assert_eq!(reader.next_chunk_idx, 2);
		assert_eq!(reader.futures.len(), 2);
		assert!(reader.allocate_chunk_future.is_none());
	}

	#[test]
	fn ranged_reader_prefetch_stops_at_exact_chunk_boundary() {
		let client = test_client();
		let file = FakeFile::new(4 * CHUNK_SIZE_U64, 4);
		// end lands exactly on the chunk 2 boundary: chunk 2 starts at the limit
		// and contains no wanted bytes, while chunk 1 (holding byte limit-1) does
		let reader = FileReaderBuilder::new(&client, &file)
			.with_end(2 * CHUNK_SIZE_U64)
			.build();
		assert_eq!(reader.next_chunk_idx, 2);
		assert_eq!(reader.futures.len(), 2);
	}

	#[test]
	fn ranged_reader_prefetches_only_chunks_within_range() {
		let client = test_client();
		let file = FakeFile::new(10 * CHUNK_SIZE_U64, 10);
		// range [8.5 MiB, 9 MiB) lies entirely within chunk 8
		let reader = FileReaderBuilder::new(&client, &file)
			.with_start(8 * CHUNK_SIZE_U64 + CHUNK_SIZE_U64 / 2)
			.with_end(9 * CHUNK_SIZE_U64)
			.build();
		assert_eq!(reader.next_chunk_idx, 9);
		assert_eq!(reader.futures.len(), 1);
	}

	#[test]
	fn empty_range_fetches_nothing() {
		let client = test_client();
		let file = FakeFile::new(10 * CHUNK_SIZE_U64, 10);
		// start == end mid-chunk: zero bytes wanted, so not even the chunk
		// containing that position may be fetched
		let mut reader = FileReaderBuilder::new(&client, &file)
			.with_start(CHUNK_SIZE_U64 / 2)
			.with_end(CHUNK_SIZE_U64 / 2)
			.build();
		assert_eq!(reader.futures.len(), 0);
		assert!(reader.allocate_chunk_future.is_none());
		let mut buf = [0u8; 8];
		assert_eq!(block_on(reader.read(&mut buf)).unwrap(), 0);
	}

	#[test]
	fn ranged_reader_does_not_cascade_fetches_after_limit_reached() {
		let client = test_client();
		let file = FakeFile::new(10 * CHUNK_SIZE_U64, 10);
		let mut reader = FileReaderBuilder::new(&client, &file)
			.with_end(CHUNK_SIZE_U64 / 2)
			.with_max_buffer_size(0)
			.build();
		// simulate the state right after the range was exhausted mid-chunk:
		// chunk 0 is current with unread bytes left, and index sits at the limit
		reader.index = reader.limit;
		reader.next_chunk_idx = 1;
		let chunk = Chunk::try_acquire(
			FILE_CHUNK_SIZE.saturating_add(FILE_CHUNK_SIZE_EXTRA.get()),
			client.state(),
		)
		.unwrap();
		reader.curr_chunk = Some(Cursor::new(chunk));
		let mut buf = [0u8; 16];
		assert_eq!(reader.read_next_chunk(&mut buf).unwrap(), 0);
		assert_eq!(
			reader.futures.len(),
			0,
			"reaching the range limit must not enqueue further chunk fetches"
		);
	}

	#[test]
	fn buffered_read_of_an_empty_file_is_empty() {
		let client = test_client();
		let file = FakeFile::new(0, 0);
		let mut reader = FileReaderBuilder::new(&client, &file).build();
		assert!(block_on(reader.fill_buf()).unwrap().is_empty());
	}

	#[test]
	fn buffered_read_of_an_inconsistent_file_errors() {
		let client = test_client();
		let file = FakeFile::new(CHUNK_SIZE_U64, 3);
		let mut reader = FileReaderBuilder::new(&client, &file).build();
		let err = block_on(reader.fill_buf()).expect_err("read should fail");
		let kind = err
			.get_ref()
			.and_then(|inner| inner.downcast_ref::<Error>())
			.map(|e| e.kind());
		assert_eq!(kind, Some(ErrorKind::Response));
	}

	#[test]
	fn buffered_read_hands_out_the_chunk_up_to_the_range_limit_then_recycles_it() {
		let client = test_client();
		let file = FakeFile::new(10 * CHUNK_SIZE_U64, 10);
		let half = CHUNK_SIZE_U64 / 2;
		let mut reader = FileReaderBuilder::new(&client, &file)
			.with_end(half)
			.with_max_buffer_size(0)
			.build();
		// the state once chunk 0 is in: it is current and holds more than the range wants
		reader.futures = FuturesOrdered::new();
		reader.allocate_chunk_future = None;
		reader.next_chunk_idx = 1;
		let mut chunk = Chunk::try_acquire(
			FILE_CHUNK_SIZE.saturating_add(FILE_CHUNK_SIZE_EXTRA.get()),
			client.state(),
		)
		.unwrap();
		chunk.write_all(&vec![7u8; CHUNK_SIZE]).unwrap();
		reader.curr_chunk = Some(Cursor::new(chunk));

		let data = block_on(reader.fill_buf()).unwrap();
		assert_eq!(data.len(), usize::try_from(half).unwrap());
		assert!(data.iter().all(|byte| *byte == 7));
		reader.consume_unpin(usize::try_from(half).unwrap());

		assert!(reader.curr_chunk.is_none());
		assert_eq!(reader.index, half);
		assert!(block_on(reader.fill_buf()).unwrap().is_empty());
	}

	#[test]
	fn an_added_reservation_fetches_the_next_chunk() {
		let client = test_client();
		let file = FakeFile::new(10 * CHUNK_SIZE_U64, 10);
		let mut reader = FileReaderBuilder::new(&client, &file)
			.with_start(CHUNK_SIZE_U64)
			.with_max_buffer_size(0)
			.build();
		let queued = reader.futures.len();
		let chunk = Chunk::try_acquire(
			FILE_CHUNK_SIZE.saturating_add(FILE_CHUNK_SIZE_EXTRA.get()),
			client.state(),
		)
		.unwrap();

		reader.add_reservation(chunk);

		assert_eq!(reader.futures.len(), queued + 1);
		assert_eq!(reader.next_chunk_idx, 2);
	}

	#[test]
	fn a_reservation_past_the_last_chunk_is_released() {
		let client = test_client();
		let file = FakeFile::new(CHUNK_SIZE_U64, 1);
		let mut reader = FileReaderBuilder::new(&client, &file)
			.with_start(CHUNK_SIZE_U64)
			.build();
		let free = client.state().memory_semaphore().available_permits();
		let chunk = Chunk::try_acquire(
			FILE_CHUNK_SIZE.saturating_add(FILE_CHUNK_SIZE_EXTRA.get()),
			client.state(),
		)
		.unwrap();

		reader.add_reservation(chunk);

		assert!(reader.futures.is_empty());
		assert_eq!(client.state().memory_semaphore().available_permits(), free);
	}

	#[test]
	fn allocate_future_respects_read_ahead_budget() {
		let client = test_client();
		// 16-chunk file, but a read-ahead budget of only 2 encrypted chunks.
		let file = FakeFile::new(16 * CHUNK_SIZE_U64, 16);
		let two_chunks = 2 * (CHUNK_SIZE_U64 + u64::from(FILE_CHUNK_SIZE_EXTRA.get()));
		let reader = FileReaderBuilder::new(&client, &file)
			.with_max_buffer_size(two_chunks)
			.build();
		// The pipeline (queued fetches plus any pending blocking acquire) must stay within the
		// 2-chunk read-ahead budget. Before the fix the blocking acquire was re-armed one past the
		// budget, so `reserved` was 3 and, left unchecked, grew to the full shared memory budget.
		let reserved = reader.futures.len() + reader.allocate_chunk_future.is_some() as usize;
		assert!(
			reserved <= 2,
			"pipeline reserved {reserved} chunks, exceeding the 2-chunk read-ahead budget"
		);
		assert!(
			reader.allocate_chunk_future.is_none(),
			"blocking acquire must not be armed once the eager fill already fills the budget"
		);
	}

	#[test]
	fn single_chunk_reader_primes_a_fetch_within_a_sub_chunk_budget() {
		let client = test_client();
		// A sub-1-MiB file is a single chunk, and the default read-ahead budget is the file size:
		// one AES tag+nonce short of a full *encrypted* chunk. The blocking acquire must still be
		// primed (it draws from the global memory semaphore, not the read-ahead budget) so the
		// reader makes progress instead of returning a premature EOF.
		let file = FakeFile::new(10 * 1024, 1);
		let reader = FileReaderBuilder::new(&client, &file).build();
		assert!(
			reader.allocate_chunk_future.is_some(),
			"a single-chunk reader whose budget is below one encrypted chunk must still prime a fetch"
		);
		assert!(reader.futures.is_empty());
	}

	#[test]
	fn chunk_consistency_boundaries() {
		assert!(chunks_consistent_with_size(0, 0));
		assert!(!chunks_consistent_with_size(0, 1));
		assert!(!chunks_consistent_with_size(0, CHUNK_SIZE_U64));
		assert!(chunks_consistent_with_size(1, 0));
		assert!(chunks_consistent_with_size(1, 1));
		assert!(chunks_consistent_with_size(1, CHUNK_SIZE_U64));
		assert!(chunks_consistent_with_size(2, CHUNK_SIZE_U64 + 1));
		assert!(chunks_consistent_with_size(2, 2 * CHUNK_SIZE_U64));
		assert!(!chunks_consistent_with_size(1, CHUNK_SIZE_U64 + 1));
		assert!(!chunks_consistent_with_size(3, CHUNK_SIZE_U64));
		assert!(!chunks_consistent_with_size(u64::MAX, u64::MAX));
	}
}
