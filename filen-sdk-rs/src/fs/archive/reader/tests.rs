//! Readers against the fake drive, with the real codec on its thread.

use std::sync::Arc;

use filen_types::fs::Uuid;

use super::*;
use crate::{
	auth::http::ClientConfig,
	consts::{CHUNK_SIZE, CHUNK_SIZE_U64},
	fs::{
		archive::{
			extract::{ArchiveEntry, EntryDownloadUpdate, ListUpdate},
			sevenz::write::{SevenZEncryption, SevenZMethod},
			test_support::{archive_password, incompressible, sevenz_of},
			zip::write::{ZipMethod, ZipWriter},
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
	let tar = crate::fs::archive::test_support::tar_of(&[("a.txt", b"alpha")]);
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
}
