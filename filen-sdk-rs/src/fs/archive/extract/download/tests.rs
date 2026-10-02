//! Downloads against the fake drive, with the real codec on its thread (or a scripted one), into
//! a writer that records what it was given.

use std::{
	io::{self, Cursor},
	pin::Pin,
	sync::{
		Mutex,
		atomic::{AtomicBool, Ordering},
	},
	task::{Context, Poll},
};

use filen_types::fs::Uuid;
use tokio::task::JoinHandle;

use super::*;
use crate::{
	consts::CHUNK_SIZE,
	fs::{
		HasUUID,
		archive::{
			entry_path::entry_path,
			extract::{
				EntryDownloadConfig, ExpansionLimit,
				codec::{ArchiveEnd, StreamJob, Task, extract_stream},
				list::PasswordCheck,
				test_support::{Options, Setup, setup, stream_job, test_config},
			},
			sevenz::write::SevenZMethod,
			test_support::{
				archive_password, incompressible, pattern, sevenz_of, tar_of, zip_of,
				zip_with_passwords,
			},
			worker,
			zip::write::{ZipMethod, ZipWriter},
		},
		drive_job::test_support::{Request, wait_until},
		file::traits::HasFileInfo,
	},
	job::test_support::controls,
};

/// What a download wrote, and whether it closed its writer.
#[derive(Default)]
struct Written {
	data: Mutex<Vec<u8>>,
	closed: AtomicBool,
	/// While set, writes never complete.
	stall: AtomicBool,
	/// A write reached the stall.
	stalled: AtomicBool,
}

impl Written {
	fn data(&self) -> Vec<u8> {
		self.data.lock().unwrap().clone()
	}

	fn closed(&self) -> bool {
		self.closed.load(Ordering::SeqCst)
	}
}

/// A download's writer, recording into its [`Written`].
struct Writer(Arc<Written>);

impl AsyncWrite for Writer {
	fn poll_write(
		self: Pin<&mut Self>,
		_: &mut Context<'_>,
		buf: &[u8],
	) -> Poll<io::Result<usize>> {
		if self.0.stall.load(Ordering::SeqCst) {
			// never woken: only the job stopping ends the write
			self.0.stalled.store(true, Ordering::SeqCst);
			return Poll::Pending;
		}
		self.0.data.lock().unwrap().extend_from_slice(buf);
		Poll::Ready(Ok(buf.len()))
	}

	fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
		Poll::Ready(Ok(()))
	}

	fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
		self.0.closed.store(true, Ordering::SeqCst);
		Poll::Ready(Ok(()))
	}
}

#[derive(Default)]
struct EntryDownloadRecorder {
	updates: Mutex<Vec<EntryDownloadUpdate>>,
}

impl EntryDownloadRecorder {
	fn first_reading(&self) -> EntryDownloadUpdate {
		self.updates
			.lock()
			.unwrap()
			.iter()
			.find(|update| update.phase == EntryDownloadPhase::Reading)
			.unwrap()
			.clone()
	}

	fn last(&self) -> EntryDownloadUpdate {
		self.updates.lock().unwrap().last().unwrap().clone()
	}
}

impl EntryDownloadCallback for EntryDownloadRecorder {
	fn on_update(&self, update: EntryDownloadUpdate) {
		self.updates.lock().unwrap().push(update);
	}
}

struct Download {
	running: JoinHandle<Result<EntryDownloadReport, EntryDownloadFailed>>,
	recorder: Arc<EntryDownloadRecorder>,
	reporter: MaybeArc<EntryDownloadReporter>,
	written: Arc<Written>,
}

/// Entry `index` of `setup`'s archive.
fn entry(setup: &Setup, index: u32) -> ArchiveEntryId {
	ArchiveEntryId {
		archive: setup.archive.uuid(),
		index,
	}
}

/// The real codec, on the job the client gives it.
fn real_codec(job: StreamJob) -> CodecStart<CodecResult> {
	Box::new(move || worker::start(move |port| extract_stream(&port, job)))
}

