//! Writes an archive's entries one after another, in order. The memory budget is split in two:
//! the file being written reads ahead with one half, and a window of the entries after it
//! downloads their first chunks with the other, so a run of small files does not wait a round
//! trip per file and a waiting entry holds no more than one chunk.

use std::{
	collections::VecDeque,
	future::poll_fn,
	pin::{Pin, pin},
	sync::{Mutex, MutexGuard, PoisonError},
	task::{Context, Poll},
};

use async_zip::{Compression, ZipEntryBuilder, base::write::ZipFileWriter};
use futures::{
	AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, Stream, StreamExt,
	stream::FuturesOrdered,
};
use tracing::debug;

use crate::{
	Error, ErrorKind,
	auth::unauth::UnauthClient,
	consts::{CHUNK_SIZE_U64, FULL_CHUNK_BYTES},
	fs::{
		HasUUID,
		file::{
			chunk::Chunk,
			enums::RemoteFileType,
			read::{FileReader, FileReaderBuilder, fetch_first_chunk, first_chunk_size},
			traits::HasFileInfo,
		},
	},
	util::{MaybeSend, MaybeSendBoxFuture, MaybeSendSync},
};

use super::{
	ZipProgressCallback, ZipState, add_dir_times, add_file_times,
	walk::{DirEntry, Entry},
};

/// Entries the window holds at most, whatever their first chunks weigh: enough small files to
/// keep the client's request concurrency busy.
const MAX_WINDOW_ENTRIES: usize = 32;

/// The most bytes handed to the archive in one write, so the stream's frames stay a fraction of a
/// chunk and the buffers between here and the consumer stay small.
const MAX_WRITE: usize = 256 * 1024;

/// What writing an archive needs from the client, as a trait so the engine can run against a
/// fake in tests.
pub(super) trait ZipBackend: MaybeSendSync {
	/// The file-IO memory budget, in bytes.
	fn memory_budget(&self) -> u64;
	/// Downloads and decrypts the first chunk of `file`, held against the memory budget for as
	/// long as it lives. `None` when the file has no bytes to download.
	fn first_chunk<'s>(
		&'s self,
		file: &RemoteFileType<'_>,
	) -> impl Future<Output = Result<Option<Chunk<'s>>, Error>> + MaybeSend;
	/// `file` from its second chunk on, with at most `read_ahead` bytes of chunks in flight.
	fn remainder<'f>(
		&'f self,
		file: &'f RemoteFileType<'_>,
		read_ahead: u64,
	) -> impl Remainder<'f> + 'f;
}

/// A file's bytes from its second chunk on.
pub(super) trait Remainder<'a>: AsyncBufRead + Unpin + MaybeSend {
	/// Adds memory the caller already holds to the read-ahead, so the reader progresses whatever
	/// else waits on the shared budget.
	fn add_reservation(&mut self, reservation: Chunk<'a>);
}

impl<'a> Remainder<'a> for FileReader<'a> {
	fn add_reservation(&mut self, reservation: Chunk<'a>) {
		FileReader::add_reservation(self, reservation);
	}
}

impl ZipBackend for UnauthClient {
	fn memory_budget(&self) -> u64 {
		u64::try_from(self.state().memory_budget()).unwrap_or(u64::MAX)
	}

	fn first_chunk<'s>(
		&'s self,
		file: &RemoteFileType<'_>,
	) -> impl Future<Output = Result<Option<Chunk<'s>>, Error>> + MaybeSend {
		fetch_first_chunk(self, file)
	}

	fn remainder<'f>(
		&'f self,
		file: &'f RemoteFileType<'_>,
		read_ahead: u64,
	) -> impl Remainder<'f> + 'f {
		FileReaderBuilder::new(self, file)
			.with_start(CHUNK_SIZE_U64)
			.with_max_buffer_size(read_ahead)
			.build()
	}
}

/// How the memory budget is split between the file being written and the window after it.
#[derive(Debug, PartialEq, Eq)]
struct BudgetSplit {
	read_ahead: u64,
	window: u64,
}

impl BudgetSplit {
	/// Half each, except that the file being written keeps at least a full chunk for its
	/// read-ahead. Its progress does not depend on the split: it reuses its first chunk's memory.
	fn new(budget: u64) -> Self {
		let full_chunk = u64::try_from(FULL_CHUNK_BYTES).unwrap_or(u64::MAX);
		let window = (budget / 2).min(budget.saturating_sub(full_chunk));
		Self {
			read_ahead: budget - window,
			window,
		}
	}
}

