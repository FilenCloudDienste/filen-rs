//! Runs an extraction of an archive of any format (a tar, compressed or not, a single compressed
//! file, a zip or a 7z): the async driver of the codec worker. It feeds the codec the archive,
//! creates the directories and uploads the files the codec reads out of it, in the order the
//! codec reads them, copies a tar's hard links from the files they name, and registers each file
//! once its data is up.
//!
//! # Memory and progress
//!
//! A job has a floor of two chunks for as long as it runs, taken outside the client's file-IO
//! budget: one for the chunk of the archive the codec is reading, one for a chunk of output
//! uploading. More (prefetched input, more uploads at once) is only taken when the client's
//! file-IO budget has it free right now, never waited for, so a job always makes progress on its
//! floor, and never waits on memory a transfer or another job holds. What the codec itself holds
//! (its decoder state, the output chunk it is filling, the one in the channel) is bounded by the
//! codec budget and three chunks.
//!
//! # Pause and cancel
//!
//! Pausing stops everything new: no chunk fetched or uploaded, no directory created, no event
//! taken from the codec, which parks. In-flight work finishes; then the job gives back its
//! prefetched chunks and reports itself paused, holding no drive lock and none of the client's
//! memory budget. The codec keeps its own state resident while paused, and so the job keeps its
//! slot; a job paused before it got one waits without taking it.
//!
//! Cancelling drops the transfers in flight at once (a file only becomes visible when it is
//! registered) but lets directory creates and registrations in flight finish, so every item
//! that was created is known and reported.

mod dirs;
mod files;
mod finish;
mod links;

use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, Utc};
use filen_types::fs::Uuid;
use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::Semaphore;

use crate::{
	Error, ErrorKind,
	connect::ConnectedTargets,
	consts::{CALLBACK_INTERVAL, MAX_SMALL_PARALLEL_REQUESTS},
	fs::{
		HasUUID,
		archive::{
			config::ArchiveConfig,
			dispose::{
				DisposalBackend, DisposalOutcome, KeptReason, SourceDisposal, SourceDisposition,
				kept_on_early_end,
			},
			entry_path::{ArchivePath, joined},
			format::ArchiveFormat,
			input::{CodecFeed, Fed, ReadingJob, start_reading},
			limits::keep,
			names::DirId,
			worker::{
				CodecStart, EntryHead, EntryKind, SkippedMember, WorkerEvent, codec_failed,
				unexpected_event, worker_died,
			},
		},
		categories::{DirType, NonRootItemType, Normal},
		drive_job::{Fatal, backend::DriveBackend, exceeds_limit, finalize::UnlessPaused},
		file::{enums::RemoteFileType, traits::HasFileInfo, write::RemoteFileInfo},
		name::ValidatedName,
	},
	job::{JobControl, Stopped, report::JobReport},
	util::{MaybeArc, MaybeSendBoxFuture, sleep},
};

use super::{
	ExpansionLimit, ExtractRoot, ExtractSkipReason,
	codec::ArchiveEnd,
	report::{
		ArchiveEntryId, ExtractActiveFile, ExtractEvent, ExtractFailed, ExtractMisleadingName,
		ExtractPhase, ExtractRenameReason, ExtractRenamedEntry, ExtractReport, ExtractSkippedEntry,
		ExtractTopLevelKey, ExtractedTopLevel, Reporter,
	},
};

use dirs::{DirState, Dirs, Opened};
use links::Links;

/// Directories planned and not created yet past which the codec is kept waiting: an archive
/// naming directories faster than they are created (each entry can imply 256) has them planned
/// only as fast as they are created.
const MAX_UNCREATED_DIRS: usize = 16 * MAX_SMALL_PARALLEL_REQUESTS;

/// File entries open (reading, uploading or waiting to be registered) past which the codec is
/// kept waiting: an archive of empty or tiny files is read only as fast as they are registered.
const MAX_OPEN_FILES: usize = 4 * MAX_SMALL_PARALLEL_REQUESTS;