/// Downloads `entry` of `setup`'s archive into a writer recording into `written`, as
/// [`Client::download_archive_entry`](crate::auth::Client::download_archive_entry) does, its
/// codec started by `codec` on the job the client would give it.
fn download_with(
	setup: &Setup,
	entry: ArchiveEntryId,
	config: EntryDownloadConfig,
	control: JobControl,
	written: Arc<Written>,
	codec: impl FnOnce(StreamJob) -> CodecStart<CodecResult>,
) -> Download {
	let recorder = Arc::new(EntryDownloadRecorder::default());
	let reporter = EntryDownloadReporter::new(Arc::clone(&recorder), setup.archive.size());
	let mut writer = Writer(Arc::clone(&written));
	let running = match choose_entry(setup.archive.clone(), entry, &reporter) {
		Err(failed) => tokio::spawn(async move { Err(failed) }),
		Ok((archive, selection)) => {
			let EntryDownloadConfig {
				expansion_limit,
				password,
				max_solid_skip,
			} = config;
			let options = Options {
				expansion: expansion_limit,
				..Options::default()
			};
			let job = StreamJob {
				password,
				skip_mac_metadata: false,
				task: Task::Download {
					selection,
					max_solid_skip,
				},
				..stream_job(setup, &options)
			};
			let task = DownloadTask {
				backend: Arc::clone(&setup.backend),
				control,
				reporter: MaybeArc::clone(&reporter),
				archive,
				ordinal: u64::from(entry.index),
				config: test_config(),
				start: codec(job),
			};
			tokio::spawn(async move { run_download(task, &mut writer).await })
		}
	};
	Download {
		running,
		recorder,
		reporter,
		written,
	}
}

/// Downloads entry `index` of `setup`'s archive with the real codec.
fn download(setup: &Setup, index: u32, config: EntryDownloadConfig) -> Download {
	download_with(
		setup,
		entry(setup, index),
		config,
		JobControl::default(),
		Arc::default(),
		real_codec,
	)
}

/// The chunks of `setup`'s archive fetched, in the order they came in.
fn fetched(setup: &Setup) -> Vec<u64> {
	setup
		.backend
		.log()
		.fetched
		.iter()
		.map(|(_, index)| *index)
		.collect()
}

/// A zip of `files`, stored: each one's data lies in the archive as it is.
fn stored_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
	let mut writer = ZipWriter::new(Vec::new());
	for (path, data) in files {
		writer
			.add_file(
				path,
				None,
				data.len() as u64,
				ZipMethod::Stored,
				None,
				&mut &data[..],
			)
			.unwrap();
	}
	writer.finish().unwrap()
}

fn with_password(password: &str) -> EntryDownloadConfig {
	EntryDownloadConfig {
		password: Some(archive_password(password)),
		..EntryDownloadConfig::default()
	}
}

