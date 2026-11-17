//! Readers against the fake drive, with the real codec on its thread.

use std::{io::Write, sync::Arc};

use filen_types::fs::Uuid;

use super::{nested::nested_id, *};
use crate::thumbnail::test_support::{PREVIEW_AT, jpeg_bytes, raw_with};
use crate::{
	auth::http::ClientConfig,
	consts::{CHUNK_SIZE, CHUNK_SIZE_U64},
	fs::{
		archive::{
			extract::{
				ArchiveEntry, EntryDownloadUpdate, ExpansionLimit, ListUpdate, SolidSkipExceeded,
			},
			sevenz::write::{SevenZEncryption, SevenZMethod},
			test_support::{
				TarMember, archive_password, gzip, incompressible, rar_of, rar5_header_flood,
				sevenz_of, tar_of, tar_with,
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
) -> Reader {
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

async fn download(reader: &Reader, index: u32) -> Result<Vec<u8>, EntryDownloadFailed> {
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
	unauth: &UnauthClient,
	archive: RemoteFileType<'static>,
	reader: &ArchiveReader,
) {
	fn assert_send<T: Send>(_: T) {}
	assert_send(client.open_archive_reader(
		archive.clone(),
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
		|_, _| std::future::ready(Ok(Some(futures::io::sink()))),
		None,
		Ignore,
		JobControl::default(),
	));
	assert_send(reader.entry_byte_access(entry(0), JobControl::default()));
	assert_send(reader.read_entry_range(
		entry(0),
		0,
		1,
		&mut futures::io::sink(),
		JobControl::default(),
	));
	assert_send(unauth.open_archive_reader(
		archive,
		ListConfig::default(),
		Ignore,
		JobControl::default(),
	));
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

async fn opened(name: &str, bytes: Vec<u8>, password: Option<&str>) -> (Arc<FakeBackend>, Reader) {
	let (backend, archive, config) = drive(name, bytes, 64 << 20);
	let reader = reader_of(&backend, &archive, &config, password);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	(backend, reader)
}

async fn range(reader: &Reader, index: u32, start: u64, end: u64) -> Result<Vec<u8>, Arc<Error>> {
	let mut written = futures::io::Cursor::new(Vec::new());
	reader
		.read_entry_range(
			entry(index),
			start,
			end,
			&mut written,
			&JobControl::default(),
		)
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
	let control = JobControl::default();
	let access = |index| reader.entry_byte_access(entry(index), &control);
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
		reader
			.entry_byte_access(entry(1), &JobControl::default())
			.await
			.unwrap(),
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
	reader
		.entry_byte_access(entry(1), &JobControl::default())
		.await
		.unwrap();
	assert_eq!(fetched(&backend), Vec::<u64>::new());
	reader.close();
	assert_eq!(
		reader
			.entry_byte_access(entry(1), &JobControl::default())
			.await
			.unwrap_err()
			.kind(),
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
			reader
				.entry_byte_access(entry(1), &JobControl::default())
				.await
				.unwrap(),
			EntryByteAccess::Mapped { size: 4000 },
			"solid {solid}"
		);
		assert_eq!(range(&reader, 1, 10, 4000).await.unwrap(), b[10..]);
		assert_eq!(range(&reader, 0, 0, 3000).await.unwrap(), a);
	}
	let compressed = sevenz_of(&files, SevenZMethod::Lzma2 { level: 1 }, false, None);
	let (_, reader) = opened("l.7z", compressed, None).await;
	assert_eq!(
		reader
			.entry_byte_access(entry(1), &JobControl::default())
			.await
			.unwrap(),
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
		reader
			.entry_byte_access(entry(0), &JobControl::default())
			.await
			.unwrap(),
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
		reader
			.entry_byte_access(entry(1), &JobControl::default())
			.await
			.unwrap(),
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
		reader
			.entry_byte_access(entry(2), &JobControl::default())
			.await
			.unwrap_err()
			.kind(),
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
		reader
			.entry_byte_access(entry(0), &JobControl::default())
			.await
			.unwrap(),
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
	reader: &Reader,
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
				std::future::ready(Ok(Some(Recording(buffer))))
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
async fn a_solid_rar_is_held_to_what_a_pass_may_throw_away() {
	let files: Vec<Vec<u8>> = (0..3).map(|at| text(1000, 90 + at)).collect();
	let named: Vec<(&str, &[u8])> = ["a", "b", "c"]
		.into_iter()
		.zip(&files)
		.map(|(name, data)| (name, &data[..]))
		.collect();
	let rar = rar_of(rars::ArchiveVersion::Rar50, &named, false, true, None);
	let (_, reader) = opened("s.rar", rar, None).await;
	// a is decoded and thrown away to reach b, and c goes on from b
	let (result, _) = read_entries(&reader, &[1, 2], Some(999)).await;
	let error = result.unwrap_err().error;
	assert_eq!(error.kind(), ErrorKind::ArchiveSolidSkipExceeded);
	assert_eq!(
		error
			.downcast_ref::<SolidSkipExceeded>()
			.unwrap()
			.skipped_bytes,
		1000
	);
	let (result, written) = read_entries(&reader, &[2, 1], Some(1000)).await;
	assert_eq!(result.unwrap().entries, 2);
	assert_eq!(written[1].2, files[2]);
	// the first decodes alone; a download's default allows no skip
	assert_eq!(downloaded(&reader, 0).await.unwrap().0, files[0]);
	let mut sink = futures::io::sink();
	let refused = reader
		.download_entry(
			entry(2),
			&mut sink,
			ReaderDownloadEntryConfig::default().max_solid_skip,
			Ignore,
			JobControl::default(),
		)
		.await
		.unwrap_err();
	assert_eq!(refused.error.kind(), ErrorKind::ArchiveSolidSkipExceeded);
	assert_eq!(
		refused
			.error
			.downcast_ref::<SolidSkipExceeded>()
			.unwrap()
			.skipped_bytes,
		2000
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compressed_tar_not_listed_is_held_to_what_a_pass_may_throw_away() {
	let (big, small) = (text(300 << 10, 95), text(100, 96));
	let tgz = gzip(&tar_of(&[("big.txt", &big), ("small.txt", &small)]));
	let (_, reader) = opened("t.tar.gz", tgz, None).await;
	assert!(!reader.index_held());
	let (result, _) = read_entries(&reader, &[1], Some(1000)).await;
	let error = result.unwrap_err().error;
	assert_eq!(error.kind(), ErrorKind::ArchiveSolidSkipExceeded);
	assert!(
		error
			.downcast_ref::<SolidSkipExceeded>()
			.unwrap()
			.skipped_bytes
			> 1000
	);
	let (result, written) = read_entries(&reader, &[1], Some(big.len() as u64)).await;
	assert_eq!(result.unwrap().entries, 1);
	assert_eq!(written[0].2, small);
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

async fn check_ranges(reader: &Reader, index: u32, data: &[u8], seed: u64) {
	assert_eq!(
		reader
			.entry_byte_access(entry(index), &JobControl::default())
			.await
			.unwrap(),
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

/// Opens entry `index` of `parent` as a nested archive.
async fn nested(parent: &Arc<Reader>, index: u32) -> Result<Arc<Reader>, Arc<Error>> {
	let (bytes, name) = parent
		.entry_bytes(entry_of(parent, index), &JobControl::default())
		.await?;
	let reader = Reader::nested(parent, bytes, name, None);
	reader
		.open(Ignore, JobControl::default())
		.await
		.map_err(|failed| failed.error)?;
	Ok(Arc::new(reader))
}

/// Entry `index` of the archive `reader` reads.
fn entry_of(reader: &Reader, index: u32) -> ArchiveEntryId {
	ArchiveEntryId {
		archive: reader.archive_uuid(),
		index,
	}
}

async fn range_of(reader: &Reader, index: u32, start: u64, end: u64) -> Vec<u8> {
	let mut written = futures::io::Cursor::new(Vec::new());
	reader
		.read_entry_range(
			entry_of(reader, index),
			start,
			end,
			&mut written,
			&JobControl::default(),
		)
		.await
		.unwrap();
	written.into_inner()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_inside_an_archive_is_read_through_its_entry() {
	let data = text(200 << 10, 12);
	let inner = zip_with(&[
		("a.txt", b"alpha", ZipMethod::Stored, None),
		("text", &data, ZipMethod::Deflate { level: 6 }, None),
	]);
	let tar = tar_with(&[TarMember::Data("t.txt", b"in a tar")]);
	for method in [ZipMethod::Stored, ZipMethod::Deflate { level: 6 }] {
		let outer = zip_with(&[
			("inner.zip", &inner, method, None),
			("inner.tar", &tar, method, None),
		]);
		let (_, outer) = opened("outer.zip", outer, None).await;
		let outer = Arc::new(outer);
		let zip = nested(&outer, 0).await.unwrap();
		assert_eq!(zip.format(), Some(ArchiveFormat::Zip));
		assert!(zip.index_held());
		let listed = zip.list(Ignore, JobControl::default()).await.unwrap();
		let id = nested_id(ARCHIVE, 0);
		assert_eq!(zip.archive_uuid(), id);
		assert!(listed.entries.iter().all(|entry| entry.id.archive == id));
		assert_eq!(range_of(&zip, 0, 0, 5).await, b"alpha");
		assert!(range_of(&zip, 1, 1000, 2000).await == data[1000..2000]);
		// a download through it is checked
		let mut written = futures::io::Cursor::new(Vec::new());
		let report = zip
			.download_entry(
				entry_of(&zip, 1),
				&mut written,
				None,
				Ignore,
				JobControl::default(),
			)
			.await
			.unwrap();
		assert!(report.checked);
		assert!(written.into_inner() == data);
		let tar = nested(&outer, 1).await.unwrap();
		tar.list(Ignore, JobControl::default()).await.unwrap();
		assert_eq!(range_of(&tar, 0, 0, 8).await, b"in a tar");
		// opened again, the same ids
		assert_eq!(nested(&outer, 0).await.unwrap().archive_uuid(), id);
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archives_nest_three_deep_and_no_deeper() {
	let mut archive = zip_with(&[("a.txt", b"alpha", ZipMethod::Stored, None)]);
	for _ in 0..4 {
		archive = zip_with(&[("next.zip", &archive, ZipMethod::Stored, None)]);
	}
	let (_, root) = opened("root.zip", archive, None).await;
	let mut reader = Arc::new(root);
	for _ in 0..3 {
		reader = nested(&reader, 0).await.unwrap();
	}
	assert_eq!(
		nested(&reader, 0).await.err().unwrap().kind(),
		ErrorKind::ArchiveUnsupported
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_only_read_whole_is_not_opened() {
	let inner = zip_with(&[("a.txt", b"alpha", ZipMethod::Stored, None)]);
	let outer = zip_with(&[("inner.zip", &inner, ZipMethod::Bzip2 { level: 6 }, None)]);
	let (_, outer) = opened("outer.zip", outer, None).await;
	assert_eq!(
		nested(&Arc::new(outer), 0).await.err().unwrap().kind(),
		ErrorKind::ArchiveUnsupported
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_expansion_limit_counts_across_the_chain() {
	let limit = ExpansionLimit {
		ratio: 10,
		floor: 1 << 20,
	};
	// the inner zip states no more than it holds, and compresses to next to nothing outside
	let zeros = vec![0u8; 4 << 20];
	let inner = zip_with(&[("zeros", &zeros, ZipMethod::Stored, None)]);
	let outer = zip_with(&[
		("inner.zip", &inner, ZipMethod::Deflate { level: 9 }, None),
		(
			"small.zip",
			&zip_with(&[(
				"z",
				&zeros[..2 << 20],
				ZipMethod::Deflate { level: 9 },
				None,
			)]),
			ZipMethod::Stored,
			None,
		),
	]);
	let (backend, archive, config) = drive("outer.zip", outer, 64 << 20);
	let outer = Reader::new(
		Arc::clone(&backend),
		archive,
		config,
		ListConfig {
			expansion_limit: Some(limit),
			..ListConfig::default()
		},
	);
	outer.open(Ignore, JobControl::default()).await.unwrap();
	let outer = Arc::new(outer);
	// 4 MiB from a few KiB is past the limit, though the entry states no more than it holds
	assert_eq!(
		nested(&outer, 0).await.err().unwrap().kind(),
		ErrorKind::ArchiveTooLarge
	);
	// the stored one opens, but what it decodes to counts against the outermost archive
	let small = nested(&outer, 1).await.unwrap();
	let mut written = futures::io::Cursor::new(Vec::new());
	let failed = small
		.download_entry(
			entry_of(&small, 0),
			&mut written,
			None,
			Ignore,
			JobControl::default(),
		)
		.await
		.unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveTooLarge);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_raw_entrys_embedded_preview_is_written_from_its_range() {
	let jpeg = jpeg_bytes(640, 480);
	let raw = raw_with(&jpeg, 1, jpeg.len() as u64);
	let zip = zip_with(&[
		("stored.dng", &raw, ZipMethod::Stored, None),
		("deflated.dng", &raw, ZipMethod::Deflate { level: 6 }, None),
		("note.txt", b"no preview here", ZipMethod::Stored, None),
		("whole.dng", &raw, ZipMethod::Bzip2 { level: 6 }, None),
	]);
	let (_, reader) = opened("raw.zip", zip, None).await;
	for index in [0, 1] {
		let mut written = futures::io::Cursor::new(Vec::new());
		let preview = reader
			.write_entry_embedded_preview(entry(index), &mut written, &JobControl::default())
			.await
			.unwrap()
			.expect("a preview");
		assert_eq!(u64::from(PREVIEW_AT), preview.located.offset);
		assert!(written.into_inner() == jpeg, "{index}");
	}
	let mut written = futures::io::Cursor::new(Vec::new());
	let none = reader
		.write_entry_embedded_preview(entry(2), &mut written, &JobControl::default())
		.await
		.unwrap();
	assert!(none.is_none());
	assert!(written.into_inner().is_empty());
	let mut written = futures::io::Cursor::new(Vec::new());
	assert_eq!(
		reader
			.write_entry_embedded_preview(entry(3), &mut written, &JobControl::default())
			.await
			.unwrap_err()
			.kind(),
		ErrorKind::ArchiveUnsupported
	);
}

/// Entry `index` downloaded whole, and whether it was checked.
async fn downloaded(reader: &Reader, index: u32) -> Result<(Vec<u8>, bool), Arc<Error>> {
	let mut written = futures::io::Cursor::new(Vec::new());
	let report = reader
		.download_entry(
			entry(index),
			&mut written,
			Some(u64::MAX),
			Ignore,
			JobControl::default(),
		)
		.await
		.map_err(|failed| failed.error)?;
	Ok((written.into_inner(), report.checked))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rars_members_are_listed_from_its_held_table_and_read_checked() {
	let (a, b) = (text(300 << 10, 51), text(5000, 52));
	let files = [("dir/a.txt", &a[..]), ("b.txt", &b[..])];
	for version in [rars::ArchiveVersion::Rar29, rars::ArchiveVersion::Rar50] {
		for (store, solid) in [(false, false), (true, false), (false, true)] {
			let rar = rar_of(version, &files, store, solid, None);
			let (backend, reader) = opened("a.rar", rar, None).await;
			assert!(
				matches!(reader.format(), Some(ArchiveFormat::Rar { .. })),
				"{version:?}"
			);
			assert!(reader.index_held());
			fetched(&backend);
			let listed = reader.list(Ignore, JobControl::default()).await.unwrap();
			assert_eq!(fetched(&backend), Vec::<u64>::new());
			let paths: Vec<&str> = listed
				.entries
				.iter()
				.map(|entry| entry.stored_path.as_str())
				.collect();
			assert_eq!(paths, ["dir/a.txt", "b.txt"], "{version:?}");
			// a solid archive's first member is decoded on its own
			let access = reader
				.entry_byte_access(entry(1), &JobControl::default())
				.await
				.unwrap();
			let expected = match (store, solid) {
				(true, _) => EntryByteAccess::Mapped {
					size: b.len() as u64,
				},
				(false, false) => EntryByteAccess::Whole {
					size: b.len() as u64,
				},
				(false, true) => EntryByteAccess::Sequential {
					size: Some(b.len() as u64),
				},
			};
			assert_eq!(access, expected, "{version:?} store {store} solid {solid}");
			if store {
				assert_eq!(range(&reader, 0, 10, 20).await.unwrap(), a[10..20]);
			}
			let (data, checked) = downloaded(&reader, 1)
				.await
				.unwrap_or_else(|e| panic!("{version:?} store {store} solid {solid}: {e:?}"));
			assert_eq!(data, b, "{version:?} store {store} solid {solid}");
			assert!(checked);
			let (result, written) = read_entries(&reader, &[1, 0], None).await;
			assert_eq!(result.unwrap().entries, 2);
			assert_eq!(written[0].2, a);
			assert_eq!(written[1].2, b);
		}
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rar_job_decodes_through_the_held_parse_reading_only_its_members_chunks() {
	let members: Vec<Vec<u8>> = (0..6)
		.map(|at| incompressible(600 << 10, 80 + at))
		.collect();
	let names: Vec<String> = (0..6).map(|at| format!("{at}.jpg")).collect();
	let files: Vec<(&str, &[u8])> = names
		.iter()
		.zip(&members)
		.map(|(name, data)| (name.as_str(), &data[..]))
		.collect();
	for store in [true, false] {
		let rar = rar_of(rars::ArchiveVersion::Rar50, &files, store, false, None);
		let (backend, reader) = opened("pages.cbr", rar, None).await;
		fetched(&backend);
		// the fifth and sixth lie in chunks 2 and 3: nothing else is read again (opening read
		// all of it, and the reader may still keep the two it read last)
		let (result, written) = read_entries(&reader, &[4, 5], Some(0)).await;
		assert_eq!(result.unwrap().entries, 2);
		assert_eq!(written[1].2, members[5]);
		let read = fetched(&backend);
		assert!(
			read.iter().all(|chunk| (2..=3).contains(chunk)),
			"store {store}: {read:?}"
		);
		// two jobs at once: the one that finds the held parse busy parses the RAR itself
		let (first, second) = tokio::join!(downloaded(&reader, 0), downloaded(&reader, 5));
		assert_eq!(first.unwrap().0, members[0]);
		assert_eq!(second.unwrap().0, members[5]);
	}
}

/// A reader of `bytes` under the smallest codec budget, whose RAR parse admits a few thousand
/// headers.
fn tight_reader(name: &str, bytes: Vec<u8>) -> Reader {
	let (backend, archive, _) = drive(name, bytes, 64 << 20);
	let config = ArchiveConfig::new(&ClientConfig::default().with_archive_codec_mem_budget(0))
		.with_own_pool(64 << 20);
	reader_of(&backend, &archive, &config, None)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rar_header_flood_is_refused_before_rars_keeps_it() {
	// service headers are no members, but they are headers all the same
	let few = tight_reader("few.rar", rar5_header_flood(10, 10));
	few.open(Ignore, JobControl::default()).await.unwrap();
	assert_eq!(
		few.list(Ignore, JobControl::default())
			.await
			.unwrap()
			.entries
			.len(),
		10
	);
	for (files, services) in [(5000, 0), (1, 5000)] {
		let flood = tight_reader("flood.rar", rar5_header_flood(files, services));
		let failed = flood.open(Ignore, JobControl::default()).await.unwrap_err();
		assert_eq!(
			failed.error.kind(),
			ErrorKind::ArchiveTooLarge,
			"{files} files, {services} services: {}",
			failed.error
		);
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_rar_parse_is_charged_as_rars_holds_it() {
	let (_, reader) = opened("services.rar", rar5_header_flood(1, 2000), None).await;
	let index = reader.retained.as_ref().unwrap().index().unwrap();
	assert!(matches!(&*index, HeldIndex::Rar(_)));
	// every header rars keeps, the service headers too, at its block's size
	assert!(index.heap_bytes() >= 2002 * size_of::<rars::rar50::Block>() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encrypted_rar_opens_under_its_password_alone() {
	let data = text(4000, 53);
	let rar = rar_of(
		rars::ArchiveVersion::Rar50,
		&[("secret.txt", &data[..])],
		false,
		false,
		Some("pw"),
	);
	let (backend, archive, config) = drive("s.rar", rar.clone(), 64 << 20);
	let wrong = reader_of(&backend, &archive, &config, Some("nope"));
	assert_eq!(
		wrong
			.open(Ignore, JobControl::default())
			.await
			.unwrap_err()
			.error
			.kind(),
		ErrorKind::ArchiveWrongPassword
	);
	let (_, reader) = opened("s.rar", rar, Some("pw")).await;
	assert_eq!(downloaded(&reader, 0).await.unwrap().0, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_comic_book_rar_behind_a_stub_is_read_by_its_name() {
	let page = text(2000, 54);
	let mut cbr = vec![0x4D, 0x5A];
	cbr.extend(vec![0u8; 1000]);
	cbr.extend(rar_of(
		rars::ArchiveVersion::Rar50,
		&[("01.png", &page[..])],
		true,
		false,
		None,
	));
	let (_, reader) = opened("comic.cbr", cbr, None).await;
	assert_eq!(range(&reader, 0, 0, 100).await.unwrap(), page[..100]);
	assert_eq!(downloaded(&reader, 0).await.unwrap().0, page);
}

/// The reader's benchmarks, run by hand (they print what they measure, and check the counters
/// bound it): `cargo test -p filen-sdk-rs --release --lib -- --ignored --nocapture
/// fs::archive::reader::tests::bench`. Native numbers; wasm runs the same code on its CPU pool.
mod bench {
	use std::time::Instant;

	use super::*;
	use crate::fs::archive::test_support::rar_of;

	fn mib(bytes: u64) -> f64 {
		bytes as f64 / f64::from(1 << 20)
	}

	fn rate(bytes: u64, started: Instant) -> f64 {
		mib(bytes) / started.elapsed().as_secs_f64()
	}

	/// Archive bytes fetched to read a window of 50 pages of a 200-page comic book, stored as
	/// comic books are: a page costs its own bytes and at most the chunks it shares.
	#[ignore = "a benchmark: run by hand with --release --ignored --nocapture"]
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn bench_bytes_fetched_for_fifty_comic_pages() {
		let pages: Vec<Vec<u8>> = (0..200)
			.map(|page| incompressible(300 << 10, page + 1))
			.collect();
		let names: Vec<String> = (0..200).map(|page| format!("{page:03}.jpg")).collect();
		let zip_files: Vec<_> = names
			.iter()
			.zip(&pages)
			.map(|(name, page)| (name.as_str(), &page[..], ZipMethod::Stored, None))
			.collect();
		let rar_files: Vec<_> = names
			.iter()
			.zip(&pages)
			.map(|(name, page)| (name.as_str(), &page[..]))
			.collect();
		let books = [
			("comic.cbz", zip_with(&zip_files)),
			(
				"comic.cbr",
				rar_of(rars::ArchiveVersion::Rar50, &rar_files, true, false, None),
			),
		];
		let window: Vec<u32> = (100..150).collect();
		let page_bytes: u64 = window
			.iter()
			.map(|&page| pages[page as usize].len() as u64)
			.sum();
		for (name, book) in books {
			let book_bytes = book.len() as u64;
			let (backend, reader) = opened(name, book, None).await;
			let opening = fetched(&backend).len() as u64;
			let started = Instant::now();
			let (result, written) = read_entries(&reader, &window, Some(0)).await;
			let elapsed = started.elapsed();
			assert_eq!(result.unwrap().entries, 50, "{name}");
			assert!(
				written
					.iter()
					.all(|(page, _, data)| *data == pages[*page as usize])
			);
			let pass = fetched(&backend).len() as u64;
			println!(
				"{name}: {:.1} MiB book, open {opening} chunks; 50 pages = {:.1} MiB read as {pass} \
				 chunks ({:.1} MiB) in {elapsed:?}",
				mib(book_bytes),
				mib(page_bytes),
				mib(pass * CHUNK_SIZE_U64),
			);
			assert!(
				pass * CHUNK_SIZE_U64 <= page_bytes + 2 * CHUNK_SIZE_U64,
				"{name}: {pass} chunks"
			);
		}
	}

	/// Inflate throughput of an entry read by range in 1 MiB windows, front to back, and how
	/// long a jump back takes with checkpoints 4, 16 and 32 MiB apart: each decodes at most the
	/// spacing (from the checkpoint before it) and the window.
	#[ignore = "a benchmark: run by hand with --release --ignored --nocapture"]
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn bench_inflate_throughput_and_seek_latency() {
		let data = text(64 << 20, 61);
		let len = data.len() as u64;
		let zip = zip_with(&[("big", &data, ZipMethod::Deflate { level: 6 }, None)]);
		for spacing in [4u64 << 20, 16 << 20, 32 << 20] {
			let (_, reader) = opened("z.zip", zip.clone(), None).await;
			let retained = reader.retained.as_ref().unwrap();
			retained.keep_seeks(StreamKey::Entry(0), Seeks::with_limits(spacing, 64 << 20));
			let started = Instant::now();
			let window = 1 << 20;
			let mut at = 0;
			while at < len {
				let end = (at + window).min(len);
				range(&reader, 0, at, end).await.unwrap();
				at = end;
			}
			let throughput = rate(len, started);
			let seeks = retained.take_seeks(StreamKey::Entry(0)).unwrap();
			assert_eq!(seeks.decoded_for_test(), len);
			let mut decoded = seeks.decoded_for_test();
			retained.keep_seeks(StreamKey::Entry(0), seeks);
			let jumps = 16u64;
			let mut most = 0;
			let started = Instant::now();
			for jump in 0..jumps {
				// back and forth over the entry, never where the last read stopped
				let to = (jump * 37 + 11) % 61 * (len / 64);
				range(&reader, 0, to, to + 4096).await.unwrap();
				let seeks = retained.take_seeks(StreamKey::Entry(0)).unwrap();
				let now = seeks.decoded_for_test();
				most = most.max(now - decoded);
				decoded = now;
				retained.keep_seeks(StreamKey::Entry(0), seeks);
			}
			let per_jump = started.elapsed() / u32::try_from(jumps).unwrap();
			println!(
				"inflate {:.0} MiB/s over {:.0} MiB; checkpoints {:.0} MiB apart: a jump takes \
				 {per_jump:?}, decoding at most {:.1} MiB",
				throughput,
				mib(len),
				mib(spacing),
				mib(most),
			);
			assert!(most <= spacing + 2 * inflate::STEP_BYTES, "{most}");
		}
	}

	/// Decode rate of a solid block (7z LZMA2, RAR 5) read to its last entry, and the time a
	/// pass would take to reach an entry 512 MiB into one at that rate.
	#[ignore = "a benchmark: run by hand with --release --ignored --nocapture"]
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn bench_solid_block_decode_rate() {
		let parts: Vec<Vec<u8>> = (0..8).map(|part| text(4 << 20, 70 + part)).collect();
		let names: Vec<String> = (0..8).map(|part| format!("{part}.txt")).collect();
		let block: u64 = parts.iter().map(|part| part.len() as u64).sum();
		let sevenz_files: Vec<_> = names
			.iter()
			.zip(&parts)
			.map(|(name, part)| (name.as_str(), Some(&part[..])))
			.collect();
		let rar_files: Vec<_> = names
			.iter()
			.zip(&parts)
			.map(|(name, part)| (name.as_str(), &part[..]))
			.collect();
		let archives = [
			(
				"s.7z",
				sevenz_of(&sevenz_files, SevenZMethod::Lzma2 { level: 1 }, true, None),
			),
			(
				"s.rar",
				rar_of(rars::ArchiveVersion::Rar50, &rar_files, false, true, None),
			),
		];
		for (name, archive) in archives {
			let (_, reader) = opened(name, archive, None).await;
			let started = Instant::now();
			let (result, written) = read_entries(&reader, &[7], None).await;
			let decode = rate(block, started);
			assert_eq!(result.unwrap().entries, 1, "{name}");
			assert_eq!(written[0].2, parts[7], "{name}");
			println!(
				"{name}: solid block of {:.0} MiB decoded at {decode:.0} MiB/s; reaching an entry \
				 512 MiB in takes {:.1} s",
				mib(block),
				512.0 / decode,
			);
		}
	}
}

/// A writer that never takes a byte, as a stream nobody drains.
struct Stalled;

impl AsyncWrite for Stalled {
	fn poll_write(
		self: std::pin::Pin<&mut Self>,
		_: &mut std::task::Context<'_>,
		_: &[u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		std::task::Poll::Pending
	}

	fn poll_flush(
		self: std::pin::Pin<&mut Self>,
		_: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		std::task::Poll::Pending
	}

	fn poll_close(
		self: std::pin::Pin<&mut Self>,
		_: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		std::task::Poll::Pending
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_writer_fails_its_pass_and_gives_its_slot_back() {
	let (zip, _) = spread_zip();
	let (backend, archive, config) = drive("z.zip", zip, 64 << 20);
	let reader = reader_of(&backend, &archive, &config, None);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	let slots = config.free_slots();
	let stalled = reader
		.read_entries(
			&[entry(0)],
			|_, _| std::future::ready(Ok(Some(Stalled))),
			None,
			Ignore,
			JobControl::default(),
		)
		.await
		.unwrap_err();
	assert_eq!(stalled.error.kind(), ErrorKind::IO, "{}", stalled.error);
	// a writer asked for that never comes fails the same way
	let unanswered = reader
		.read_entries(
			&[entry(0)],
			|_, _| std::future::pending::<Result<Option<Stalled>, Error>>(),
			None,
			Ignore,
			JobControl::default(),
		)
		.await
		.unwrap_err();
	assert_eq!(unanswered.error.kind(), ErrorKind::IO);
	assert_eq!(config.free_slots(), slots);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_pass_mid_entry_ends_it_at_once() {
	let (zip, _) = spread_zip();
	let (_, reader) = opened("z.zip", zip, None).await;
	let (control, controller) = JobControl::new();
	let canceller = tokio::spawn(async move {
		tokio::time::sleep(std::time::Duration::from_millis(50)).await;
		controller.cancel();
	});
	let started = std::time::Instant::now();
	let cancelled = reader
		.read_entries(
			&[entry(0), entry(1)],
			|_, _| std::future::ready(Ok(Some(Stalled))),
			None,
			Ignore,
			control,
		)
		.await
		.unwrap_err();
	canceller.await.unwrap();
	assert_eq!(cancelled.error.kind(), ErrorKind::Cancelled);
	assert_eq!(cancelled.report.entries, 0);
	// well before a stall would have ended it
	assert!(started.elapsed() < crate::fs::archive::input::SINK_STALL_TIMEOUT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bare_tar_pass_under_no_budget_lists_once_and_counts_only_what_it_wrote() {
	let (tar, _) = spread_tar();
	let (backend, archive, config) = drive("t.tar", tar, 0);
	let reader = reader_of(&backend, &archive, &config, None);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	fetched(&backend);
	let written: Written = Arc::default();
	let into = Arc::clone(&written);
	let report = reader
		.read_entries(
			&[entry(0), entry(3)],
			move |entry: ArchiveEntryId, _| {
				// the first is passed over
				let buffer = Arc::default();
				let writer = (entry.index == 3).then(|| {
					into.lock()
						.unwrap()
						.push((entry.index, None, Arc::clone(&buffer)));
					Recording(buffer)
				});
				std::future::ready(Ok(writer))
			},
			None,
			Ignore,
			JobControl::default(),
		)
		.await
		.unwrap();
	assert_eq!(report.entries, 1);
	assert_eq!(report.bytes_written, 5);
	assert_eq!(written.lock().unwrap().len(), 1);
	// one listing seeks the tar's headers once: its first chunk and its last two
	let read = fetched(&backend);
	assert!(read.len() <= 6, "{read:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gzip_tars_access_holds_without_room_to_keep_its_table() {
	let (tar, big) = spread_tar();
	let (backend, archive, config) = drive("t.tar.gz", gzip(&tar), 0);
	let reader = reader_of(&backend, &archive, &config, None);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	let control = JobControl::default();
	for _ in 0..2 {
		assert_eq!(
			reader.entry_byte_access(entry(1), &control).await.unwrap(),
			EntryByteAccess::Seekable {
				size: big.len() as u64
			}
		);
	}
	assert_eq!(range(&reader, 1, 10, 20).await.unwrap(), big[10..20]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gzip_tars_members_are_read_beside_a_stalled_one() {
	let (tar, big) = spread_tar();
	let (_, reader) = opened("t.tar.gz", gzip(&tar), None).await;
	let reader = Arc::new(reader);
	let (control, controller) = JobControl::new();
	let stalled = {
		let reader = Arc::clone(&reader);
		tokio::spawn(async move {
			let mut writer = Stalled;
			reader
				.read_entry_range(entry(1), 0, 100, &mut writer, &control)
				.await
		})
	};
	tokio::time::sleep(std::time::Duration::from_millis(50)).await;
	let read = tokio::time::timeout(std::time::Duration::from_secs(10), range(&reader, 3, 0, 5))
		.await
		.expect("another member is read while the first one's writer stalls");
	assert_eq!(read.unwrap(), b"gamma");
	controller.cancel();
	assert_eq!(
		stalled.await.unwrap().unwrap_err().kind(),
		ErrorKind::Cancelled
	);
	// the member's reads take turns: the next goes on once the stalled one ended
	assert_eq!(range(&reader, 1, 3, 9).await.unwrap(), big[3..9]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_range_read_that_reads_the_index_again_ends() {
	let (zip, data) = spread_zip();
	let (_, reader) = opened("z.zip", zip, None).await;
	reader.retained.as_ref().unwrap().evict_index_for_test();
	let (control, controller) = JobControl::new();
	controller.cancel();
	let mut written = futures::io::Cursor::new(Vec::new());
	let cancelled = reader
		.read_entry_range(entry(0), 0, 10, &mut written, &control)
		.await
		.unwrap_err();
	assert_eq!(cancelled.kind(), ErrorKind::Cancelled);
	assert_eq!(range(&reader, 0, 0, 10).await.unwrap(), data[0][..10]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoints_laid_in_flight_are_charged_and_capped_by_the_budget() {
	let data = text(12 << 20, 41);
	let zip = zip_with(&[("big", &data, ZipMethod::Deflate { level: 6 }, None)]);
	// room for little more than the reader's own index
	let (backend, archive, config) = drive("z.zip", zip, 200 << 10);
	let reader = reader_of(&backend, &archive, &config, None);
	reader.open(Ignore, JobControl::default()).await.unwrap();
	let len = data.len() as u64;
	assert_eq!(
		range(&reader, 0, len - 10, len).await.unwrap(),
		data[data.len() - 10..]
	);
	let retained = reader.retained.as_ref().unwrap();
	assert!(config.retained.charged_for_test() <= 200 << 10);
	if let Some(seeks) = retained.take_seeks(StreamKey::Entry(0)) {
		assert!(seeks.bytes() <= 200 << 10, "{}", seeks.bytes());
	}
}
