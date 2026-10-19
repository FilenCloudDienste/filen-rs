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

// Chunk by chunk, for a caller that writes each chunk's bytes out itself instead of reading them
// through `AsyncRead`. A file is read one way or the other, not both.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
impl<'a> FileReader<'a> {
	/// The range's next decrypted chunk, holding only the range's bytes, or `None` past the
	/// range's end. Hand it back with [`Self::recycle_chunk`] once its bytes are used, so its memory
	/// fetches a later chunk.
	pub(crate) fn poll_next_chunk(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<Option<Chunk<'a>>>> {
		if let Err(error) = check_chunks_consistent(self.file.chunks(), self.file.size()) {
			return std::task::Poll::Ready(Err(std::io::Error::other(error)));
		}
		// A chunk the caller dropped instead of handing back fetches nothing, so with nothing in
		// flight the read would end here. Keep one acquire armed, as the reader always does.
		if self.futures.is_empty() && self.allocate_chunk_future.is_none() {
			self.allocate_chunk_future = self.allocate_next_chunk();
		}
		let waiting_for_memory = self.poll_read_ahead(cx);
		match std::task::ready!(self.futures.poll_next_unpin(cx)) {
			Some(Ok(cursor)) => {
				// the first chunk is read from the range's start, and any chunk up to its end
				let skip = usize::try_from(cursor.position()).unwrap_or(usize::MAX);
				let wanted =
					usize::try_from(self.limit.saturating_sub(self.index)).unwrap_or(usize::MAX);
				let mut chunk = cursor.into_inner();
				let data: &mut Vec<u8> = chunk.as_mut();
				data.truncate(skip.saturating_add(wanted));
				data.drain(..skip.min(data.len()));
				self.index += data.len() as u64;
				std::task::Poll::Ready(Ok(Some(chunk)))
			}
			Some(Err(e)) => std::task::Poll::Ready(Err(std::io::Error::other(e))),
			None if waiting_for_memory => std::task::Poll::Pending,
			None => std::task::Poll::Ready(Ok(None)),
		}
	}

	/// Fetches a later chunk with the memory of `chunk`, one from [`Self::poll_next_chunk`] whose
	/// bytes are used.
	pub(crate) fn recycle_chunk(&mut self, chunk: Chunk<'a>) {
		self.push_fetch_next_chunk(chunk);
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
mod tests;
