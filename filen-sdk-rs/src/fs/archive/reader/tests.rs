//! Readers against the fake drive, with the real codec on its thread.

use std::{io::Write, sync::Arc};

use filen_types::fs::Uuid;

use super::*;
use crate::{
	auth::http::ClientConfig,
	consts::{CHUNK_SIZE, CHUNK_SIZE_U64},
	fs::{
		archive::{
			extract::{ArchiveEntry, EntryDownloadUpdate, ListUpdate, SolidSkipExceeded},
			sevenz::write::{SevenZEncryption, SevenZMethod},
			test_support::{
				TarMember, archive_password, gzip, incompressible, sevenz_of, tar_of, tar_with,
			},
			zip::{
				crypto::{AesStrength, test_support::zip_crypto_encrypt},
				write::{Encryption, ZipMethod, ZipWriter},
			},
		},
		drive_job::test_support::{FakeBackend, remote_file},
	},
};

const ARCHIVE: Uuid = Uuid::from_u128(0xA);

struct Ignore;

impl ListCallback for Ignore {
	fn on_entries_batch(&self, _: Vec<ArchiveEntry>) {}
	fn on_update(&self, _: ListUpdate) {}
}

impl EntryDownloadCallback for Ignore {
	fn on_update(&self, _: EntryDownloadUpdate) {}
}

/// A zip of three stored entries of a chunk and more each: its head, each entry and its index
/// lie in chunks of their own.
fn spread_zip() -> (Vec<u8>, Vec<Vec<u8>>) {
	let data: Vec<Vec<u8>> = (1..=3)
		.map(|seed: u8| incompressible(CHUNK_SIZE + 10 * usize::from(seed), u64::from(seed)))
		.collect();
	let mut writer = ZipWriter::new(Vec::new());
	for (at, data) in data.iter().enumerate() {
		writer
			.add_file(
				&format!("{at}.bin"),
				None,
				data.len() as u64,
				ZipMethod::Stored,
				None,
				&mut &data[..],
			)
			.unwrap();
	}
	(writer.finish().unwrap(), data)
}

/// The archive in a fake drive, and the client settings readers of it run under, with a pool
/// of their own of `budget` bytes.
fn drive(
	name: &str,
	bytes: Vec<u8>,
	budget: u64,
) -> (Arc<FakeBackend>, RemoteFileType<'static>, ArchiveConfig) {
	let archive = remote_file(ARCHIVE, Uuid::from_u128(0xA0), name, &bytes, None);
	let mut backend = FakeBackend::new(Uuid::from_u128(0xD)).with_memory(8);
	backend.contents.insert(ARCHIVE, bytes);
	let config = ArchiveConfig::new(&ClientConfig::default()).with_own_pool(budget);
	(Arc::new(backend), archive, config)
}

fn reader_of(
	backend: &Arc<FakeBackend>,
	archive: &RemoteFileType<'static>,
	config: &ArchiveConfig,
	password: Option<&str>,
) -> Reader<FakeBackend> {
	Reader::new(
		Arc::clone(backend),
		archive.clone(),
		config.clone(),
		ListConfig {
			password: password.map(archive_password),
			..ListConfig::default()
		},
	)
}

/// The chunks fetched since the last call, in the order they came in.
fn fetched(backend: &FakeBackend) -> Vec<u64> {
	std::mem::take(&mut backend.log().fetched)
		.into_iter()
		.map(|(_, index)| index)
		.collect()
}

fn entry(index: u32) -> ArchiveEntryId {
	ArchiveEntryId {
		archive: ARCHIVE,
		index,
	}
}

