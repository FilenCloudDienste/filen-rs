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
		let starved = self.reserved.is_empty() && self.current.is_none() && self.next < self.chunks;
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
async fn write_away<'a>(backend: &'a FakeBackend, entries: Vec<Entry<'a>>) -> Result<(), Error> {
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
	window: &mut Lookahead<'a, FakeBackend, impl Stream<Item = Result<Entry<'a>, Error>> + Unpin>,
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
