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
	// Set at build time. When false, the advertised size/chunk count cannot describe a real
	// file and every read yields an error instead of attempting the chunk-size math.
	chunks_consistent: bool,
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
			chunks_consistent,
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

/// Whether a file's advertised chunk count can be produced from its advertised size: the last
/// chunk must not start past the end of the file and its plaintext must fit within one chunk.
/// Remote metadata violating this would drive the chunk-size math out of range, so such
/// readers refuse to read. A chunk count of zero is only consistent with a zero size (legacy
/// empty files); with a nonzero size it would silently read as truncated-to-empty.
fn chunks_consistent_with_size(chunks: u64, size: u64) -> bool {
	match chunks.checked_sub(1) {
		None => size == 0,
		Some(last_chunk_idx) => last_chunk_idx
			.checked_mul(CHUNK_SIZE_U64)
			.and_then(|last_chunk_start| size.checked_sub(last_chunk_start))
			.is_some_and(|last_chunk_len| last_chunk_len <= CHUNK_SIZE_U64),
	}
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

	fn next_chunk_size(&self) -> Option<NonZeroU32> {
		if self.file.chunks() == 0 {
			return None;
		}
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
		if self.next_chunk_idx < self.file.chunks() - 1 {
			Some(FILE_CHUNK_SIZE.saturating_add(FILE_CHUNK_SIZE_EXTRA.get()))
		} else if self.next_chunk_idx == self.file.chunks() - 1 {
			let size: u64 = self
				.next_chunk_idx
				.checked_mul(u64::from(FILE_CHUNK_SIZE.get()))
				.and_then(|chunk_start| self.file.size().checked_sub(chunk_start))?
				.saturating_add(u64::from(FILE_CHUNK_SIZE_EXTRA.get()));
			let size: u32 = size.try_into().ok()?;
			NonZeroU32::new(size)
		} else {
			None
		}
	}

	fn try_allocate_next_chunk(&self) -> Option<Chunk<'a>> {
		let chunk_size = self.next_chunk_size()?;

		let current_allocated = (self.allocate_chunk_future.is_some() as u64
			+ self.futures.len() as u64)
			* CHUNK_SIZE_U64;
		if current_allocated + u64::from(chunk_size.get()) > self.max_buffer_size {
			return None;
		}

		Chunk::try_acquire(chunk_size, self.client.state())
	}

	fn allocate_next_chunk(&self) -> Option<MaybeSendBoxFuture<'a, Chunk<'a>>> {
		let chunk_size = self.next_chunk_size()?;
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
		// Plaintext size of this chunk; reported bytes are clamped to it so the encrypted body's
		// per-chunk overhead never over-counts past the (plaintext) file size.
		let plaintext_len = file
			.size()
			.saturating_sub(chunk_idx * CHUNK_SIZE_U64)
			.min(CHUNK_SIZE_U64);
		self.futures.push_back(Box::pin(async move {
			let (_, permits) = out_data.into_parts();
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
			let data = api::download::download_file_chunk(client, file, chunk_idx, Some(&on_bytes))
				.await?;
			let mut chunk = Chunk::from_parts(data, permits);
			file.key()
				.ok_or(MetadataWasNotDecryptedError)?
				.decrypt_data(chunk.as_mut())
				.await?;

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

impl futures::io::AsyncRead for FileReader<'_> {
	fn poll_read(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
		buf: &mut [u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		if !self.chunks_consistent {
			return std::task::Poll::Ready(Err(std::io::Error::other(Error::custom(
				ErrorKind::Response,
				format!(
					"file chunk count ({}) is inconsistent with file size ({})",
					self.file.chunks(),
					self.file.size()
				),
			))));
		}

		// first try to queue more chunks
		while let Some(chunk) = self.try_allocate_next_chunk() {
			self.push_fetch_next_chunk(chunk);
		}

		// first see if our allocation future is ready
		let mut should_pend = false;
		if let Some(mut fut) = self.allocate_chunk_future.take() {
			match fut.as_mut().poll(cx) {
				std::task::Poll::Ready(chunk) => {
					// we have a new chunk, set it to curr_chunk
					self.push_fetch_next_chunk(chunk);
					self.allocate_chunk_future = self.allocate_next_chunk();
				}
				std::task::Poll::Pending => {
					// allocation is still pending, we can't read anything yet
					if self.next_chunk_size().is_some() {
						// we have more chunks to allocate, so we put the future back
						self.allocate_chunk_future = Some(fut);
						should_pend = true;
					}
					// if we don't have more chunks to allocate, we can drop the future
				}
			}
		}

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
	use std::borrow::Cow;

	use chrono::{DateTime, Utc};
	use filen_types::{crypto::Blake3Hash, fs::Uuid};
	use futures::{executor::block_on, io::AsyncReadExt};

	use super::*;
	use crate::{
		auth::http::ClientConfig,
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