/// What the codec returns.
pub(crate) type CodecResult = Result<ArchiveEnd, Error>;

/// What [`run_extract`] needs.
pub(crate) struct ExtractTask<B> {
	pub(crate) backend: Arc<B>,
	pub(crate) control: JobControl,
	pub(crate) reporter: MaybeArc<Reporter>,
	pub(crate) archive: RemoteFileType<'static>,
	pub(crate) destination: DirType<'static, Normal>,
	pub(crate) root: ExtractRoot,
	pub(crate) max_bytes: Option<u64>,
	pub(crate) max_items: Option<u64>,
	/// What a tar's hard links may copy (see [`ExpansionLimit`]).
	pub(crate) expansion: Option<ExpansionLimit>,
	/// For a partial extraction, the directory of the archive its entries land relative to
	/// (the codec strips it from their paths); empty otherwise.
	pub(crate) base: Vec<ValidatedName>,
	pub(crate) config: ArchiveConfig,
	/// Starts the codec; called once the job holds its lease.
	pub(crate) start: CodecStart<CodecResult>,
	/// Whether the caller asked for the archive to be removed: every way the job ends then
	/// reports what became of it.
	pub(crate) dispose: Option<ArchiveDisposal>,
}

/// What to do with the archive once the extraction is verified.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ArchiveDisposal {
	/// Remove it this way, if it is still in `parent`.
	Remove { how: SourceDisposal, parent: Uuid },
	/// It was in the trash when the job started, so there is no directory to confirm it is
	/// still in: it is kept.
	Unavailable,
}

/// Where a file entry is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FilePhase {
	/// Its data is still coming: from the codec, or copied from a hard link's target.
	Receiving,
	/// All its data has come.
	Ended,
	/// It is being registered.
	Finalizing,
	/// It failed; its later data is dropped.
	Failed {
		/// All its data has come.
		ended: bool,
	},
}

impl FilePhase {
	/// The phase once all the file's data has come.
	fn end(self) -> Self {
		match self {
			Self::Receiving => Self::Ended,
			Self::Failed { .. } => Self::Failed { ended: true },
			ended @ (Self::Ended | Self::Finalizing) => ended,
		}
	}
}

/// Where a file slot's data comes from.
enum SlotSource {
	/// The codec, which sends it next.
	Codec,
	/// A copy of another file, for a hard link.
	Link(LinkPhase),
}