/// The memory an entry's first chunk takes.
fn first_chunk_cost(entry: &Entry<'_>) -> u64 {
	match entry {
		Entry::File {
			file,
			path: Some(_),
		} => first_chunk_size(file).map_or(0, |size| u64::from(size.get())),
		Entry::File { path: None, .. } | Entry::Dir(_) => 0,
	}
}

/// An entry ready to be written.
struct Fetched<'a> {
	entry: Entry<'a>,
	first_chunk: Option<Chunk<'a>>,
	/// Of the window's budget, released once the entry leaves the window.
	reserved: u64,
}

/// The window of entries after the one being written, each downloading its first chunk.
struct Lookahead<'a, B, S> {
	backend: &'a B,
	/// `None` once it has ended or failed.
	entries: Option<S>,
	/// Taken from `entries` while the window had no room for its first chunk.
	waiting: Option<Entry<'a>>,
	fetching: FuturesOrdered<MaybeSendBoxFuture<'a, Result<Fetched<'a>, Error>>>,
	fetched: VecDeque<Fetched<'a>>,
	/// Bytes of first chunks the entries in the window hold or wait for.
	reserved: u64,
	budget: u64,
	/// A listing or download that failed, reported as soon as it does: the archive is lost
	/// anyway, so nothing before it is worth finishing.
	failure: Option<Error>,
}

impl<'a, B, S> Lookahead<'a, B, S>
where
	B: ZipBackend,
	S: Stream<Item = Result<Entry<'a>, Error>> + Unpin,
{
	fn new(backend: &'a B, entries: S, budget: u64) -> Self {
		Self {
			backend,
			entries: Some(entries),
			waiting: None,
			fetching: FuturesOrdered::new(),
			fetched: VecDeque::new(),
			reserved: 0,
			budget,
			failure: None,
		}
	}

	fn len(&self) -> usize {
		self.fetching.len() + self.fetched.len()
	}

	/// The next entry from `entries`, if one is ready.
	fn poll_entry(&mut self, cx: &mut Context<'_>) -> Option<Entry<'a>> {
		if let Some(entry) = self.waiting.take() {
			return Some(entry);
		}
		let entries = self.entries.as_mut()?;
		match entries.poll_next_unpin(cx) {
			Poll::Ready(Some(Ok(entry))) => Some(entry),
			Poll::Ready(Some(Err(error))) => {
				self.entries = None;
				self.fail(error);
				None
			}
			Poll::Ready(None) => {
				self.entries = None;
				None
			}
			Poll::Pending => None,
		}
	}

	fn fail(&mut self, error: Error) {
		self.failure.get_or_insert(error);
	}

	/// Fills the window and moves along the downloads in it. An idle window, with no entry being
	/// written, takes an entry whatever its first chunk weighs, so an entry heavier than the
	/// window's budget is still written.
	fn poll_fill(&mut self, cx: &mut Context<'_>, idle: bool) {
		while self.len() < MAX_WINDOW_ENTRIES {
			let Some(entry) = self.poll_entry(cx) else {
				break;
			};
			let cost = first_chunk_cost(&entry);
			let fits = self.reserved.saturating_add(cost) <= self.budget;
			let admitted = fits || (idle && self.len() == 0);
			if !admitted {
				self.waiting = Some(entry);
				break;
			}
			self.reserved = self.reserved.saturating_add(cost);
			self.fetching.push_back(fetch(self.backend, entry, cost));
		}
		while let Poll::Ready(Some(fetched)) = self.fetching.poll_next_unpin(cx) {
			match fetched {
				Ok(fetched) => self.fetched.push_back(fetched),
				Err(error) => self.fail(error),
			}
		}
	}

	/// The next entry to write, once its first chunk is in.
	async fn next(&mut self) -> Option<Result<Fetched<'a>, Error>> {
		poll_fn(|cx| {
			self.poll_fill(cx, true);
			if let Some(error) = self.failure.take() {
				return Poll::Ready(Some(Err(error)));
			}
			if let Some(fetched) = self.fetched.pop_front() {
				self.reserved = self.reserved.saturating_sub(fetched.reserved);
				return Poll::Ready(Some(Ok(fetched)));
			}
			if self.entries.is_none() && self.waiting.is_none() && self.fetching.is_empty() {
				return Poll::Ready(None);
			}
			Poll::Pending
		})
		.await
	}

	/// Runs `work` while the window keeps filling, and gives it up when something in the window
	/// fails.
	async fn alongside<T>(
		&mut self,
		work: impl Future<Output = Result<T, Error>>,
	) -> Result<T, Error> {
		let mut work = pin!(work);
		poll_fn(|cx| {
			self.poll_fill(cx, false);
			if let Some(error) = self.failure.take() {
				return Poll::Ready(Err(error));
			}
			work.as_mut().poll(cx)
		})
		.await
	}
}