fn with_solid_skip(max_solid_skip: u64) -> EntryDownloadConfig {
	EntryDownloadConfig {
		max_solid_skip: Some(max_solid_skip),
		..EntryDownloadConfig::default()
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zip_entry_is_written_whole_fetching_only_the_head_the_index_and_its_own_chunks() {
	let (a, b, c) = (
		incompressible(2 * CHUNK_SIZE, 1),
		incompressible(CHUNK_SIZE / 4, 2),
		incompressible(2 * CHUNK_SIZE, 3),
	);
	// stored, so a lies in chunks 0 and 1, b in chunk 2, c from chunk 2 into chunk 4, which
	// holds the index after it
	let zip = stored_zip(&[("a.bin", &a), ("b.bin", &b), ("c.bin", &c)]);
	assert_eq!(zip.len().div_ceil(CHUNK_SIZE), 5);
	let setup = setup("bundle.zip", zip.clone(), |_| {});
	let download = download(&setup, 1, EntryDownloadConfig::default());
	let report = download.running.await.unwrap().unwrap();

	assert_eq!(download.written.data(), b);
	assert!(download.written.closed());
	let last = download.recorder.last();
	assert_eq!(
		report,
		EntryDownloadReport {
			bytes_written: b.len() as u64,
			bytes_read: last.bytes_read,
			checked: true,
		}
	);
	// the head to tell the format, the index at the end, then b's own chunk
	assert_eq!(fetched(&setup), [0, 4, 2]);
	assert!(report.bytes_read < zip.len() as u64);
	assert_eq!(
		(last.phase, last.bytes_written, last.entry_bytes, last.eta),
		(
			EntryDownloadPhase::Done,
			b.len() as u64,
			Some(b.len() as u64),
			Some(Duration::ZERO)
		)
	);
	// before the entry was reached, nothing tells the time left
	let reading = download.recorder.first_reading();
	assert_eq!((reading.entry_bytes, reading.eta), (None, None));
	setup.backend.assert_released(&download.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_entry_of_a_zip_with_encrypted_ones_downloads_without_a_password() {
	let zip = zip_with_passwords(&[
		("secret.txt", Some(b"secret"), Some("pw")),
		("plain.txt", Some(b"plain"), None),
	]);
	let setup = setup("mixed.zip", zip, |_| {});
	let download = download(&setup, 1, EntryDownloadConfig::default());
	let report = download.running.await.unwrap().unwrap();
	assert_eq!(download.written.data(), b"plain");
	assert!(report.checked);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encrypted_entry_of_several_chunks_is_fetched_once() {
	let data = incompressible(3 * CHUNK_SIZE + CHUNK_SIZE / 2, 4);
	// AES over data deflate cannot shrink: the entry from chunk 0 into chunk 3, which holds
	// the index after it
	let zip = zip_of(&[("secret.bin", Some(&data))], Some("pw"));
	assert_eq!(zip.len().div_ceil(CHUNK_SIZE), 4);
	let setup = setup("secret.zip", zip, |_| {});
	let download = download(&setup, 0, with_password("pw"));
	let report = download.running.await.unwrap().unwrap();
	assert_eq!(download.written.data(), data);
	assert!(report.checked);
	// the head, the index in the last chunk, then the entry from its start: chunk 0 still at
	// hand, the last chunk again for the entry's end. A password checked on the entry before
	// it is read would fetch every one of them once more
	let mut fetched = fetched(&setup);
	fetched.sort_unstable();
	assert_eq!(fetched, [0, 1, 2, 3, 3]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aes_entry_without_a_password_fails_password_required_writing_nothing() {
	let zip = zip_of(&[("secret.txt", Some(b"secret"))], Some("pw"));
	let setup = setup("secret.zip", zip, |_| {});
	let download = download(&setup, 0, EntryDownloadConfig::default());
	let failed = download.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchivePasswordRequired);
	assert!(download.written.data().is_empty());
	assert!(!download.written.closed());
	assert_eq!(
		(failed.report.bytes_written, failed.report.checked),
		(0, false)
	);
	assert_eq!(download.recorder.last().phase, EntryDownloadPhase::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aes_entry_under_a_wrong_password_fails_wrong_password_writing_nothing() {
	let zip = zip_of(&[("secret.txt", Some(b"secret"))], Some("pw"));
	let setup = setup("secret.zip", zip, |_| {});
	let download = download(&setup, 0, with_password("wrong"));
	let failed = download.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWrongPassword);
	assert!(download.written.data().is_empty());
	assert!(!download.written.closed());
	assert_eq!(failed.report.bytes_written, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_damaged_entry_fails_corrupt_and_leaves_the_writer_unclosed() {
	let data = incompressible(CHUNK_SIZE + CHUNK_SIZE / 2, 5);
	let mut zip = stored_zip(&[("a.bin", &data)]);
	let start = zip
		.windows(64)
		.position(|window| window == &data[..64])
		.unwrap();
	zip[start + 1000] ^= 0x01;
	let setup = setup("damaged.zip", zip, |_| {});
	let download = download(&setup, 0, EntryDownloadConfig::default());
	let failed = download.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveCorrupt);
	// the first chunk was written before the CRC-32 at the entry's end was checked
	assert_eq!(download.written.data().len(), CHUNK_SIZE);
	assert!(!download.written.closed());
	assert_eq!(
		(failed.report.bytes_written, failed.report.checked),
		(CHUNK_SIZE as u64, false)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_directory_entry_is_refused_before_anything_is_written() {
	let zip = zip_of(&[("docs", None), ("docs/a.txt", Some(b"a"))], None);
	let zipped = setup("docs.zip", zip, |_| {});
	let refused = download(&zipped, 0, EntryDownloadConfig::default());
	let failed = refused.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::InvalidState);
	assert!(refused.written.data().is_empty());
	assert!(!refused.written.closed());

	// a 7z directory whose file comes after 2 MiB of another in its solid block: refused from
	// the index alone, so the block is not read up to the file (chunks 0 and the last only)
	let sevenz = sevenz_of(
		&[
			("x.bin", Some(&incompressible(2 * CHUNK_SIZE, 6))),
			("docs/a.txt", Some(b"a")),
			("docs", None),
		],
		SevenZMethod::Copy,
		true,
		None,
	);
	assert_eq!(sevenz.len().div_ceil(CHUNK_SIZE), 3);
	let setup = setup("docs.7z", sevenz, |_| {});
	// whatever skip the download allows
	let download = download(&setup, 2, with_solid_skip(u64::MAX));
	let failed = download.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::InvalidState);
	assert_eq!(fetched(&setup), [0, 2]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlink_entry_fails_with_its_skip_reason() {
	let mut writer = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
	writer
		.add_symlink(
			"link",
			"target/file",
			::zip::write::SimpleFileOptions::default(),
		)
		.unwrap();
	let zip = writer.finish().unwrap().into_inner();
	let setup = setup("link.zip", zip, |_| {});
	let download = download(&setup, 0, EntryDownloadConfig::default());
	let failed = download.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::InvalidState);
	assert!(
		matches!(
			failed.error.downcast_ref::<EntryDownloadError>(),
			Some(EntryDownloadError::Skipped(ExtractSkipReason::Symlink { target }))
				if target == "target/file"
		),
		"{:?}",
		failed.error
	);
	assert!(download.written.data().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_of_another_archive_is_refused_with_a_final_update_before_anything_is_fetched() {
	let zip = zip_of(&[("a.txt", Some(b"a"))], None);
	let setup = setup("a.zip", zip, |_| {});
	let foreign = ArchiveEntryId {
		archive: Uuid::from_u128(0xF),
		index: 0,
	};
	let download = download_with(
		&setup,
		foreign,
		EntryDownloadConfig::default(),
		JobControl::default(),
		Arc::default(),
		real_codec,
	);
	let failed = download.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::InvalidState);
	assert_eq!(failed.report, EntryDownloadReport::default());
	assert!(fetched(&setup).is_empty());
	assert_eq!(download.recorder.last().phase, EntryDownloadPhase::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tar_is_refused_once_its_first_chunk_is_read() {
	let tar = tar_of(&[("a.bin", &incompressible(3 * CHUNK_SIZE, 7))]);
	let setup = setup("a.tar", tar, |_| {});
	let download = download(&setup, 0, EntryDownloadConfig::default());
	let failed = download.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveUnsupported);
	assert_eq!(fetched(&setup), [0]);
	assert!(download.written.data().is_empty());
	setup.backend.assert_released(&download.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_7z_entry_alone_in_its_folder_reads_only_that_folder() {
	let (a, b, c) = (
		incompressible(CHUNK_SIZE + CHUNK_SIZE / 4, 1),
		incompressible(CHUNK_SIZE / 2, 2),
		incompressible(CHUNK_SIZE + CHUNK_SIZE / 2, 3),
	);
	// copied, a folder each, after the 32-byte start header: b inside chunk 1, c from chunk 1
	// into chunk 3, which holds the header after it
	let sevenz = sevenz_of(
		&[
			("a.bin", Some(&a)),
			("b.bin", Some(&b)),
			("c.bin", Some(&c)),
		],
		SevenZMethod::Copy,
		false,
		None,
	);
	assert_eq!(sevenz.len().div_ceil(CHUNK_SIZE), 4);
	let setup = setup("apart.7z", sevenz, |_| {});
	let download = download(&setup, 1, EntryDownloadConfig::default());
	let report = download.running.await.unwrap().unwrap();
	assert_eq!(download.written.data(), b);
	assert!(report.checked);
	assert_eq!(fetched(&setup), [0, 3, 1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_7z_entry_in_a_solid_block_decodes_the_files_before_it() {
	let (a, b) = (
		incompressible(CHUNK_SIZE + CHUNK_SIZE / 2, 1),
		incompressible(CHUNK_SIZE / 2, 2),
	);
	let sevenz = sevenz_of(
		&[("a.bin", Some(&a)), ("b.bin", Some(&b))],
		SevenZMethod::Copy,
		true,
		None,
	);
	let setup = setup("solid.7z", sevenz.clone(), |_| {});
	let download = download(&setup, 1, with_solid_skip(a.len() as u64));
	let report = download.running.await.unwrap().unwrap();
	assert_eq!(download.written.data(), b);
	assert!(report.checked);
	// a was read to reach b: with the head and the header, all of the archive
	assert_eq!(report.bytes_read, sevenz.len() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_7z_file_downloads_from_the_index_alone() {
	let big = incompressible(2 * CHUNK_SIZE, 8);
	// the big file in chunks 0 to 2, the header after it in chunk 2; the empty file has no data
	let sevenz = sevenz_of(
		&[("big.bin", Some(&big)), ("empty", Some(b""))],
		SevenZMethod::Copy,
		false,
		None,
	);
	assert_eq!(sevenz.len().div_ceil(CHUNK_SIZE), 3);
	let setup = setup("empty.7z", sevenz, |_| {});
	let download = download(&setup, 1, EntryDownloadConfig::default());
	let report = download.running.await.unwrap().unwrap();
	assert!(download.written.data().is_empty());
	assert!(download.written.closed());
	assert_eq!((report.bytes_written, report.checked), (0, true));
	assert_eq!(fetched(&setup), [0, 2]);
}

/// A codec that sends a 7z's entry 0, the 3 bytes `abc`, whole, then returns `end`.
fn sends_abc_then(end: CodecResult) -> impl FnOnce(StreamJob) -> CodecStart<CodecResult> {
	move |_| {
		Box::new(move || {
			worker::start(move |port| {
				port.send(WorkerEvent::Opened(ArchiveFormat::SevenZ))?;
				port.send(WorkerEvent::Entry(EntryHead {
					ordinal: 0,
					path: entry_path("a.bin").unwrap(),
					modified: None,
					kind: EntryKind::File { size: Some(3) },
				}))?;
				port.send(WorkerEvent::Data(b"abc".to_vec()))?;
				port.send(WorkerEvent::FileEnd)?;
				end
			})
		})
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_7z_entry_without_a_crc_downloads_unchecked() {
	let setup = setup("scripted.7z", vec![0; 100], |_| {});
	// what a 7z codec returns for an entry whose header lists no CRC-32
	let end = ArchiveEnd {
		unaccounted_bytes: 0,
		duplicates: None,
		unchecked_entries: 1,
		password: PasswordCheck::NotNeeded,
	};
	let download = download_with(
		&setup,
		entry(&setup, 0),
		EntryDownloadConfig::default(),
		JobControl::default(),
		Arc::default(),
		sends_abc_then(Ok(end)),
	);
	let report = download.running.await.unwrap().unwrap();
	assert_eq!(download.written.data(), b"abc");
	assert!(download.written.closed());
	assert_eq!((report.bytes_written, report.checked), (3, false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codec_failing_after_its_entry_ended_leaves_the_writer_unclosed() {
	let setup = setup("scripted.7z", vec![0; 100], |_| {});
	let damaged = Error::custom(ErrorKind::ArchiveCorrupt, "damaged after the entry");
	let download = download_with(
		&setup,
		entry(&setup, 0),
		EntryDownloadConfig::default(),
		JobControl::default(),
		Arc::default(),
		sends_abc_then(Err(damaged)),
	);
	let failed = download.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveCorrupt);
	// the whole entry reached the writer, but the codec never vouched for it
	assert_eq!(download.written.data(), b"abc");
	assert!(!download.written.closed());
	assert_eq!(
		(failed.report.bytes_written, failed.report.checked),
		(3, false)
	);
	assert_eq!(download.recorder.last().phase, EntryDownloadPhase::Failed);
}

#[test]
fn each_refusal_has_its_error_kind() {
	let kind = |error: EntryDownloadError| Error::from(error).kind();
	assert_eq!(
		kind(EntryDownloadError::NotIndexed),
		ErrorKind::ArchiveUnsupported
	);
	assert_eq!(kind(EntryDownloadError::NotAFile), ErrorKind::InvalidState);
	let target = || "t".to_owned();
	let skips = [
		(
			ExtractSkipReason::UnsupportedMethod,
			ErrorKind::ArchiveUnsupported,
		),
		(
			ExtractSkipReason::UnsupportedType,
			ErrorKind::ArchiveUnsupported,
		),
		(ExtractSkipReason::Sparse, ErrorKind::ArchiveUnsupported),
		(
			ExtractSkipReason::OverlappingData,
			ErrorKind::ArchiveCorrupt,
		),
		(
			ExtractSkipReason::Symlink { target: target() },
			ErrorKind::InvalidState,
		),
		(
			ExtractSkipReason::Hardlink { target: target() },
			ErrorKind::InvalidState,
		),
		(ExtractSkipReason::Device, ErrorKind::InvalidState),
		(ExtractSkipReason::PathTooLong, ErrorKind::InvalidState),
		(ExtractSkipReason::PathTooDeep, ErrorKind::InvalidState),
		(ExtractSkipReason::UnsafePath, ErrorKind::InvalidState),
		(ExtractSkipReason::AntiItem, ErrorKind::InvalidState),
		(ExtractSkipReason::MacMetadata, ErrorKind::InvalidState),
	];
	for (reason, expected) in skips {
		let shown = format!("{reason:?}");
		assert_eq!(
			kind(EntryDownloadError::Skipped(reason)),
			expected,
			"{shown}"
		);
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_stating_exactly_the_expansion_limit_downloads_and_one_byte_more_is_refused_unread()
 {
	let data = incompressible(2 * CHUNK_SIZE + CHUNK_SIZE / 2, 9);
	// stored: the entry in chunks 0 to 2, the index after it in chunk 2, far enough into it
	// that the search for the zip's end record stays in that chunk
	let zip = stored_zip(&[("a.bin", &data)]);
	assert_eq!(zip.len().div_ceil(CHUNK_SIZE), 3);
	// no ratio: the floor alone is the limit
	let limited = |floor| EntryDownloadConfig {
		expansion_limit: Some(ExpansionLimit { ratio: 0, floor }),
		..EntryDownloadConfig::default()
	};
	let fits = setup("fits.zip", zip.clone(), |_| {});
	let fitting = download(&fits, 0, limited(data.len() as u64));
	fitting.running.await.unwrap().unwrap();
	assert_eq!(fitting.written.data(), data);

	let over = setup("over.zip", zip, |_| {});
	let download = download(&over, 0, limited(data.len() as u64 - 1));
	let failed = download.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveTooLarge);
	assert_eq!(fetched(&over), [0, 2]);
	assert!(download.written.data().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_paused_while_it_reads_gives_back_its_memory_and_goes_on_once_resumed() {
	let data = incompressible(3 * CHUNK_SIZE, 10);
	let setup = setup("paused.zip", stored_zip(&[("a.bin", &data)]), |_| {});
	setup
		.backend
		.hold_requests(Request::Fetch, [setup.archive.uuid()]);
	let (pause, _cancel, control) = controls();
	let download = download_with(
		&setup,
		entry(&setup, 0),
		EntryDownloadConfig::default(),
		control,
		Arc::default(),
		real_codec,
	);
	wait_until("a fetch is held", || !setup.backend.log().held.is_empty()).await;
	pause.send_replace(true);
	setup.backend.release_all();
	wait_until("the download is paused", || download.reporter.is_paused()).await;
	assert_eq!(
		setup.backend.memory.available_permits(),
		setup.backend.budget
	);
	pause.send_replace(false);
	download.running.await.unwrap().unwrap();
	assert_eq!(download.written.data(), data);
	setup.backend.assert_released(&download.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_cancelled_while_its_writer_blocks_ends_cancelled() {
	let data = incompressible(CHUNK_SIZE + CHUNK_SIZE / 2, 11);
	let setup = setup("stalled.zip", stored_zip(&[("a.bin", &data)]), |_| {});
	let written = Arc::new(Written::default());
	written.stall.store(true, Ordering::SeqCst);
	let (_pause, cancel, control) = controls();
	let download = download_with(
		&setup,
		entry(&setup, 0),
		EntryDownloadConfig::default(),
		control,
		Arc::clone(&written),
		real_codec,
	);
	wait_until("a write is stalled", || {
		written.stalled.load(Ordering::SeqCst)
	})
	.await;
	cancel.send_replace(true);
	let failed = download.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	assert_eq!(
		download.recorder.last().phase,
		EntryDownloadPhase::Cancelled
	);
	assert!(!written.closed());
	setup.backend.assert_released(&download.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_fetch_ends_the_download_with_the_fetchs_error() {
	let zip = zip_of(&[("a.txt", Some(b"a"))], None);
	let setup = setup("failing.zip", zip, |backend| {
		backend
			.fail_fetch
			.insert("failing.zip".to_owned(), ErrorKind::Server);
	});
	let download = download(&setup, 0, EntryDownloadConfig::default());
	let failed = download.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Server);
	assert_eq!(download.recorder.last().phase, EntryDownloadPhase::Failed);
	assert!(download.written.data().is_empty());
	setup.backend.assert_released(&download.reporter);
}

/// A solid, copied 7z of two incompressible files of 2 MiB: the second one 2 MiB into the block,
/// which runs from chunk 0 into chunk 4, the header after it.
fn two_solid_files() -> (Vec<u8>, Vec<u8>) {
	let (a, b) = (
		incompressible(2 * CHUNK_SIZE, 12),
		incompressible(2 * CHUNK_SIZE, 13),
	);
	let sevenz = sevenz_of(
		&[("a.bin", Some(&a)), ("b.bin", Some(&b))],
		SevenZMethod::Copy,
		true,
		None,
	);
	assert_eq!(sevenz.len().div_ceil(CHUNK_SIZE), 5);
	(sevenz, b)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_solid_entry_skipping_exactly_the_limit_downloads_and_one_byte_more_fails_before_its_block_is_fetched()
 {
	let (sevenz, b) = two_solid_files();
	let skipped = 2 * CHUNK_SIZE as u64;
	let fits = setup("fits.7z", sevenz.clone(), |_| {});
	let fitting = download(&fits, 1, with_solid_skip(skipped));
	fitting.running.await.unwrap().unwrap();
	assert_eq!(fitting.written.data(), b);

	let over = setup("over.7z", sevenz, |_| {});
	let download = download(&over, 1, with_solid_skip(skipped - 1));
	let failed = download.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveSolidSkipExceeded);
	// the head and the header: none of the block's chunks 1 to 3
	assert_eq!(fetched(&over), [0, 4]);
	assert!(download.written.data().is_empty());
}

/// A solid, copied 7z of a (100 bytes), b (200) and c (50): one block of 350 bytes.
fn small_solid_block() -> Vec<u8> {
	sevenz_of(
		&[
			("a", Some(&pattern(100, 1))),
			("b", Some(&pattern(200, 2))),
			("c", Some(&pattern(50, 3))),
		],
		SevenZMethod::Copy,
		true,
		None,
	)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_config_refuses_a_solid_entry_stored_after_others() {
	let setup = setup("solid.7z", small_solid_block(), |_| {});
	let download = download(&setup, 1, EntryDownloadConfig::default());
	let failed = download.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveSolidSkipExceeded);
	assert!(download.written.data().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_file_of_a_solid_block_downloads_under_the_default_config() {
	let setup = setup("solid.7z", small_solid_block(), |_| {});
	let download = download(&setup, 0, EntryDownloadConfig::default());
	let report = download.running.await.unwrap().unwrap();
	assert_eq!(download.written.data(), pattern(100, 1));
	assert!(report.checked);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_solid_download_carries_the_skip_in_its_error() {
	let setup = setup("solid.7z", small_solid_block(), |_| {});
	let download = download(&setup, 2, EntryDownloadConfig::default());
	let failed = download.running.await.unwrap().unwrap_err();
	// c, after a and b: the whole block is fetched to reach its end
	let skip = SolidSkipExceeded {
		skipped_bytes: 300,
		limit: 0,
		estimated_packed_bytes: 350,
	};
	assert_eq!(
		failed.error.downcast_ref::<SolidSkipExceeded>(),
		Some(&skip)
	);
	let error = Error::from(failed);
	assert_eq!(error.kind(), ErrorKind::ArchiveSolidSkipExceeded);
	assert_eq!(error.downcast_ref::<SolidSkipExceeded>(), Some(&skip));
}
