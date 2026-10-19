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

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod chunk_by_chunk {
	use std::task::{Context, Poll, Waker};

	use tokio::sync::SemaphorePermit;

	use super::*;

	/// A reader over `start..end` of `file` that fetches nothing, as the whole memory budget is
	/// held, with the bytes 0 to 19 queued as its next chunk: downloaded, decrypted and placed at
	/// `start`, as a fetch leaves it.
	fn reader_with_a_queued_chunk<'a>(
		client: &'a UnauthClient,
		file: &'a FakeFile,
		start: u64,
		end: u64,
	) -> (FileReader<'a>, SemaphorePermit<'a>) {
		let mut chunk = Chunk::try_acquire(NonZeroU32::MIN, client.state()).unwrap();
		AsMut::<Vec<u8>>::as_mut(&mut chunk).extend(0..20);
		let budget = client.state().memory_semaphore();
		let held = budget
			.try_acquire_many(u32::try_from(budget.available_permits()).unwrap())
			.unwrap();
		let mut reader = FileReaderBuilder::new(client, file)
			.with_start(start)
			.with_end(end)
			.build();
		let mut cursor = Cursor::new(chunk);
		cursor.set_position(start % CHUNK_SIZE_U64);
		reader
			.futures
			.push_back(Box::pin(std::future::ready(Ok::<_, Error>(cursor))));
		reader.next_chunk_idx += 1;
		(reader, held)
	}

	#[test]
	fn a_chunk_read_from_a_range_holds_only_the_ranged_bytes() {
		let client = test_client();
		let file = FakeFile::new(20, 1);
		let (mut reader, _held) = reader_with_a_queued_chunk(&client, &file, 3, 10);
		let mut cx = Context::from_waker(Waker::noop());

		let Poll::Ready(Ok(Some(chunk))) = reader.poll_next_chunk(&mut cx) else {
			panic!("the queued chunk is ready");
		};
		assert_eq!(chunk.as_ref(), [3, 4, 5, 6, 7, 8, 9]);
		reader.recycle_chunk(chunk);

		assert!(matches!(
			reader.poll_next_chunk(&mut cx),
			Poll::Ready(Ok(None))
		));
	}

	#[test]
	fn a_chunk_dropped_instead_of_handed_back_does_not_end_the_read() {
		let client = test_client();
		let file = FakeFile::new(CHUNK_SIZE_U64 + 20, 2);
		let (mut reader, _held) = reader_with_a_queued_chunk(&client, &file, 0, file.size);
		// as after the read-ahead budget declined to queue the next acquire
		reader.allocate_chunk_future = None;
		let mut cx = Context::from_waker(Waker::noop());
		let Poll::Ready(Ok(Some(chunk))) = reader.poll_next_chunk(&mut cx) else {
			panic!("the queued chunk is ready");
		};

		drop(chunk);

		assert!(
			reader.poll_next_chunk(&mut cx).is_pending(),
			"the second chunk waits for memory"
		);
	}
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