fn fetch<'a, B: ZipBackend>(
	backend: &'a B,
	entry: Entry<'a>,
	reserved: u64,
) -> MaybeSendBoxFuture<'a, Result<Fetched<'a>, Error>> {
	Box::pin(async move {
		let first_chunk = match &entry {
			Entry::File {
				file,
				path: Some(_),
			} => backend.first_chunk(file).await?,
			Entry::File { path: None, .. } | Entry::Dir(_) => None,
		};
		Ok(Fetched {
			entry,
			first_chunk,
			reserved,
		})
	})
}

/// Reports an archive's progress: after each file and directory, and within a file whenever
/// another chunk's worth of it is written.
struct Progress<'a, C> {
	state: &'a Mutex<ZipState>,
	callback: Option<&'a C>,
	/// Bytes written since the last report.
	unreported: u64,
}

impl<C: ZipProgressCallback> Progress<'_, C> {
	fn state(&self) -> MutexGuard<'_, ZipState> {
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}

	fn report(&mut self) {
		self.unreported = 0;
		let Some(callback) = self.callback else {
			return;
		};
		let state = self.state().clone();
		callback(
			state.bytes_written,
			state.total_bytes,
			state.items_processed,
			state.total_items,
		);
	}

	/// Bytes written before the file about to be written.
	fn file_start(&self) -> u64 {
		self.state().bytes_written
	}

	/// `written` bytes of the `size`-byte file that started at `start` are written.
	fn file_written(&mut self, start: u64, written: u64, size: u64, delta: u64) {
		self.state().bytes_written = start.saturating_add(written.min(size));
		self.unreported = self.unreported.saturating_add(delta);
		if self.unreported >= CHUNK_SIZE_U64 {
			self.report();
		}
	}

	/// The `size`-byte file that started at `start` is done (written or skipped): it counts as
	/// its full size, so the bytes written end at the total.
	fn file_done(&mut self, start: u64, size: u64) {
		{
			let mut state = self.state();
			state.bytes_written = start.saturating_add(size);
			state.items_processed = state.items_processed.saturating_add(1);
		}
		self.report();
	}

	fn dir_done(&mut self) {
		{
			let mut state = self.state();
			state.items_processed = state.items_processed.saturating_add(1);
		}
		self.report();
	}
}

/// Writes each entry into the archive.
struct EntryWriter<'a, B, W, C> {
	backend: &'a B,
	zip: ZipFileWriter<W>,
	read_ahead: u64,
	progress: Progress<'a, C>,
}