/// A hard link's copy of the file it names: that file, fetched by its uuid, then its chunks one
/// at a time (their hash is taken in order), uploaded as the link's.
enum LinkPhase {
	/// The file is being fetched by its uuid.
	Resolving,
	/// No chunk of the file is being fetched.
	Idle(Arc<RemoteFileType<'static>>),
	/// A chunk of the file is being fetched.
	Fetching(Arc<RemoteFileType<'static>>),
}

/// A file entry being extracted.
struct FileSlot<U> {
	path: String,
	parent: DirId,
	upload: Arc<U>,
	active: ExtractActiveFile,
	name: ValidatedName,
	hasher: blake3::Hasher,
	written: u64,
	next_index: u64,
	uploading: usize,
	info: Option<RemoteFileInfo>,
	modified: Option<DateTime<Utc>>,
	phase: FilePhase,
	/// What a tar's hard links find it by, when it may be the target of one.
	link_key: Option<u64>,
	source: SlotSource,
}

impl<U> FileSlot<U> {
	fn failed(&self) -> bool {
		matches!(self.phase, FilePhase::Failed { .. })
	}

	/// Its size: as the archive states it, or else what was read of it.
	fn bytes(&self) -> u64 {
		self.active.size.unwrap_or(self.written)
	}

	/// The name the archive gives it: the last segment of its path.
	fn archive_name(&self) -> &str {
		self.path
			.rsplit_once('/')
			.map_or(&*self.path, |(_, name)| name)
	}
}

/// A file entry, as [`Driver::open_file`] takes it, with a name free in its directory.
struct NewFile {
	ordinal: u64,
	entry: ArchiveEntryId,
	path: String,
	parent: DirId,
	name: ValidatedName,
	size: Option<u64>,
	modified: Option<DateTime<Utc>>,
	source: FileSource,
	/// What a tar's hard links find it by, when it may be the target of one.
	link_key: Option<u64>,
}

/// Where a file's data comes from.
enum FileSource {
	/// The codec, which sends it next.
	Codec,
	/// A copy of the registered file `target`, for a hard link.
	Link { target: Uuid },
}

/// A hard link's target fetched by its uuid, for the link's file `ordinal`.
type LinkSource = (u64, Result<NonRootItemType<'static, Normal>, Error>);

/// An uploaded chunk of a file, by the file's ordinal.
type UploadedChunk = (u64, u64, Result<RemoteFileInfo, Error>);

/// A file's registration, by the file's ordinal; one a pause came first to is started again on
/// resume.
type Registration = (u64, UnlessPaused);

struct Driver<B: DriveBackend> {
	backend: Arc<B>,
	control: JobControl,
	reporter: MaybeArc<Reporter>,
	archive: Arc<RemoteFileType<'static>>,
	destination: DirType<'static, Normal>,
	root: ExtractRoot,
	max_bytes: Option<u64>,
	max_items: Option<u64>,
	config: ArchiveConfig,
	feed: CodecFeed<B, ArchiveEnd>,
	/// The floor's output chunk; its input chunk is the feed's.
	output_slot: Arc<Semaphore>,
	/// The client's file-IO budget, for what goes beyond the floor.
	memory: Arc<Semaphore>,
	targets: Arc<ConnectedTargets>,
	dispose: Option<ArchiveDisposal>,

	/// Set up when the codec reports what the archive holds.
	opened: Option<Opened>,
	/// Entries land in the destination itself, so items at the top are created in a directory
	/// the job did not create.
	into_destination: bool,
	/// The destination listing could not name every item.
	unverified: bool,
	dirs: Dirs,
	/// Open file entries by ordinal, so ready ones register in archive order.
	files: BTreeMap<u64, FileSlot<B::Upload>>,
	/// The file receiving data.
	current: Option<u64>,
	/// A tar's hard links.
	links: Links,
	expansion: Option<ExpansionLimit>,
	base: Vec<ValidatedName>,
	uploads: FuturesUnordered<MaybeSendBoxFuture<'static, UploadedChunk>>,
	finalizes: FuturesUnordered<MaybeSendBoxFuture<'static, Registration>>,
	/// Data of the current file the driver cannot take on yet, and so the last event it took:
	/// while it waits, the codec parks.
	held: Option<Vec<u8>>,
	/// The codec returned, or was given up on.
	codec_done: bool,
	/// The order-free digest of the files and directories the job created, which the output
	/// has to hold exactly before the archive is removed.
	created_digest: u128,
	/// Top-level items past the report's records, by uuid and whether each is a directory:
	/// what propagating them to targets the destination gains takes.
	top_level_beyond: Vec<(Uuid, bool)>,
	/// Entries the archive holds no checksum for, which were extracted unverified.
	unchecked_entries: u64,
	/// Entries skipped on purpose, as macOS metadata: they keep nothing from removing the archive.
	left_out: u64,
	/// Plaintext bytes committed to uploads, for `max_bytes`.
	committed: u64,
	items: u64,

	report: ExtractReport,
	fatal: Fatal,
}

/// Runs an extraction: waits for a job slot, starts the codec, and drives it to the end.
pub(crate) async fn run_extract<B: DisposalBackend>(
	task: ExtractTask<B>,
) -> Result<ExtractReport, ExtractFailed> {
	let ExtractTask {
		backend,
		control,
		reporter,
		archive,
		destination,
		root,
		max_bytes,
		max_items,
		expansion,
		base,
		config,
		start,
		dispose,
	} = task;
	let report = ExtractReport::new(archive.size());
	let archive_uuid = archive.uuid();
	// ended before anything was extracted: the archive to remove is kept
	let fail = |mut report: ExtractReport, phase, error| {
		if dispose.is_some() {
			report_kept_on_early_end(&reporter, &mut report, archive_uuid, phase);
		}
		reporter.finish(phase);
		ExtractFailed {
			report: ExtractReport {
				counts: reporter.counts(),
				..report
			},
			error,
		}
	};

	let archive = Arc::new(archive);
	let started = start_reading(
		Arc::clone(&backend),
		Arc::clone(&archive),
		ReadingJob {
			config: &config,
			control: &control,
			reporter: &reporter,
			reading: ExtractPhase::Scanning,
			name: ExtractReport::NAME,
		},
		start,
	)
	.await;
	let (_lease, feed) = match started {
		Ok(started) => started,
		Err((phase, error)) => return Err(fail(report, phase, error)),
	};

	let memory = backend.memory();
	let mut driver = Driver {
		feed,
		backend,
		control,
		reporter,
		archive,
		destination,
		root,
		max_bytes,
		max_items,
		config,
		output_slot: Arc::new(Semaphore::new(1)),
		memory,
		targets: Arc::default(),
		dispose,
		opened: None,
		into_destination: false,
		unverified: false,
		dirs: Dirs::default(),
		files: BTreeMap::new(),
		links: Links::default(),
		expansion,
		base,
		current: None,
		uploads: FuturesUnordered::new(),
		finalizes: FuturesUnordered::new(),
		held: None,
		codec_done: false,
		top_level_beyond: Vec::new(),
		created_digest: 0,
		unchecked_entries: 0,
		left_out: 0,
		committed: 0,
		items: 0,
		report,
		fatal: Fatal::default(),
	};
	let result = driver.run().await;
	let Driver {
		mut report,
		reporter,
		..
	} = driver;
	report.counts = reporter.counts();
	match result {
		Ok(()) => Ok(report),
		Err(error) => Err(ExtractFailed { report, error }),
	}
}

/// Records what became of the archive in `report`, and tells of it.
fn report_disposition(
	reporter: &Reporter,
	report: &mut ExtractReport,
	disposition: SourceDisposition,
) {
	reporter.event(ExtractEvent::SourceDisposition(disposition.clone()));
	report.dispositions.push(disposition);
}

/// Records and tells that the archive to remove is kept by a job that ended early, in `phase`.
fn report_kept_on_early_end(
	reporter: &Reporter,
	report: &mut ExtractReport,
	archive: Uuid,
	phase: ExtractPhase,
) {
	for disposition in kept_on_early_end(&[archive], phase == ExtractPhase::Cancelled) {
		report_disposition(reporter, report, disposition);
	}
}

/// Records `record` in `list`, or counts it in `omitted` past the records a report keeps (see
/// [`keep`]); `record` back for its event while the report keeps them.
fn record<T: Clone>(list: &mut Vec<T>, omitted: &mut u64, record: T) -> Option<T> {
	keep(list, omitted, record.clone()).then_some(record)
}

impl<B: DisposalBackend> Driver<B> {
	/// `Err` with [`ErrorKind::Cancelled`] when cancelled, or the error that ended the job.
	async fn run(&mut self) -> Result<(), Arc<Error>> {
		let outcome = async {
			self.extract().await?;
			self.feed.release();
			if self.fatal.error().is_some() {
				return Ok(());
			}
			self.reporter.set_phase(ExtractPhase::Finishing);
			self.recheck_targets().await?;
			if let Some(ArchiveDisposal::Remove { how, parent }) = self.dispose {
				self.reporter.set_phase(ExtractPhase::DisposingSources);
				self.reporter.checkpoint(&self.control).await?;
				let outcome = self.dispose_archive(how, parent).await;
				let disposition = SourceDisposition {
					uuid: self.archive.uuid(),
					outcome,
				};
				report_disposition(&self.reporter, &mut self.report, disposition);
			}
			Ok(())
		}
		.await;
		let (phase, result) = self.fatal.end(outcome, &self.control, ExtractReport::NAME);
		// a cancel once everything was extracted keeps the archive, but the job is done; it still
		// ends as cancelled jobs do, however late it was seen
		if result.is_ok() && self.control.is_cancelled() {
			self.reporter.set_cancelling();
		}
		// the archive to remove was not touched: say so, rather than leave its disposition out
		if self.dispose.is_some() && self.report.dispositions.is_empty() {
			let archive = self.archive.uuid();
			if result.is_ok() {
				// only an unavailable archive, one in the trash, is not removed after a complete
				// extraction
				let reason = if self.complete(self.reporter.counts()) {
					KeptReason::Changed
				} else {
					KeptReason::Incomplete
				};
				let disposition = SourceDisposition {
					uuid: archive,
					outcome: DisposalOutcome::kept(reason),
				};
				report_disposition(&self.reporter, &mut self.report, disposition);
			} else {
				report_kept_on_early_end(&self.reporter, &mut self.report, archive, phase);
			}
		}
		if result.is_err() {
			// planned and never started: a job that ends early leaves them
			let unattempted = self
				.dirs
				.slots
				.iter()
				.filter(|dir| matches!(dir.state, DirState::Planned))
				.count();
			self.reporter.dirs_not_attempted(unattempted as u64);
		}
		if let Err(error) = &result
			&& error.kind() == ErrorKind::ArchiveWrongPassword
			&& self.reporter.counts().files_done == 0
		{
			self.trash_created_dirs().await;
		}
		self.reporter.finish(phase);
		result
	}

	/// Ends the job with `error` when an entry's error is one nothing can succeed after.
	fn note_error(&mut self, error: &Arc<Error>) {
		self.fatal.note(error, &self.control, &*self.reporter);
	}

	/// Ends the job with `error`, unless an earlier error already did.
	fn stop_with(&mut self, error: Error) {
		self.fatal
			.stop(Arc::new(error), &self.control, &*self.reporter);
	}

	fn entry_id(&self, ordinal: u64) -> ArchiveEntryId {
		ArchiveEntryId::of(self.archive.uuid(), ordinal)
	}

	async fn extract(&mut self) -> Result<(), Stopped> {
		loop {
			let pause_requested = self.control.is_pause_requested();
			self.reporter.set_pause_requested(pause_requested);
			let stopping = self.control.is_stopping();
			if stopping {
				self.drop_transfers();
			} else if !pause_requested {
				self.advance();
			}
			if self.finished(stopping) {
				return if stopping { Err(Stopped) } else { Ok(()) };
			}
			if pause_requested && !stopping && self.idle() {
				self.pause().await?;
				// finalizes held back while pausing
				self.finalize_ready();
				continue;
			}
			let take_events =
				!stopping && !pause_requested && self.held.is_none() && !self.backlogged();

			tokio::select! {
				biased;
				() = self.control.stopping(), if !stopping => {},
				() = self.control.pause_changed(pause_requested) => {},
				Some((ordinal, len, result)) = self.uploads.next(), if !self.uploads.is_empty() => {
					self.upload_finished(ordinal, len, result);
				}
				Some((ordinal, result)) = self.finalizes.next(), if !self.finalizes.is_empty() => {
					self.finalize_finished(ordinal, result);
				}
				Some((dir, result)) = self.dirs.creates.next(), if !self.dirs.creates.is_empty() => {
					self.dir_finished(dir, result);
				}
				Some((ordinal, result)) = self.links.sources.next(), if !self.links.sources.is_empty() => {
					self.link_source_fetched(ordinal, result);
				}
				Some(chunk) = self.links.chunks.next(), if !self.links.chunks.is_empty() => {
					self.link_chunk_fetched(chunk);
				}
				fed = self.feed.next(take_events, !stopping) => match fed {
					Fed::Fetched(result) => {
						if let Err(error) = result {
							self.stop_with(error);
						}
						self.report_bytes_read();
					}
					Fed::Asked => self.report_bytes_read(),
					Fed::Event(event) => self.on_event(event).await?,
					Fed::EventsClosed => {}
					Fed::Finished(result) => self.codec_finished(result),
				},
				() = sleep(CALLBACK_INTERVAL) => self.tick(pause_requested),
			}
		}
	}

	/// Whether nothing is left to do: the codec ended and everything it produced is settled, or,
	/// when stopping, nothing that must finish is still running.
	fn finished(&self, stopping: bool) -> bool {
		let settled = self.dirs.creates.is_empty() && self.finalizes.is_empty();
		if stopping {
			return settled;
		}
		// a directory can be left to start only while a pause is requested, which starts none
		settled
			&& self.dirs.ready.is_empty()
			&& self.uploads.is_empty()
			&& self.held.is_none()
			&& self.files.is_empty()
			&& self.links.ready.is_empty()
			&& self.codec_done
	}

	/// Whether the codec waits for what it sent so far to be worked off first: hard links not
	/// opened yet count as open files.
	fn backlogged(&self) -> bool {
		self.dirs.uncreated >= MAX_UNCREATED_DIRS || self.open_files() >= MAX_OPEN_FILES
	}

	/// Files open, and hard links taken on that will be.
	fn open_files(&self) -> usize {
		self.files.len() + self.links.waiting_count + self.links.ready.len()
	}

	fn idle(&self) -> bool {
		self.uploads.is_empty()
			&& !self.feed.fetching()
			&& self.links.sources.is_empty()
			&& self.links.chunks.is_empty()
			&& self.dirs.creates.is_empty()
			&& self.finalizes.is_empty()
	}

	/// Waits out a pause holding nothing: prefetched chunks are given back.
	async fn pause(&mut self) -> Result<(), Stopped> {
		self.feed
			.wait_out_pause(&self.reporter, &self.control)
			.await
	}

	/// Drops the transfers in flight; the files they belonged to are abandoned.
	fn drop_transfers(&mut self) {
		self.reporter.wind_down(&self.control);
		self.feed.drop_all();
		self.held = None;
		self.links.sources = FuturesUnordered::new();
		self.links.chunks = FuturesUnordered::new();
		self.drop_taken_links();
		if !self.uploads.is_empty() {
			self.uploads = FuturesUnordered::new();
		}
		let reporter = &self.reporter;
		self.files.retain(|_, file| {
			if file.phase == FilePhase::Finalizing {
				return true;
			}
			if !file.failed() {
				reporter.file_abandoned(file.active.dest_uuid, file.bytes());
			}
			false
		});
		self.current = None;
	}

	/// Starts whatever can start without waiting: answers the codec, prefetches, creates the
	/// directories whose parents exist, takes up held data, registers finished files.
	fn advance(&mut self) {
		self.feed.advance(&self.reporter.ops());
		self.report_bytes_read();
		while self.dirs.creates.len() < MAX_SMALL_PARALLEL_REQUESTS
			&& let Some(dir) = self.dirs.ready.pop_front()
		{
			self.start_dir(dir);
		}
		if let Some(data) = self.held.take() {
			self.take_data(data);
		}
		self.open_ready_links();
		self.copy_links();
		// files held back while a pause was requested: a pause lifted before the job went idle
		// has no resume of its own to start them
		self.finalize_ready();
	}

	/// Reports the bytes of the archive the codec has read, which its inputs count once each: as
	/// the codec reads, not only on the idle ticks, which a busy job skips.
	fn report_bytes_read(&mut self) {
		let read = self.feed.bytes_read();
		// every chunk fed was checked to hold exactly what the archive's size gives it
		if read > self.archive.size() {
			self.stop_with(Error::custom(
				ErrorKind::Internal,
				format!(
					"the codec read {read} bytes of a {}-byte archive",
					self.archive.size()
				),
			));
			return;
		}
		self.reporter.set_bytes_read(read);
	}

	fn tick(&mut self, pause_requested: bool) {
		self.report_bytes_read();
		self.reporter.tick();
		let owed = self.held.is_some() || self.backlogged() || pause_requested;
		if self.feed.give_up_if_stalled(owed) {
			self.codec_done = true;
			self.stop_with(worker_died());
		}
	}

	fn codec_finished(&mut self, result: CodecResult) {
		self.codec_done = true;
		match result {
			Ok(end) => {
				self.report.unaccounted_bytes = end.unaccounted_bytes;
				self.report.duplicates = end.duplicates;
				self.unchecked_entries = end.unchecked_entries;
			}
			Err(error) => {
				// an error the driver caused (it failed a fetch, or stopped) is already the job's
				let ended = self.fatal.error().is_some() || self.control.is_stopping();
				// The archive is damaged from here on, but what came before it is whole: the files
				// whose data is complete still finish, and only the one being read is dropped.
				if codec_failed(self.archive.uuid(), &error, ended) {
					self.fatal.record(Arc::new(error));
					self.held = None;
					if let Some(ordinal) = self.current.take()
						&& let Some(file) = self.files.remove(&ordinal)
						&& !file.failed()
					{
						self.reporter
							.file_abandoned(file.active.dest_uuid, file.bytes());
					}
				}
			}
		}
	}

	async fn on_event(&mut self, event: WorkerEvent) -> Result<(), Stopped> {
		match event {
			WorkerEvent::Opened(layout) => self.open(layout).await?,
			WorkerEvent::Entry(head) => self.on_entry(head),
			WorkerEvent::Skipped(member) => self.on_skipped(member),
			WorkerEvent::Data(data) => self.take_data(data),
			WorkerEvent::FileEnd => self.end_file(),
			WorkerEvent::Link(link) => self.on_link(*link),
			// the feed answers asks, only a compressing codec sends a head, and only a listing's
			// lists
			WorkerEvent::Ask { .. } | WorkerEvent::Head(_) | WorkerEvent::Listed(_) => {
				self.stop_with(unexpected_event());
			}
		}
		Ok(())
	}

	fn top_level_created(
		&mut self,
		key: ExtractTopLevelKey,
		item: NonRootItemType<'static, Normal>,
	) {
		let is_dir = matches!(item, NonRootItemType::Dir(_));
		let top = ExtractedTopLevel { key, item };
		if !keep(
			&mut self.report.top_level,
			&mut self.report.omitted.top_level,
			top.clone(),
		) {
			self.top_level_beyond.push((top.item.uuid(), is_dir));
		}
		self.reporter.top_level_created(top);
	}

	fn report_propagation(&self, dest_uuid: Uuid, errors: Vec<Error>) {
		for error in errors {
			self.reporter.event(ExtractEvent::PropagationFailed {
				dest_uuid,
				error: Arc::new(error),
			});
		}
	}

	fn renamed(
		&mut self,
		entry: ArchiveEntryId,
		path: String,
		name: &ValidatedName,
		reason: ExtractRenameReason,
	) {
		let renamed = ExtractRenamedEntry {
			entry,
			path,
			name: name.as_ref().to_owned(),
			reason,
		};
		if let Some(renamed) = record(
			&mut self.report.renamed,
			&mut self.report.omitted.renamed,
			renamed,
		) {
			self.reporter.event(ExtractEvent::Renamed(renamed));
		}
	}

	/// Counts an item against `max_items`; `false` once that ended the job.
	fn count_item(&mut self) -> bool {
		self.items += 1;
		if let Some(max) = self.max_items
			&& exceeds_limit(self.items, max)
		{
			self.stop_with(Error::custom(
				ErrorKind::ArchiveTooLarge,
				format!("the archive holds more than {max} items"),
			));
			return false;
		}
		true
	}

	fn on_skipped(&mut self, member: SkippedMember) {
		if member.reason == ExtractSkipReason::MacMetadata {
			self.left_out += 1;
		}
		let skipped = ExtractSkippedEntry {
			entry: self.entry_id(member.ordinal),
			path: member.path,
			path_truncated: member.path_truncated,
			bytes: member.bytes,
			reason: member.reason,
		};
		let kept = record(
			&mut self.report.skipped,
			&mut self.report.omitted.skipped,
			skipped,
		);
		self.reporter.skipped(member.bytes, kept);
	}

	fn on_entry(&mut self, head: EntryHead) {
		let entry = self.entry_id(head.ordinal);
		self.report_path(entry, &head.path);
		match head.kind {
			EntryKind::Dir => {
				self.resolve_dirs(head.path.segments(), entry, head.modified);
			}
			EntryKind::File { size } => {
				let Some(mut file) = self.new_file(head.ordinal, &head.path, size, head.modified)
				else {
					return;
				};
				// a tar's hard links name the files before them by path
				if matches!(self.opened().layout, ArchiveFormat::Tar { .. }) {
					file.link_key = Some(self.link_key(&head.path));
				}
				let (ordinal, link_key) = (file.ordinal, file.link_key);
				self.open_file(file);
				if let Some(key) = link_key {
					self.link_target(ordinal, key);
				}
			}
		}
	}

	/// Reports what an entry at `path` that is taken on is named: a path made into valid drive
	/// names, and a name that reads as something it is not.
	fn report_path(&mut self, entry: ArchiveEntryId, path: &ArchivePath) {
		let joined = self.archive_joined(path.segments());
		if path.rewritten {
			let (name, _) = path.split_last();
			self.renamed(
				entry,
				joined.clone(),
				name,
				ExtractRenameReason::PathRewritten,
			);
		}
		if path.suspicious {
			let misleading = ExtractMisleadingName {
				entry,
				path: joined,
			};
			if let Some(misleading) = record(
				&mut self.report.misleading_names,
				&mut self.report.omitted.misleading_names,
				misleading,
			) {
				self.reporter
					.event(ExtractEvent::MisleadingName(misleading));
			}
		}
	}

	/// `segments`, a path below the extraction's root, as the path of the archive it is: the
	/// base of a partial extraction first.
	fn archive_joined(&self, segments: &[ValidatedName]) -> String {
		joined(self.base.iter().chain(segments))
	}

	/// A file entry at `path`: its directories planned and a free name taken for it in the last;
	/// `None` once the job ended.
	fn new_file(
		&mut self,
		ordinal: u64,
		path: &ArchivePath,
		size: Option<u64>,
		modified: Option<DateTime<Utc>>,
	) -> Option<NewFile> {
		let entry = self.entry_id(ordinal);
		let (name, parents) = path.split_last();
		let parent = self.resolve_dirs(parents, entry, None)?;
		if !self.count_item() {
			return None;
		}
		let allocated = self.opened_mut().resolver.file_name(parent, name.clone());
		// a keep-both name is reported once the file is registered, under the name it got then
		match allocated {
			Ok(name) => Some(NewFile {
				ordinal,
				entry,
				path: self.archive_joined(path.segments()),
				parent,
				name,
				size,
				modified,
				source: FileSource::Codec,
				link_key: None,
			}),
			Err(error) => {
				self.stop_with(error.into());
				None
			}
		}
	}
}

#[cfg(test)]
mod tests;