async fn download(
	reader: &Reader<FakeBackend>,
	index: u32,
) -> Result<Vec<u8>, EntryDownloadFailed> {
	let mut written = futures::io::Cursor::new(Vec::new());
	reader
		.download_entry(
			entry(index),
			&mut written,
			None,
			Ignore,
			JobControl::default(),
		)
		.await?;
	Ok(written.into_inner())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_reads_a_zips_head_and_index_once_for_all_its_calls() {
	let (zip, data) = spread_zip();
	let last = (zip.len() as u64 - 1) / CHUNK_SIZE_U64;
	let (backend, archive, config) = drive("z.zip", zip, 64 << 20);
	let reader = reader_of(&backend, &archive, &config, None);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	assert_eq!(reader.format(), Some(ArchiveFormat::Zip));
	assert!(reader.index_held());
	assert_eq!(fetched(&backend), [0, last - 1, last]);
	for (at, data) in data.iter().enumerate() {
		let index = u32::try_from(at).unwrap();
		assert_eq!(&download(&reader, index).await.unwrap(), data, "{at}");
	}
	// the head and the index are not read again, and each chunk once: chunk 0, kept by the
	// open, gave way to the index's two
	assert_eq!(fetched(&backend), [0, 1, 2, last]);
	let listed = reader.list(Ignore, JobControl::default()).await.unwrap();
	assert_eq!(listed.totals.files, 3);
	assert_eq!(listed.format, Some(ArchiveFormat::Zip));
	assert_eq!(fetched(&backend), Vec::<u64>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_reader_holds_no_job_slot_and_none_of_the_transfer_budget() {
	let (zip, _) = spread_zip();
	let (backend, archive, config) = drive("z.zip", zip, 64 << 20);
	let memory = backend.memory().available_permits();
	let reader = reader_of(&backend, &archive, &config, None);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	download(&reader, 1).await.unwrap();
	assert!(reader.index_held());
	assert_eq!(config.free_slots(), config.job_concurrency());
	assert_eq!(backend.memory().available_permits(), memory);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_whose_index_was_given_up_reads_it_again_and_holds_it_again() {
	let (zip, data) = spread_zip();
	let last = (zip.len() as u64 - 1) / CHUNK_SIZE_U64;
	let (backend, archive, config) = drive("z.zip", zip, 64 << 20);
	let reader = reader_of(&backend, &archive, &config, None);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	let retained = reader.retained.as_ref().unwrap();
	retained.evict_index_for_test();
	assert!(!reader.index_held());
	fetched(&backend);
	assert_eq!(download(&reader, 2).await.unwrap(), data[2]);
	assert!(fetched(&backend).contains(&last));
	assert!(reader.index_held());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_with_no_budget_reads_the_index_for_every_call() {
	let (zip, data) = spread_zip();
	let last = (zip.len() as u64 - 1) / CHUNK_SIZE_U64;
	let (backend, archive, config) = drive("z.zip", zip, 0);
	let reader = reader_of(&backend, &archive, &config, None);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	assert!(!reader.index_held());
	fetched(&backend);
	assert_eq!(download(&reader, 1).await.unwrap(), data[1]);
	assert_eq!(fetched(&backend), [0, last - 1, last, 1, 2]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_reader_refuses_every_call_and_keeps_nothing() {
	let (zip, _) = spread_zip();
	let (backend, archive, config) = drive("z.zip", zip, 64 << 20);
	let reader = reader_of(&backend, &archive, &config, None);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	reader.close();
	assert!(!reader.index_held());
	let failed = download(&reader, 0).await.unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::InvalidState);
	let failed = reader
		.list(Ignore, JobControl::default())
		.await
		.unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::InvalidState);
	assert_eq!(config.retained.charged_for_test(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_open_is_a_reader_of_its_own_whose_entries_carry_the_files_uuid() {
	let (zip, _) = spread_zip();
	let (backend, archive, config) = drive("z.zip", zip, 64 << 20);
	let (a, b) = (
		reader_of(&backend, &archive, &config, None),
		reader_of(&backend, &archive, &config, None),
	);
	assert_ne!(a.id, b.id);
	a.open(Ignore, JobControl::default()).await.unwrap();
	let listed = a.list(Ignore, JobControl::default()).await.unwrap();
	assert!(
		listed
			.entries
			.iter()
			.all(|entry| entry.id.archive == ARCHIVE)
	);
	let mut written = futures::io::Cursor::new(Vec::new());
	let other = ArchiveEntryId {
		archive: Uuid::from_u128(0xB),
		index: 0,
	};
	let failed = a
		.download_entry(other, &mut written, None, Ignore, JobControl::default())
		.await
		.unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::InvalidState);
	// an entry the index does not hold
	assert_eq!(
		download(&a, 7).await.unwrap_err().error.kind(),
		ErrorKind::InvalidState
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_7z_reader_opens_its_encrypted_index_once_under_its_password() {
	let files: Vec<Vec<u8>> = (1..=2).map(|seed| incompressible(1000, seed)).collect();
	let sevenz = sevenz_of(
		&[("a.bin", Some(&files[0])), ("b.bin", Some(&files[1]))],
		SevenZMethod::Lzma2 { level: 1 },
		true,
		Some((SevenZEncryption::EntriesAndHeaders, "pw")),
	);
	let (backend, archive, config) = drive("s.7z", sevenz, 64 << 20);
	let wrong = reader_of(&backend, &archive, &config, Some("nope"));
	let failed = wrong.open(Ignore, JobControl::default()).await.unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWrongPassword);
	let reader = reader_of(&backend, &archive, &config, Some("pw"));
	reader.open(Ignore, JobControl::default()).await.unwrap();
	assert_eq!(reader.format(), Some(ArchiveFormat::SevenZ));
	assert!(reader.index_held());
	let mut written = futures::io::Cursor::new(Vec::new());
	reader
		.download_entry(
			entry(1),
			&mut written,
			Some(u64::MAX),
			Ignore,
			JobControl::default(),
		)
		.await
		.unwrap();
	assert_eq!(written.into_inner(), files[1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tar_reader_opens_on_its_head_alone() {
	let tar = tar_of(&[("a.txt", b"alpha")]);
	let (backend, archive, config) = drive("t.tar", tar, 64 << 20);
	let reader = reader_of(&backend, &archive, &config, None);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	assert_eq!(reader.format(), Some(ArchiveFormat::Tar { codec: None }));
	assert!(!reader.index_held());
	assert_eq!(fetched(&backend), [0]);
	let listed = reader.list(Ignore, JobControl::default()).await.unwrap();
	assert_eq!(listed.totals.files, 1);
	// the chunk the open read is kept
	assert_eq!(fetched(&backend), Vec::<u64>::new());
}

/// The bindings run a reader's calls on the SDK's multi-threaded runtime, which needs `Send`
/// futures.
fn _reader_futures_are_send(
	client: Arc<Client>,
	archive: RemoteFileType<'static>,
	reader: &ArchiveReader,
) {
	fn assert_send<T: Send>(_: T) {}
	assert_send(client.open_archive_reader(
		archive,
		ListConfig::default(),
		Ignore,
		JobControl::default(),
	));
	assert_send(reader.list(Ignore, JobControl::default()));
	assert_send(reader.download_entry(
		entry(0),
		&mut futures::io::sink(),
		ReaderDownloadEntryConfig::default(),
		Ignore,
		JobControl::default(),
	));
	assert_send(reader.read_entries(
		&[entry(0)],
		|_, _| Some(futures::io::sink()),
		None,
		Ignore,
		JobControl::default(),
	));
	assert_send(reader.entry_byte_access(entry(0)));
	assert_send(reader.read_entry_range(entry(0), 0, 1, &mut futures::io::sink()));
}

/// A zip of `files`, each under its method and, when given, AES under its password.
fn zip_with(files: &[(&str, &[u8], ZipMethod, Option<&str>)]) -> Vec<u8> {
	let mut writer = ZipWriter::new(Vec::new());
	for &(path, data, method, password) in files {
		let password = password.map(archive_password);
		let encryption = password.as_ref().map(|password| Encryption {
			password,
			strength: AesStrength::Aes256,
		});
		writer
			.add_file(
				path,
				None,
				data.len() as u64,
				method,
				encryption,
				&mut &data[..],
			)
			.unwrap();
	}
	writer.add_dir("dir", None).unwrap();
	writer.finish().unwrap()
}

async fn opened(
	name: &str,
	bytes: Vec<u8>,
	password: Option<&str>,
) -> (Arc<FakeBackend>, Reader<FakeBackend>) {
	let (backend, archive, config) = drive(name, bytes, 64 << 20);
	let reader = reader_of(&backend, &archive, &config, password);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	(backend, reader)
}

async fn range(
	reader: &Reader<FakeBackend>,
	index: u32,
	start: u64,
	end: u64,
) -> Result<Vec<u8>, Arc<Error>> {
	let mut written = futures::io::Cursor::new(Vec::new());
	reader
		.read_entry_range(entry(index), start, end, &mut written)
		.await?;
	Ok(written.into_inner())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zips_entries_are_mapped_only_when_stored_as_they_are() {
	let data = incompressible(5000, 1);
	let zip = zip_with(&[
		("stored", &data, ZipMethod::Stored, None),
		("deflated", &data, ZipMethod::Deflate { level: 6 }, None),
		("stored-aes", &data, ZipMethod::Stored, Some("pw")),
		("../escapes", &data, ZipMethod::Stored, None),
	]);
	let (_, reader) = opened("z.zip", zip, Some("pw")).await;
	let access = |index| reader.entry_byte_access(entry(index));
	assert_eq!(
		access(0).await.unwrap(),
		EntryByteAccess::Mapped { size: 5000 }
	);
	assert_eq!(
		access(1).await.unwrap(),
		EntryByteAccess::Seekable { size: 5000 }
	);
	assert_eq!(
		access(2).await.unwrap(),
		EntryByteAccess::Seekable { size: 5000 }
	);
	assert_eq!(access(3).await.unwrap_err().kind(), ErrorKind::InvalidState);
	assert_eq!(access(4).await.unwrap_err().kind(), ErrorKind::InvalidState);
	assert_eq!(access(9).await.unwrap_err().kind(), ErrorKind::InvalidState);
	assert_eq!(range(&reader, 0, 0, 5000).await.unwrap(), data);
	assert_eq!(range(&reader, 1, 0, 5000).await.unwrap(), data);
	assert_eq!(range(&reader, 2, 10, 20).await.unwrap(), data[10..20]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mapped_range_fetches_only_the_chunks_it_lies_in() {
	let (zip, data) = spread_zip();
	let (backend, reader) = opened("z.zip", zip, None).await;
	fetched(&backend);
	// entry 1 lies in chunks 1 and 2: its local header too
	let len = data[1].len() as u64;
	let at = CHUNK_SIZE_U64;
	assert_eq!(
		reader.entry_byte_access(entry(1)).await.unwrap(),
		EntryByteAccess::Mapped { size: len }
	);
	assert_eq!(fetched(&backend), [1]);
	for (start, end) in [
		(0, 1),
		(len - 1, len),
		(0, len),
		(100, 100),
		(at / 2, at / 2 + 3),
	] {
		let start_at = usize::try_from(start).unwrap();
		let end_at = usize::try_from(end).unwrap();
		assert_eq!(
			range(&reader, 1, start, end).await.unwrap(),
			data[1][start_at..end_at],
			"{start}..{end}"
		);
	}
	// chunk 2 once, chunk 1 kept from the header
	assert_eq!(fetched(&backend), [2]);
	assert_eq!(
		range(&reader, 1, 0, len + 1).await.unwrap_err().kind(),
		ErrorKind::InvalidState
	);
	assert_eq!(
		range(&reader, 1, 5, 4).await.unwrap_err().kind(),
		ErrorKind::InvalidState
	);
	// resolved once
	reader.entry_byte_access(entry(1)).await.unwrap();
	assert_eq!(fetched(&backend), Vec::<u64>::new());
	reader.close();
	assert_eq!(
		reader.entry_byte_access(entry(1)).await.unwrap_err().kind(),
		ErrorKind::InvalidState
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_7z_entry_is_mapped_when_its_folder_only_copies() {
	let (a, b) = (incompressible(3000, 1), incompressible(4000, 2));
	let files = [("a", Some(&a[..])), ("b", Some(&b[..]))];
	for solid in [false, true] {
		let copied = sevenz_of(&files, SevenZMethod::Copy, solid, None);
		let (_, reader) = opened("c.7z", copied, None).await;
		assert_eq!(
			reader.entry_byte_access(entry(1)).await.unwrap(),
			EntryByteAccess::Mapped { size: 4000 },
			"solid {solid}"
		);
		assert_eq!(range(&reader, 1, 10, 4000).await.unwrap(), b[10..]);
		assert_eq!(range(&reader, 0, 0, 3000).await.unwrap(), a);
	}
	let compressed = sevenz_of(&files, SevenZMethod::Lzma2 { level: 1 }, false, None);
	let (_, reader) = opened("l.7z", compressed, None).await;
	assert_eq!(
		reader.entry_byte_access(entry(1)).await.unwrap(),
		EntryByteAccess::Whole { size: 4000 }
	);
	let encrypted = sevenz_of(
		&files,
		SevenZMethod::Copy,
		false,
		Some((SevenZEncryption::Entries, "pw")),
	);
	let (_, reader) = opened("e.7z", encrypted, Some("pw")).await;
	assert_eq!(
		reader.entry_byte_access(entry(0)).await.unwrap(),
		EntryByteAccess::Whole { size: 3000 }
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_reads_the_index_again_to_resolve_an_entry() {
	let (zip, data) = spread_zip();
	let last = (zip.len() as u64 - 1) / CHUNK_SIZE_U64;
	let (backend, reader) = opened("z.zip", zip, None).await;
	reader.retained.as_ref().unwrap().evict_index_for_test();
	fetched(&backend);
	assert_eq!(range(&reader, 2, 0, 5).await.unwrap(), data[2][..5]);
	assert!(fetched(&backend).contains(&last));
	assert!(reader.index_held());
}

/// A tar of a small file, one of three chunks, a directory and a small file after it: the last
/// header lies three chunks after the first.
fn spread_tar() -> (Vec<u8>, Vec<u8>) {
	let big = incompressible(3 * CHUNK_SIZE, 7);
	let tar = tar_with(&[
		TarMember::Data("a.txt", b"alpha"),
		TarMember::Data("big.bin", &big),
		TarMember::Data("dir/", b""),
		TarMember::Data("dir/c.txt", b"gamma"),
	]);
	(tar, big)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tar_readers_listing_seeks_past_member_data() {
	let (tar, big) = spread_tar();
	let last = (tar.len() as u64 - 1) / CHUNK_SIZE_U64;
	let (backend, archive, config) = drive("t.tar", tar, 64 << 20);
	let read = Reader::once(
		Arc::clone(&backend),
		archive.clone(),
		config.clone(),
		ListConfig::default(),
	)
	.list(Ignore, JobControl::default())
	.await
	.unwrap();
	assert_eq!(fetched(&backend), (0..=last).collect::<Vec<_>>());
	let reader = reader_of(&backend, &archive, &config, None);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	let sought = reader.list(Ignore, JobControl::default()).await.unwrap();
	// the head, kept by the open, and the chunk of the headers after the big member
	assert_eq!(fetched(&backend), [0, last]);
	assert_eq!(sought.entries, read.entries);
	assert_eq!(sought.totals, read.totals);
	assert_eq!(sought.unaccounted_bytes, read.unaccounted_bytes);
	assert!(reader.index_held());

	assert_eq!(
		reader.entry_byte_access(entry(1)).await.unwrap(),
		EntryByteAccess::Mapped {
			size: big.len() as u64
		}
	);
	let len = big.len() as u64;
	assert_eq!(
		range(&reader, 1, len - 10, len).await.unwrap(),
		big[big.len() - 10..]
	);
	assert_eq!(range(&reader, 3, 0, 5).await.unwrap(), b"gamma");
	assert_eq!(
		reader.entry_byte_access(entry(2)).await.unwrap_err().kind(),
		ErrorKind::InvalidState
	);
	// a member is downloaded as a range of the archive: a tar's members have no checksum
	let mut written = futures::io::Cursor::new(Vec::new());
	let report = reader
		.download_entry(entry(0), &mut written, None, Ignore, JobControl::default())
		.await
		.unwrap();
	assert_eq!(written.into_inner(), b"alpha");
	assert!(!report.checked);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gzip_tars_members_are_seekable_at_their_listed_sizes() {
	let tar = tar_with(&[TarMember::Data("a.txt", b"alpha")]);
	let (_, reader) = opened("t.tar.gz", gzip(&tar), None).await;
	assert_eq!(
		reader.entry_byte_access(entry(0)).await.unwrap(),
		EntryByteAccess::Seekable { size: 5 }
	);
	assert_eq!(range(&reader, 0, 1, 5).await.unwrap(), b"lpha");
}

/// What a pass wrote, by entry, in the order the writers were asked for.
type Written = Arc<Mutex<Vec<(u32, Option<u64>, Arc<Mutex<Vec<u8>>>)>>>;

/// A writer recording into its buffer.
struct Recording(Arc<Mutex<Vec<u8>>>);

impl AsyncWrite for Recording {
	fn poll_write(
		self: std::pin::Pin<&mut Self>,
		_: &mut std::task::Context<'_>,
		buf: &[u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		self.0.lock().unwrap().extend_from_slice(buf);
		std::task::Poll::Ready(Ok(buf.len()))
	}

	fn poll_flush(
		self: std::pin::Pin<&mut Self>,
		_: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		std::task::Poll::Ready(Ok(()))
	}

	fn poll_close(
		self: std::pin::Pin<&mut Self>,
		_: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		std::task::Poll::Ready(Ok(()))
	}
}

async fn read_entries(
	reader: &Reader<FakeBackend>,
	indexes: &[u32],
	max_solid_skip: Option<u64>,
) -> (
	Result<EntriesReadReport, EntriesReadFailed>,
	Vec<(u32, Option<u64>, Vec<u8>)>,
) {
	let written: Written = Arc::default();
	let into = Arc::clone(&written);
	let entries: Vec<ArchiveEntryId> = indexes.iter().map(|&index| entry(index)).collect();
	let result = reader
		.read_entries(
			&entries,
			move |entry: ArchiveEntryId, size| {
				let buffer = Arc::default();
				into.lock()
					.unwrap()
					.push((entry.index, size, Arc::clone(&buffer)));
				Some(Recording(buffer))
			},
			max_solid_skip,
			Ignore,
			JobControl::default(),
		)
		.await;
	let written = written
		.lock()
		.unwrap()
		.iter()
		.map(|(index, size, data)| (*index, *size, data.lock().unwrap().clone()))
		.collect();
	(result, written)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_writes_the_entries_asked_for_in_archive_order_each_checked() {
	let (zip, data) = spread_zip();
	let (_, reader) = opened("z.zip", zip, None).await;
	let (result, written) = read_entries(&reader, &[2, 0], Some(0)).await;
	let report = result.unwrap();
	assert_eq!(report.entries, 2);
	assert!(report.checked);
	assert_eq!(
		written,
		[
			(0, Some(data[0].len() as u64), data[0].clone()),
			(2, Some(data[2].len() as u64), data[2].clone()),
		]
	);
	assert_eq!(reader.config.free_slots(), reader.config.job_concurrency());
	// a directory is no file to read
	let zip = zip_with(&[("a", b"alpha", ZipMethod::Stored, None)]);
	let (_, reader) = opened("d.zip", zip, None).await;
	let (result, _) = read_entries(&reader, &[1], None).await;
	assert_eq!(result.unwrap_err().error.kind(), ErrorKind::InvalidState);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_over_a_compressed_tar_ends_at_its_last_entry() {
	let tail = incompressible(4 * CHUNK_SIZE, 3);
	let tar = tar_with(&[
		TarMember::Data("a.txt", b"alpha"),
		TarMember::Data("b.txt", b"beta"),
		TarMember::Data("tail.bin", &tail),
	]);
	let gz = gzip(&tar);
	let len = gz.len() as u64;
	let (_, reader) = opened("t.tar.gz", gz, None).await;
	let (result, written) = read_entries(&reader, &[1], None).await;
	let report = result.unwrap();
	assert_eq!(written, [(1, Some(4), b"beta".to_vec())]);
	// the stream's own check, at its end, is not reached
	assert!(!report.checked);
	assert!(report.bytes_read < len, "{} of {len}", report.bytes_read);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_is_refused_when_its_solid_block_throws_away_more_than_allowed() {
	let files: Vec<Vec<u8>> = (1..=3).map(|seed| incompressible(1000, seed)).collect();
	let entries: Vec<(&str, Option<&[u8]>)> = ["a", "b", "c"]
		.iter()
		.zip(&files)
		.map(|(path, data)| (*path, Some(&data[..])))
		.collect();
	let sevenz = sevenz_of(&entries, SevenZMethod::Lzma2 { level: 1 }, true, None);
	let (_, reader) = opened("s.7z", sevenz, None).await;
	// a is decoded and thrown away to reach b, and c goes on from b
	let (result, _) = read_entries(&reader, &[1, 2], Some(999)).await;
	let error = result.unwrap_err().error;
	assert_eq!(error.kind(), ErrorKind::ArchiveSolidSkipExceeded);
	let exceeded = error.downcast_ref::<SolidSkipExceeded>().unwrap();
	assert_eq!(exceeded.skipped_bytes, 1000);
	let (result, written) = read_entries(&reader, &[2, 1], Some(1000)).await;
	assert_eq!(result.unwrap().entries, 2);
	assert_eq!(
		written,
		[
			(1, Some(1000), files[1].clone()),
			(2, Some(1000), files[2].clone()),
		]
	);
	// a and c: b is thrown away too
	let (result, _) = read_entries(&reader, &[0, 2], Some(999)).await;
	assert_eq!(
		result.unwrap_err().error.kind(),
		ErrorKind::ArchiveSolidSkipExceeded
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bare_tars_members_are_read_as_ranges_with_no_job() {
	let (tar, big) = spread_tar();
	let (backend, archive, config) = drive("t.tar", tar, 64 << 20);
	let reader = reader_of(&backend, &archive, &config, None);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	reader.list(Ignore, JobControl::default()).await.unwrap();
	fetched(&backend);
	let (result, written) = read_entries(&reader, &[3, 0], None).await;
	let report = result.unwrap();
	assert_eq!(report.entries, 2);
	assert!(!report.checked);
	assert_eq!(
		written,
		[
			(0, Some(5), b"alpha".to_vec()),
			(3, Some(5), b"gamma".to_vec())
		]
	);
	// both lie in chunks kept from the listing
	assert_eq!(fetched(&backend), Vec::<u64>::new());
	let (result, written) = read_entries(&reader, &[1], None).await;
	result.unwrap();
	assert_eq!(written[0].2, big);
}

/// Bytes deflate about halves: letters of a 16-letter alphabet, by xorshift from `seed`.
fn text(len: usize, seed: u64) -> Vec<u8> {
	incompressible(len, seed)
		.into_iter()
		.map(|byte| b'a' + (byte & 0x0F))
		.collect()
}

/// Ranges of an entry of `len` bytes to check: its ends, around chunk and window boundaries,
/// and some by xorshift from `seed`.
fn ranges(len: u64, seed: u64) -> Vec<(u64, u64)> {
	let mut ranges = vec![(0, 1), (len - 1, len), (0, len), (len / 2, len / 2)];
	for at in [CHUNK_SIZE_U64, 32 << 10, 4 << 20] {
		if at + 10 <= len {
			ranges.push((at - 10, at + 10));
		}
	}
	let mut state = seed;
	let mut next = || {
		state ^= state << 13;
		state ^= state >> 7;
		state ^= state << 17;
		state
	};
	for _ in 0..12 {
		let start = next() % len;
		let end = (start + next() % (len - start + 1)).min(len);
		ranges.push((start, end));
	}
	ranges
}

async fn check_ranges(reader: &Reader<FakeBackend>, index: u32, data: &[u8], seed: u64) {
	assert_eq!(
		reader.entry_byte_access(entry(index)).await.unwrap(),
		EntryByteAccess::Seekable {
			size: data.len() as u64
		}
	);
	for (start, end) in ranges(data.len() as u64, seed) {
		let (from, to) = (
			usize::try_from(start).unwrap(),
			usize::try_from(end).unwrap(),
		);
		let read = range(reader, index, start, end).await.unwrap();
		assert!(read == data[from..to], "{start}..{end}");
	}
}

/// A zip of `files`, each ZipCrypto-encrypted under `password`, stored or deflated, built by
/// hand: no writer the tests have writes ZipCrypto.
fn zip_crypto_zip(files: &[(&str, &[u8], bool)], password: &[u8]) -> Vec<u8> {
	let mut zip = Vec::new();
	let mut central = Vec::new();
	for (name, data, deflate) in files {
		let crc = crc32fast::hash(data);
		let packed = if *deflate {
			let mut encoder =
				flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
			encoder.write_all(data).unwrap();
			encoder.finish().unwrap()
		} else {
			data.to_vec()
		};
		let stored = zip_crypto_encrypt(password, (crc >> 24) as u8, &packed);
		let offset = u32::try_from(zip.len()).unwrap();
		let method: u16 = if *deflate { 8 } else { 0 };
		let common = [
			&20u16.to_le_bytes()[..],
			&1u16.to_le_bytes(),
			&method.to_le_bytes(),
			&0u16.to_le_bytes(),
			&0x21u16.to_le_bytes(),
			&crc.to_le_bytes(),
			&u32::try_from(stored.len()).unwrap().to_le_bytes(),
			&u32::try_from(data.len()).unwrap().to_le_bytes(),
			&u16::try_from(name.len()).unwrap().to_le_bytes(),
			&0u16.to_le_bytes(),
		]
		.concat();
		zip.extend(0x0403_4b50u32.to_le_bytes());
		zip.extend(&common);
		zip.extend(name.as_bytes());
		zip.extend(&stored);
		central.extend(0x0201_4b50u32.to_le_bytes());
		central.extend(20u16.to_le_bytes());
		central.extend(&common);
		central.extend([0u8; 10]);
		central.extend(offset.to_le_bytes());
		central.extend(name.as_bytes());
	}
	let central_at = u32::try_from(zip.len()).unwrap();
	let count = u16::try_from(files.len()).unwrap().to_le_bytes();
	zip.extend(&central);
	zip.extend(0x0605_4b50u32.to_le_bytes());
	zip.extend([0u8; 4]);
	zip.extend(count);
	zip.extend(count);
	zip.extend(u32::try_from(central.len()).unwrap().to_le_bytes());
	zip.extend(central_at.to_le_bytes());
	zip.extend([0u8; 2]);
	zip
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deflated_zip_entry_reads_any_range_as_a_full_decode_does() {
	let data = text(9 << 20, 5);
	let zip = zip_with(&[
		(
			"small",
			b"hello hello hello",
			ZipMethod::Deflate { level: 6 },
			None,
		),
		("big", &data, ZipMethod::Deflate { level: 6 }, None),
	]);
	let (_, reader) = opened("z.zip", zip, None).await;
	check_ranges(&reader, 1, &data, 11).await;
	assert_eq!(range(&reader, 0, 6, 11).await.unwrap(), b"hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encrypted_zip_entries_read_by_range_under_their_password() {
	let data = text(3 << 20, 6);
	let zip = zip_with(&[
		(
			"deflated",
			&data,
			ZipMethod::Deflate { level: 6 },
			Some("pw"),
		),
		("stored", &data, ZipMethod::Stored, Some("pw")),
	]);
	let (_, reader) = opened("aes.zip", zip.clone(), Some("pw")).await;
	check_ranges(&reader, 0, &data, 21).await;
	check_ranges(&reader, 1, &data, 22).await;
	let (_, wrong) = opened("aes.zip", zip, Some("nope")).await;
	assert_eq!(
		range(&wrong, 0, 0, 1).await.unwrap_err().kind(),
		ErrorKind::ArchiveWrongPassword
	);
	let crypto = zip_crypto_zip(
		&[("deflated", &data, true), ("stored", &data, false)],
		b"pw",
	);
	let (_, reader) = opened("zc.zip", crypto, Some("pw")).await;
	check_ranges(&reader, 0, &data, 31).await;
	check_ranges(&reader, 1, &data, 32).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reading_on_in_windows_decodes_the_entry_about_once() {
	let data = text(12 << 20, 7);
	let zip = zip_with(&[("big", &data, ZipMethod::Deflate { level: 6 }, None)]);
	let (_, reader) = opened("z.zip", zip, None).await;
	let len = data.len() as u64;
	let window = 300 << 10;
	let mut at = 0;
	while at < len {
		let end = (at + window).min(len);
		let read = range(&reader, 0, at, end).await.unwrap();
		assert!(read == data[usize::try_from(at).unwrap()..usize::try_from(end).unwrap()]);
		at = end;
	}
	let retained = reader.retained.as_ref().unwrap();
	let seeks = retained.take_seeks(StreamKey::Entry(0)).unwrap();
	assert_eq!(seeks.decoded_for_test(), len);
	// checkpoints were laid on the way, every 4 MiB at the first call boundary past it
	let laid = seeks.checkpoints_for_test();
	assert_eq!(laid.len(), 2, "{laid:?}");
	for (at, laid) in laid.iter().enumerate() {
		let spacing = (at as u64 + 1) * (4 << 20);
		assert!((spacing..spacing + (64 << 10)).contains(laid), "{laid}");
	}
	retained.keep_seeks(StreamKey::Entry(0), seeks);
	// a jump back starts at the checkpoint before it
	let start = (8 << 20) + 5;
	range(&reader, 0, start, start + 10).await.unwrap();
	let seeks = retained.take_seeks(StreamKey::Entry(0)).unwrap();
	assert!(seeks.decoded_for_test() < len + (1 << 20));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoints_are_thinned_once_they_take_too_much() {
	let data = text(3 << 20, 8);
	let zip = zip_with(&[("big", &data, ZipMethod::Deflate { level: 6 }, None)]);
	let (_, reader) = opened("z.zip", zip, None).await;
	let retained = reader.retained.as_ref().unwrap();
	retained.keep_seeks(
		StreamKey::Entry(0),
		Seeks::with_limits(64 << 10, 4 * inflate::CURSOR_BYTES),
	);
	let len = data.len() as u64;
	range(&reader, 0, len - 1, len).await.unwrap();
	let seeks = retained.take_seeks(StreamKey::Entry(0)).unwrap();
	let laid = seeks.checkpoints_for_test();
	assert!(laid.len() <= 4, "{laid:?}");
	assert!(laid.len() >= 2, "{laid:?}");
	assert!(
		laid.windows(2).all(|pair| pair[1] - pair[0] >= 1 << 20),
		"{laid:?}"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_reads_of_one_entry_at_once_both_get_their_bytes() {
	let data = text(4 << 20, 9);
	let zip = zip_with(&[("big", &data, ZipMethod::Deflate { level: 6 }, None)]);
	let (_, reader) = opened("z.zip", zip, None).await;
	let (a, b) = tokio::join!(
		range(&reader, 0, 1 << 20, 2 << 20),
		range(&reader, 0, 3 << 20, 4 << 20)
	);
	assert!(a.unwrap() == data[1 << 20..2 << 20]);
	assert!(b.unwrap() == data[3 << 20..4 << 20]);
}

/// A gzip member of `data`, its header carrying every optional field.
fn gzip_member(data: &[u8]) -> Vec<u8> {
	let mut member = vec![0x1F, 0x8B, 8, 2 | 4 | 8 | 16, 0, 0, 0, 0, 0, 3];
	member.extend(3u16.to_le_bytes());
	member.extend(b"xyz");
	member.extend(b"name.tar\0");
	member.extend(b"a comment\0");
	// the header's CRC-16: the low half of its CRC-32
	let header_crc = crc32fast::hash(&member).to_le_bytes();
	member.extend(&header_crc[..2]);
	let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
	encoder.write_all(data).unwrap();
	member.extend(encoder.finish().unwrap());
	member.extend(crc32fast::hash(data).to_le_bytes());
	member.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
	member
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gzip_tars_members_read_by_range_across_its_members() {
	let big = text(3 << 20, 10);
	let tar = tar_with(&[
		TarMember::Data("a.txt", b"alpha"),
		TarMember::Data("big.bin", &big),
		TarMember::Data("c.txt", b"gamma"),
	]);
	// two members, split inside the big file, and padding after them
	let split = 2 << 20;
	let mut gz = gzip_member(&tar[..split]);
	gz.extend(gzip_member(&tar[split..]));
	gz.extend([0u8; 16]);
	let (_, reader) = opened("t.tar.gz", gz, None).await;
	check_ranges(&reader, 1, &big, 41).await;
	assert_eq!(range(&reader, 2, 0, 5).await.unwrap(), b"gamma");
	assert_eq!(range(&reader, 0, 0, 5).await.unwrap(), b"alpha");
}