impl<B, W, C> EntryWriter<'_, B, W, C>
where
	B: ZipBackend,
	W: AsyncWrite + Unpin,
	C: ZipProgressCallback,
{
	async fn write(&mut self, fetched: Fetched<'_>) -> Result<(), Error> {
		match fetched.entry {
			Entry::File {
				file,
				path: Some(path),
			} => self.write_file(&file, path, fetched.first_chunk).await,
			Entry::File { file, path: None } => {
				debug!("Skipping file with undecryptable metadata: {}", file.uuid());
				// still update progress so counters stay consistent
				let start = self.progress.file_start();
				self.progress.file_done(start, file.size());
				Ok(())
			}
			Entry::Dir(entry) => {
				if let Some(entry) = entry {
					self.write_dir(entry).await?;
				}
				self.progress.dir_done();
				Ok(())
			}
		}
	}

	async fn write_file(
		&mut self,
		file: &RemoteFileType<'_>,
		path: String,
		first_chunk: Option<Chunk<'_>>,
	) -> Result<(), Error> {
		let size = file.size();
		let mut builder =
			ZipEntryBuilder::new(path.into(), Compression::Stored).uncompressed_size(size);
		if let Some(modified_time) = file.last_modified() {
			builder = builder.last_modification_date(modified_time.into());
		}
		let entry = add_file_times(file, builder).build();

		let mut remainder =
			(size > CHUNK_SIZE_U64).then(|| self.backend.remainder(file, self.read_ahead));
		if let Some(remainder) = &mut remainder {
			start_downloading(remainder).await?;
		}

		let mut writer = self.zip.write_entry_stream(entry).await.map_err(|e| {
			Error::custom(ErrorKind::IO, format!("Failed to start zip entry: {}", e))
		})?;
		let start = self.progress.file_start();
		let mut written = 0u64;
		if let Some(chunk) = first_chunk {
			let data = chunk.as_ref();
			// a chunk is never longer than the file, as the reader would cut it
			let len = usize::try_from(size).map_or(data.len(), |size| size.min(data.len()));
			for piece in data.get(..len).unwrap_or(data).chunks(MAX_WRITE) {
				writer.write_all(piece).await?;
				let delta = u64::try_from(piece.len()).unwrap_or(u64::MAX);
				written = written.saturating_add(delta);
				self.progress.file_written(start, written, size, delta);
			}
			// The rest reads with this chunk's memory: waiting on the shared budget for it again,
			// behind the windows of every other download, could wait forever on memory that only
			// this file's progress frees.
			if let Some(remainder) = &mut remainder {
				remainder.add_reservation(chunk);
			}
		}
		if let Some(remainder) = &mut remainder {
			while let Some(piece) = remainder.fill_buf().await?.chunks(MAX_WRITE).next() {
				writer.write_all(piece).await?;
				let len = piece.len();
				remainder.consume_unpin(len);
				let delta = u64::try_from(len).unwrap_or(u64::MAX);
				written = written.saturating_add(delta);
				self.progress.file_written(start, written, size, delta);
			}
		}
		writer
			.close()
			.await
			.map_err(|e| Error::custom(ErrorKind::IO, e.to_string()))?;
		self.progress.file_done(start, size);
		Ok(())
	}

	async fn write_dir(&mut self, entry: DirEntry) -> Result<(), Error> {
		// this is apparently how you add a directory in async-zip
		// (you add an empty entry with a trailing slash)
		let DirEntry { mut path, created } = entry;
		path.push('/');
		let builder = ZipEntryBuilder::new(path.into(), Compression::Stored);
		let entry = add_dir_times(created, builder).build();
		self.zip
			.write_entry_whole(entry, &[])
			.await
			.map_err(|e| Error::custom(ErrorKind::IO, e.to_string()))
	}

	async fn finish(self) -> Result<W, Error> {
		let mut writer = self
			.zip
			.close()
			.await
			.map_err(|e| Error::custom(ErrorKind::IO, e.to_string()))?;
		writer.close().await?;
		Ok(writer)
	}
}

/// Polls `reader` once, so its first downloads are under way while the archive is busy with
/// what comes before them. A failure is returned now: the reader does not report it again.
async fn start_downloading(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<(), Error> {
	poll_fn(|cx| match Pin::new(&mut *reader).poll_fill_buf(cx) {
		Poll::Ready(Err(error)) => Poll::Ready(Err(Error::from(error))),
		Poll::Ready(Ok(_)) | Poll::Pending => Poll::Ready(Ok(())),
	})
	.await
}

/// Writes `entries` as a zip into `writer`, in order, and returns the writer once the archive
/// is complete and the writer closed. Every file and directory counts towards `state` as it is
/// written, reported through `callback`.
pub(super) async fn write_entries<'a, B, S, W, C>(
	backend: &'a B,
	entries: S,
	writer: W,
	state: &'a Mutex<ZipState>,
	callback: Option<&'a C>,
) -> Result<W, Error>
where
	B: ZipBackend,
	S: Stream<Item = Result<Entry<'a>, Error>> + Unpin,
	W: AsyncWrite + Unpin,
	C: ZipProgressCallback,
{
	let split = BudgetSplit::new(backend.memory_budget());
	let mut lookahead = Lookahead::new(backend, entries, split.window);
	let mut entry_writer = EntryWriter {
		backend,
		zip: ZipFileWriter::new(writer),
		read_ahead: split.read_ahead,
		progress: Progress {
			state,
			callback,
			unreported: 0,
		},
	};
	while let Some(fetched) = lookahead.next().await {
		let fetched = fetched?;
		lookahead.alongside(entry_writer.write(fetched)).await?;
	}
	entry_writer.finish().await
}

#[cfg(test)]
mod tests {
	use std::{
		io::{self, Read, Write},
		num::NonZeroU32,
		time::Duration,
	};

	use filen_types::fs::Uuid;
	use futures::{
		AsyncRead,
		future::join_all,
		io::{copy, sink},
		ready, stream,
	};

	use super::*;
	use crate::{
		auth::http::{ClientConfig, SharedClientState},
		consts::FILE_CHUNK_SIZE_EXTRA,
		fs::{
			drive_job::test_support::{chunk_data, full_chunk, stored_file},
			file::read::chunk_plaintext_len,
			zip::walk::DirEntry,
		},
	};

	const FULL_CHUNK: u64 = FULL_CHUNK_BYTES as u64;

