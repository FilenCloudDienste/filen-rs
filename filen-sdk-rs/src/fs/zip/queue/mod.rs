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
mod tests;