	/// Files held in memory, downloaded through the real memory semaphore.
	struct FakeBackend {
		client: UnauthClient,
		/// The file whose first chunk fails to download.
		fail_first_chunk: Option<Uuid>,
	}

	impl FakeBackend {
		fn with_memory(chunks: usize) -> Self {
			let config = ClientConfig::default().with_memory_budget(chunks * FULL_CHUNK_BYTES);
			Self {
				client: UnauthClient::from_config(config).unwrap(),
				fail_first_chunk: None,
			}
		}
	}

	/// Chunk `index` of the `size`-byte file `uuid`, read once its memory is reserved.
	async fn read_chunk<'s>(
		state: &'s SharedClientState,
		uuid: Uuid,
		size: u64,
		index: u64,
	) -> io::Result<Chunk<'s>> {
		let plaintext = chunk_plaintext_len(size, index);
		let reserved = u32::try_from(plaintext).unwrap() + FILE_CHUNK_SIZE_EXTRA.get();
		let mut chunk = Chunk::acquire(NonZeroU32::new(reserved).unwrap(), state).await;
		// arrives on a later poll, as a download does
		tokio::task::yield_now().await;
		chunk.write_all(&chunk_data(uuid, index, size))?;
		Ok(chunk)
	}

	impl ZipBackend for FakeBackend {
		fn memory_budget(&self) -> u64 {
			self.client.memory_budget()
		}

		fn first_chunk<'s>(
			&'s self,
			file: &RemoteFileType<'_>,
		) -> impl Future<Output = Result<Option<Chunk<'s>>, Error>> + MaybeSend {
			let (uuid, size) = (file.uuid(), file.size());
			let wanted = first_chunk_size(file).is_some();
			async move {
				if self.fail_first_chunk == Some(uuid) {
					return Err(Error::custom(ErrorKind::Server, "download failed"));
				}
				if !wanted {
					return Ok(None);
				}
				Ok(Some(read_chunk(self.client.state(), uuid, size, 0).await?))
			}
		}

		fn remainder<'f>(
			&'f self,
			file: &'f RemoteFileType<'_>,
			read_ahead: u64,
		) -> impl Remainder<'f> + 'f {
			FakeReader::new(self.client.state(), file, 1, read_ahead)
		}
	}

	/// Reads a file from chunk `next` on with memory as `FileReader` takes it: chunks reserved
	/// up to the read-ahead without waiting, one more waited for while within it (or while it
	/// holds none), and each read chunk's memory reused for the next.
	struct FakeReader<'a> {
		state: &'a SharedClientState,
		uuid: Uuid,
		size: u64,
		next: u64,
		chunks: u64,
		read_ahead: u64,
		/// Memory held for chunks not fetched yet.
		reserved: Vec<Chunk<'a>>,
		/// The chunk being read, and how much of it is.
		current: Option<(Chunk<'a>, usize)>,
		acquiring: Option<MaybeSendBoxFuture<'a, Chunk<'a>>>,
	}

	impl<'a> FakeReader<'a> {
		fn new(
			state: &'a SharedClientState,
			file: &RemoteFileType<'_>,
			next: u64,
			read_ahead: u64,
		) -> Self {
			let mut reader = Self {
				state,
				uuid: file.uuid(),
				size: file.size(),
				next,
				chunks: file.size().div_ceil(CHUNK_SIZE_U64),
				read_ahead,
				reserved: Vec::new(),
				current: None,
				acquiring: None,
			};
			while reader.within_read_ahead() {
				let Some(chunk) = Chunk::try_acquire(full_chunk(), state) else {
					break;
				};
				reader.reserved.push(chunk);
			}
			reader.arm();
			reader
		}

		fn within_read_ahead(&self) -> bool {
			let held =
				u64::try_from(self.reserved.len()).unwrap() + u64::from(self.acquiring.is_some());
			held < self.chunks.saturating_sub(self.next)
				&& (held + u64::from(self.current.is_some()) + 1) * FULL_CHUNK <= self.read_ahead
		}

		/// Starts waiting for one more chunk's memory, as `FileReader` does.
		fn arm(&mut self) {
			let starved =
				self.reserved.is_empty() && self.current.is_none() && self.next < self.chunks;
			if self.acquiring.is_none() && (starved || self.within_read_ahead()) {
				self.acquiring = Some(Box::pin(Chunk::acquire(full_chunk(), self.state)));
			}
		}
	}

	impl<'a> Remainder<'a> for FakeReader<'a> {
		fn add_reservation(&mut self, reservation: Chunk<'a>) {
			if self.next < self.chunks {
				self.reserved.push(reservation);
			}
		}
	}

	impl AsyncBufRead for FakeReader<'_> {
		fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
			let this = self.get_mut();
			let current = loop {
				if let Some((chunk, read)) = this.current.take() {
					if read < chunk.len() {
						break (chunk, read);
					}
					// read out: its memory takes the next chunk
					this.add_reservation(chunk);
				}
				if let Some(acquiring) = &mut this.acquiring
					&& let Poll::Ready(chunk) = acquiring.as_mut().poll(cx)
				{
					this.acquiring = None;
					this.add_reservation(chunk);
				}
				if this.next >= this.chunks {
					this.reserved.clear();
					this.acquiring = None;
					return Poll::Ready(Ok(&[]));
				}
				if let Some(mut chunk) = this.reserved.pop() {
					AsMut::<Vec<u8>>::as_mut(&mut chunk).clear();
					chunk.write_all(&chunk_data(this.uuid, this.next, this.size))?;
					this.next += 1;
					this.current = Some((chunk, 0));
					this.arm();
					continue;
				}
				if this.acquiring.is_none() {
					this.arm();
					continue;
				}
				return Poll::Pending;
			};
			let (chunk, read) = &*this.current.insert(current);
			let data: &[u8] = <Chunk<'_> as AsRef<[u8]>>::as_ref(chunk);
			Poll::Ready(Ok(data.get(*read..).unwrap_or_default()))
		}

		fn consume(self: Pin<&mut Self>, amt: usize) {
			if let Some((_, read)) = &mut self.get_mut().current {
				*read += amt;
			}
		}
	}

	impl AsyncRead for FakeReader<'_> {
		fn poll_read(
			mut self: Pin<&mut Self>,
			cx: &mut Context<'_>,
			buf: &mut [u8],
		) -> Poll<io::Result<usize>> {
			let data = ready!(self.as_mut().poll_fill_buf(cx))?;
			let len = data.len().min(buf.len());
			buf[..len].copy_from_slice(&data[..len]);
			self.consume(len);
			Poll::Ready(Ok(len))
		}
	}

	fn stored(id: u128, name: &str, size: u64) -> RemoteFileType<'static> {
		stored_file(
			Uuid::from_u128(id),
			Uuid::nil(),
			name,
			size,
			size.div_ceil(CHUNK_SIZE_U64),
			None,
		)
	}

	fn file<'a>(id: u128, name: &str, size: u64) -> Entry<'a> {
		Entry::File {
			file: stored(id, name, size),
			path: Some(name.to_owned()),
		}
	}

	fn contents(id: u128, size: u64) -> Vec<u8> {
		(0..size.div_ceil(CHUNK_SIZE_U64))
			.flat_map(|index| chunk_data(Uuid::from_u128(id), index, size))
			.collect()
	}

	type Reports = Mutex<Vec<(u64, u64, u64, u64)>>;

	/// Writes `entries` into an archive, with totals of `total_bytes` and `total_items`.
	async fn write<'a>(
		backend: &'a FakeBackend,
		entries: Vec<Entry<'a>>,
		(total_bytes, total_items): (u64, u64),
		reports: &Reports,
	) -> Result<Vec<u8>, Error> {
		let state = Mutex::new(ZipState::new(total_bytes, total_items));
		let callback = |bw, tb, ip, ti| reports.lock().unwrap().push((bw, tb, ip, ti));
		write_entries(
			backend,
			stream::iter(entries.into_iter().map(Ok)),
			Vec::new(),
			&state,
			Some(&callback),
		)
		.await
	}

	/// Writes `entries` into an archive nobody keeps.
	async fn write_away<'a>(
		backend: &'a FakeBackend,
		entries: Vec<Entry<'a>>,
	) -> Result<(), Error> {
		let state = Mutex::new(ZipState::new(0, 0));
		write_entries(
			backend,
			stream::iter(entries.into_iter().map(Ok)),
			sink(),
			&state,
			None::<&fn(u64, u64, u64, u64)>,
		)
		.await
		.map(drop)
	}

	fn read_archive(bytes: Vec<u8>) -> Vec<(String, Vec<u8>)> {
		let mut archive = zip::ZipArchive::new(io::Cursor::new(bytes)).unwrap();
		(0..archive.len())
			.map(|index| {
				let mut entry = archive.by_index(index).unwrap();
				let mut data = Vec::new();
				// checks the entry's CRC too
				entry.read_to_end(&mut data).unwrap();
				(entry.name().to_owned(), data)
			})
			.collect()
	}

	#[tokio::test(start_paused = true)]
	async fn writes_every_entry_in_order_with_its_contents() {
		let backend = FakeBackend::with_memory(16);
		let big = 2 * CHUNK_SIZE_U64 + CHUNK_SIZE_U64 / 2;
		let entries = vec![
			file(1, "a.txt", 10),
			file(2, "big", big),
			Entry::Dir(Some(DirEntry {
				path: "d".to_owned(),
				created: None,
			})),
			file(3, "empty", 0),
			Entry::File {
				file: stored(4, "undecryptable", 5),
				path: None,
			},
			Entry::Dir(None),
		];
		let reports = Reports::default();

		let bytes = write(&backend, entries, (10 + big + 5, 6), &reports)
			.await
			.unwrap();

		assert_eq!(
			read_archive(bytes),
			[
				("a.txt".to_owned(), contents(1, 10)),
				("big".to_owned(), contents(2, big)),
				("d/".to_owned(), Vec::new()),
				("empty".to_owned(), Vec::new()),
			]
		);
		let last = *reports.lock().unwrap().last().unwrap();
		assert_eq!(last, (10 + big + 5, 10 + big + 5, 6, 6));
	}

	#[tokio::test(start_paused = true)]
	async fn reports_progress_within_a_file_and_ends_at_the_total() {
		let backend = FakeBackend::with_memory(16);
		let size = 4 * CHUNK_SIZE_U64 + CHUNK_SIZE_U64 / 2;
		let reports = Reports::default();

		write(&backend, vec![file(1, "f", size)], (size, 1), &reports)
			.await
			.unwrap();

		let reports = reports.into_inner().unwrap();
		let mib = CHUNK_SIZE_U64;
		assert_eq!(
			reports,
			[
				(mib, size, 0, 1),
				(2 * mib, size, 0, 1),
				(3 * mib, size, 0, 1),
				(4 * mib, size, 0, 1),
				(size, size, 1, 1),
			]
		);
	}

	#[tokio::test(start_paused = true)]
	async fn a_failed_download_ends_the_archive_with_its_error() {
		let mut backend = FakeBackend::with_memory(16);
		backend.fail_first_chunk = Some(Uuid::from_u128(2));
		let entries = vec![file(1, "a", 10), file(2, "b", 10), file(3, "c", 10)];

		let error = write(&backend, entries, (30, 3), &Reports::default())
			.await
			.unwrap_err();

		assert_eq!(error.kind(), ErrorKind::Server);
	}

	#[tokio::test(start_paused = true)]
	async fn a_failure_ahead_ends_the_archive_without_finishing_the_file_being_written() {
		let mut backend = FakeBackend::with_memory(16);
		backend.fail_first_chunk = Some(Uuid::from_u128(2));
		let entries = vec![file(1, "big", 20 * CHUNK_SIZE_U64), file(2, "fails", 10)];
		let reports = Reports::default();

		let error = write(&backend, entries, (0, 2), &reports)
			.await
			.unwrap_err();

		assert_eq!(error.kind(), ErrorKind::Server);
		let reports = reports.into_inner().unwrap();
		assert!(
			reports.iter().all(|&(_, _, items, _)| items == 0),
			"{reports:?}"
		);
	}

	/// Files of one, two and a half, and a fraction of a chunk, in turn.
	fn mixed_files<'a>(count: u128) -> Vec<Entry<'a>> {
		mixed_files_from(0, count)
	}

	fn mixed_files_from<'a>(first: u128, count: u128) -> Vec<Entry<'a>> {
		(first..first + count)
			.map(|i| {
				let size = match i % 3 {
					0 => CHUNK_SIZE_U64,
					1 => 2 * CHUNK_SIZE_U64 + CHUNK_SIZE_U64 / 2,
					_ => 100,
				};
				file(i, &format!("f{i}"), size)
			})
			.collect()
	}

	async fn writes_all_of(backend: FakeBackend, files: u128) {
		let entries = mixed_files(files);
		let written = tokio::time::timeout(
			Duration::from_secs(60),
			write(&backend, entries, (0, 0), &Reports::default()),
		)
		.await
		.expect("the archive is written instead of waiting on memory forever")
		.unwrap();
		assert_eq!(read_archive(written).len(), usize::try_from(files).unwrap());
	}

	/// The smallest budget a client accepts, in chunks: the HTTP provider needs half of it to
	/// hold a chunk.
	const SMALLEST_BUDGET: usize = if cfg!(feature = "http-provider") {
		2
	} else {
		1
	};

	#[tokio::test(start_paused = true)]
	async fn never_deadlocks_on_the_smallest_memory_budget() {
		writes_all_of(FakeBackend::with_memory(SMALLEST_BUDGET), 12).await;
	}

	#[tokio::test(start_paused = true)]
	async fn never_deadlocks_with_many_files_on_a_small_budget() {
		writes_all_of(FakeBackend::with_memory(3), 40).await;
	}

	/// Waits for every one of `downloads`, failing instead of hanging when memory never frees up.
	async fn all_finish<T>(downloads: impl IntoIterator<Item = impl Future<Output = T>>) -> Vec<T> {
		tokio::time::timeout(Duration::from_secs(600), join_all(downloads))
			.await
			.expect("every download finishes instead of waiting on memory forever")
	}

	#[tokio::test(start_paused = true)]
	async fn concurrent_archives_of_large_files_never_deadlock() {
		let backend = FakeBackend::with_memory(16);
		let archives = (0..3u128).map(|archive| {
			let entries = (0..30)
				.map(|i| file(archive * 100 + i, "f", 3 * CHUNK_SIZE_U64))
				.collect();
			write_away(&backend, entries)
		});

		for written in all_finish(archives).await {
			written.unwrap();
		}
	}

	#[tokio::test(start_paused = true)]
	async fn concurrent_archives_of_mixed_files_never_deadlock() {
		let backend = FakeBackend::with_memory(16);
		let archives =
			(0..4u128).map(|archive| write_away(&backend, mixed_files_from(archive * 100, 24)));

		for written in all_finish(archives).await {
			written.unwrap();
		}
	}

	#[tokio::test(start_paused = true)]
	async fn archives_beside_a_stalled_download_holding_its_read_ahead_never_deadlock() {
		let backend = FakeBackend::with_memory(16);
		let big = stored(999, "big", 40 * CHUNK_SIZE_U64);
		// its consumer reads nothing until the archives are done
		let download = FakeReader::new(backend.client.state(), &big, 0, 8 * FULL_CHUNK);
		let archives =
			(0..2u128).map(|archive| write_away(&backend, mixed_files_from(archive * 100, 24)));

		for written in all_finish(archives).await {
			written.unwrap();
		}
		copy(download, &mut sink()).await.unwrap();
	}

	#[test]
	fn splits_the_budget_in_half_but_leaves_the_file_being_written_a_full_chunk() {
		assert_eq!(
			BudgetSplit::new(16 * FULL_CHUNK),
			BudgetSplit {
				read_ahead: 8 * FULL_CHUNK,
				window: 8 * FULL_CHUNK,
			}
		);
		assert_eq!(
			BudgetSplit::new(3 * FULL_CHUNK),
			BudgetSplit {
				read_ahead: 3 * FULL_CHUNK - 3 * FULL_CHUNK / 2,
				window: 3 * FULL_CHUNK / 2,
			}
		);
		assert_eq!(
			BudgetSplit::new(FULL_CHUNK),
			BudgetSplit {
				read_ahead: FULL_CHUNK,
				window: 0,
			}
		);
	}

	/// Fills `window` once, as writing an entry (or waiting for the next one, when `idle`) does.
	async fn fill<'a>(
		window: &mut Lookahead<
			'a,
			FakeBackend,
			impl Stream<Item = Result<Entry<'a>, Error>> + Unpin,
		>,
		idle: bool,
	) {
		poll_fn(|cx| {
			window.poll_fill(cx, idle);
			Poll::Ready(())
		})
		.await;
	}

	#[tokio::test(start_paused = true)]
	async fn the_window_holds_first_chunks_up_to_its_budget() {
		let backend = FakeBackend::with_memory(16);
		let entries = stream::iter((0..5).map(|i| Ok(file(i, "f", 3 * CHUNK_SIZE_U64))));
		let mut window = Lookahead::new(&backend, entries, 2 * FULL_CHUNK);

		fill(&mut window, false).await;

		assert_eq!(window.len(), 2);
		assert_eq!(window.reserved, 2 * FULL_CHUNK);
		assert!(window.waiting.is_some());
	}

	#[tokio::test(start_paused = true)]
	async fn only_an_idle_window_takes_an_entry_heavier_than_its_budget() {
		let backend = FakeBackend::with_memory(16);
		let entries = stream::iter((0..2).map(|i| Ok(file(i, "f", 3 * CHUNK_SIZE_U64))));
		let mut window = Lookahead::new(&backend, entries, 0);

		fill(&mut window, false).await;
		assert_eq!(window.len(), 0);
		fill(&mut window, true).await;
		assert_eq!(window.len(), 1);
		fill(&mut window, true).await;
		assert_eq!(window.len(), 1);
	}
}
