//! Runs an extraction of an archive of any format (a tar, compressed or not, a single compressed
//! file, a zip or a 7z): the async driver of the codec worker. It feeds the codec the archive,
//! creates the directories and uploads the files the codec reads out of it, in the order the
//! codec reads them, copies a tar's hard links from the files they name, and registers each file
//! once its data is up.
//!
//! # Memory and progress
//!
//! A job holds a floor of two chunks of the archive memory ([`ArchiveConfig`]) for as long as it
//! runs: one for the chunk of the archive the codec is reading, one for a chunk of output
//! uploading. More (prefetched input, more uploads at once) is only taken when the client's
//! file-IO budget has it free right now, never waited for, so a job always makes progress on its
//! floor, and never waits on memory a transfer or another job holds. What the codec itself holds
//! (its decoder state, the output chunk it is filling, the one in the channel) is bounded by the
//! codec budget and three chunks.
//!
//! # Pause and cancel
//!
//! Pausing stops everything new: no chunk fetched or uploaded, no directory created, no event
//! taken from the codec, which parks. In-flight work finishes; then the job gives back its floor
//! and prefetched chunks and reports itself paused, holding no drive lock and no memory
//! reservation. The codec keeps its own state resident while paused, and so the job keeps its
//! slot; a job paused before it got one waits without taking it.
//!
//! Cancelling drops the transfers in flight at once (a file only becomes visible when it is
//! registered) but lets directory creates and registrations in flight finish, so every item
//! that was created is known and reported.

mod dirs;
mod files;
mod finish;
mod links;

use std::{
	collections::{BTreeMap, HashMap, VecDeque},
	sync::Arc,
};

use chrono::{DateTime, Utc};
use filen_types::fs::Uuid;
use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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
			input::{CodecFeed, Fed, start_reading},
			names::{DirId, PathResolver},
			worker::{
				EntryHead, EntryKind, SkippedMember, WorkerEvent, WorkerLink, codec_failed,
				worker_died,
			},
		},
		categories::{DirType, NonRootItemType, Normal},
		drive_job::{
			Fatal,
			backend::DriveBackend,
			dir::{CreatedDirOutcome, DirError},
			finalize::{FinalizeError, Finalized},
		},
		file::{enums::RemoteFileType, traits::HasFileInfo, write::RemoteFileInfo},
		name::ValidatedName,
	},
	job::{
		JobControl, Stopped,
		report::{JobReport, OpGuard},
	},
	util::{MaybeArc, MaybeSendBoxFuture, sleep},
};

use super::{
	ExpansionLimit, ExtractRoot, ExtractSkipReason,
	codec::ArchiveEnd,
	report::{
		ArchiveEntryId, ExtractActiveFile, ExtractEvent, ExtractFailed, ExtractMisleadingName,
		ExtractPhase, ExtractRenameReason, ExtractRenamedEntry, ExtractReport, ExtractSkippedEntry,
		ExtractTopLevelKey, ExtractedTopLevel, Reporter, keep,
	},
};

use dirs::{DirSlot, DirState};
use links::{LinkCopy, LinkTargets, PendingLink, TakenLink};

/// Chunks of one file uploading at once.
const CHUNKS_PER_FILE: usize = 4;

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
	/// Starts the codec; called once the job holds its lease and memory floor.
	pub(crate) start: Box<dyn FnOnce() -> Result<WorkerLink<CodecResult>, Error> + Send>,
	/// How to remove the archive once the extraction is verified, and the directory it is in.
	pub(crate) dispose: Option<(SourceDisposal, Uuid)>,
	/// Whether the caller asked for the archive to be removed, which `dispose` leaves out for an
	/// archive in the trash: every way the job ends reports what became of it.
	pub(crate) disposal_requested: bool,
}

/// A file entry being extracted.
struct FileSlot<U> {
	entry: ArchiveEntryId,
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
	/// All its data has come in: from the codec, or copied from a hard link's target.
	ended: bool,
	finalizing: bool,
	failed: bool,
	/// What a tar's hard links find it by, when it may be the target of one.
	link_key: Option<u64>,
	/// For a hard link, the file it copies.
	copy: Option<LinkCopy>,
}

impl<U> FileSlot<U> {
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
}

/// Where a file's data comes from.
enum FileSource {
	/// The codec, which sends it next.
	Codec,
	/// A copy of the registered file `target`, for a hard link.
	Link { target: Uuid },
}

/// A chunk of a hard link's target, fetched for the link's file `ordinal`.
type LinkChunk = (u64, Result<Vec<u8>, Error>, OwnedSemaphorePermit, OpGuard);

/// A hard link's target fetched by its uuid, for the link's file `ordinal`.
type LinkSource = (u64, Result<NonRootItemType<'static, Normal>, Error>);

/// An uploaded chunk of a file, by the file's ordinal.
type UploadedChunk = (u64, u64, Result<RemoteFileInfo, Error>);

/// A file's registration, by the file's ordinal; `None` when a pause came first, to be started
/// again on resume.
type Registration = (u64, Option<Result<Finalized, FinalizeError>>);

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
	/// The floor's output chunk; its input chunk is the input's.
	output_slot: Arc<Semaphore>,
	/// The client's file-IO budget, for what goes beyond the floor.
	memory: Arc<Semaphore>,
	targets: Arc<ConnectedTargets>,
	/// What the archive turned out to hold.
	layout: Option<ArchiveFormat>,
	dispose: Option<(SourceDisposal, Uuid)>,
	disposal_requested: bool,

	/// Set up when the codec reports what the archive holds.
	resolver: Option<PathResolver>,
	/// Entries land in the destination itself, so items at the top are created in a directory
	/// the job did not create.
	into_destination: bool,
	/// The destination listing could not name every item.
	unverified: bool,
	dirs: Vec<DirSlot>,
	/// The directory entries land in, set up with the root's slot: the destination, or the
	/// folder the job created in it.
	root_dir: Option<DirType<'static, Normal>>,
	/// Directories planned and neither created nor failed.
	uncreated_dirs: usize,
	ready_dirs: VecDeque<DirId>,
	dir_creates:
		FuturesUnordered<MaybeSendBoxFuture<'static, (DirId, Result<CreatedDirOutcome, DirError>)>>,
	/// Open file entries by ordinal, so ready ones register in archive order.
	files: BTreeMap<u64, FileSlot<B::Upload>>,
	/// The file receiving data.
	current: Option<u64>,
	/// The files a tar's hard links may name: kept for a tar only, whose links name files by
	/// path.
	link_targets: LinkTargets,
	/// Hard links waiting for the file they name to be registered, by that file's ordinal, and
	/// how many there are.
	waiting_links: HashMap<u64, Vec<PendingLink>>,
	links_waiting: usize,
	/// Hard links whose file is registered, opened only as fast as the files open before them
	/// are worked off: a thousand links to one file do not all start at once.
	ready_links: VecDeque<(TakenLink, Uuid)>,
	/// What the hard links taken on copy in all, charged against `expansion`.
	link_bytes: u64,
	expansion: Option<ExpansionLimit>,
	base: Vec<ValidatedName>,
	link_sources: FuturesUnordered<MaybeSendBoxFuture<'static, LinkSource>>,
	link_chunks: FuturesUnordered<MaybeSendBoxFuture<'static, LinkChunk>>,
	uploads: FuturesUnordered<MaybeSendBoxFuture<'static, UploadedChunk>>,
	finalizes: FuturesUnordered<MaybeSendBoxFuture<'static, Registration>>,
	/// An event the driver cannot take on yet, and so the last it took: while it waits, the
	/// codec parks.
	held: Option<WorkerEvent>,
	codec_result: Option<CodecResult>,
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
		disposal_requested,
	} = task;
	let report = ExtractReport::new(super::ArchiveTotals::Streaming {
		archive_bytes: archive.size(),
	});
	let archive_uuid = archive.uuid();
	// ended before anything was extracted: the archive to remove is kept
	let fail = |mut report: ExtractReport, phase, error| {
		if disposal_requested {
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
		&config,
		&control,
		&reporter,
		ExtractPhase::Scanning,
		start,
		ExtractReport::NAME,
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
		layout: None,
		dispose,
		disposal_requested,
		resolver: None,
		into_destination: false,
		unverified: false,
		dirs: Vec::new(),
		root_dir: None,
		uncreated_dirs: 0,
		ready_dirs: VecDeque::new(),
		dir_creates: FuturesUnordered::new(),
		files: BTreeMap::new(),
		link_targets: LinkTargets::default(),
		waiting_links: HashMap::new(),
		links_waiting: 0,
		ready_links: VecDeque::new(),
		link_bytes: 0,
		expansion,
		base,
		link_sources: FuturesUnordered::new(),
		link_chunks: FuturesUnordered::new(),
		current: None,
		uploads: FuturesUnordered::new(),
		finalizes: FuturesUnordered::new(),
		held: None,
		codec_result: None,
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
			if let Some((how, parent)) = self.dispose {
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
		if self.disposal_requested && self.report.dispositions.is_empty() {
			let archive = self.archive.uuid();
			if result.is_ok() {
				// only an archive in the trash is not removed after a complete extraction
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
		ArchiveEntryId {
			archive: self.archive.uuid(),
			// the member cap keeps ordinals far below u32::MAX
			index: u32::try_from(ordinal).unwrap_or(u32::MAX),
		}
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
				Some((dir, result)) = self.dir_creates.next(), if !self.dir_creates.is_empty() => {
					self.dir_finished(dir, result);
				}
				Some((ordinal, result)) = self.link_sources.next(), if !self.link_sources.is_empty() => {
					self.link_source_fetched(ordinal, result);
				}
				Some(chunk) = self.link_chunks.next(), if !self.link_chunks.is_empty() => {
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
		let settled = self.dir_creates.is_empty() && self.finalizes.is_empty();
		if stopping {
			return settled;
		}
		// a directory can be left to start only while a pause is requested, which starts none
		settled
			&& self.ready_dirs.is_empty()
			&& self.uploads.is_empty()
			&& self.held.is_none()
			&& self.files.is_empty()
			&& self.ready_links.is_empty()
			&& self.codec_result.is_some()
	}

	/// Whether the codec waits for what it sent so far to be worked off first: hard links not
	/// opened yet count as open files.
	fn backlogged(&self) -> bool {
		self.uncreated_dirs >= MAX_UNCREATED_DIRS || self.open_files() >= MAX_OPEN_FILES
	}

	/// Files open, and hard links taken on that will be.
	fn open_files(&self) -> usize {
		self.files.len() + self.links_waiting + self.ready_links.len()
	}

	fn idle(&self) -> bool {
		self.uploads.is_empty()
			&& !self.feed.fetching()
			&& self.link_sources.is_empty()
			&& self.link_chunks.is_empty()
			&& self.dir_creates.is_empty()
			&& self.finalizes.is_empty()
	}

	/// Waits out a pause holding nothing: prefetched chunks and the floor are given back.
	async fn pause(&mut self) -> Result<(), Stopped> {
		self.feed
			.wait_out_pause(&self.reporter, &self.control, &self.config)
			.await
	}

	/// Drops the transfers in flight; the files they belonged to are abandoned.
	fn drop_transfers(&mut self) {
		self.reporter.wind_down(&self.control);
		self.feed.drop_all();
		self.held = None;
		self.link_sources = FuturesUnordered::new();
		self.link_chunks = FuturesUnordered::new();
		self.drop_taken_links();
		if !self.uploads.is_empty() {
			self.uploads = FuturesUnordered::new();
		}
		let abandoned: Vec<u64> = self
			.files
			.iter()
			.filter(|(_, file)| !file.finalizing)
			.map(|(ordinal, _)| *ordinal)
			.collect();
		for ordinal in abandoned {
			if let Some(file) = self.files.remove(&ordinal)
				&& !file.failed
			{
				self.reporter
					.file_abandoned(file.active.dest_uuid, file.bytes());
			}
		}
		self.current = None;
	}

	/// Starts whatever can start without waiting: answers the codec, prefetches, creates the
	/// directories whose parents exist, takes up a held event, registers finished files.
	fn advance(&mut self) {
		self.feed.advance(&self.reporter.ops());
		self.report_bytes_read();
		while self.dir_creates.len() < MAX_SMALL_PARALLEL_REQUESTS
			&& let Some(dir) = self.ready_dirs.pop_front()
		{
			self.start_dir(dir);
		}
		if let Some(event) = self.held.take() {
			self.retry_held(event);
		}
		self.open_ready_links();
		self.copy_links();
		// files held back while a pause was requested: a pause lifted before the job went idle
		// has no resume of its own to start them
		self.finalize_ready();
	}

	/// Reports the bytes of the archive the codec has read, which its inputs count once each: as
	/// the codec reads, not only on the idle ticks, which a busy job skips.
	fn report_bytes_read(&self) {
		let read = self.feed.bytes_read();
		debug_assert!(
			read <= self.archive.size(),
			"the codec read {read} bytes of a {}-byte archive",
			self.archive.size()
		);
		self.reporter.set_bytes_read(read);
	}

	fn tick(&mut self, pause_requested: bool) {
		self.report_bytes_read();
		self.reporter.tick();
		let owed = self.held.is_some() || self.backlogged() || pause_requested;
		if self.feed.give_up_if_stalled(owed) {
			self.codec_result = Some(Err(worker_died()));
			self.stop_with(worker_died());
		}
	}

	fn codec_finished(&mut self, result: CodecResult) {
		match &result {
			Ok(end) => {
				self.report.unaccounted_bytes = end.unaccounted_bytes;
				self.report.duplicates = end.duplicates.clone();
				self.unchecked_entries = end.unchecked_entries;
			}
			Err(error) => {
				// an error the driver caused (it failed a fetch, or stopped) is already the job's
				let ended = self.fatal.error().is_some() || self.control.is_stopping();
				// The archive is damaged from here on, but what came before it is whole: the files
				// whose data is complete still finish, and only the one being read is dropped.
				if codec_failed(self.archive.uuid(), error, ended) {
					self.fatal
						.record(Arc::new(Error::custom(error.kind(), error.to_string())));
					self.held = None;
					if let Some(ordinal) = self.current.take()
						&& let Some(file) = self.files.remove(&ordinal)
						&& !file.failed
					{
						self.reporter
							.file_abandoned(file.active.dest_uuid, file.bytes());
					}
				}
			}
		}
		self.codec_result = Some(result);
	}

	async fn on_event(&mut self, event: WorkerEvent) -> Result<(), Stopped> {
		match event {
			WorkerEvent::Opened(layout) => {
				self.layout = Some(layout);
				self.open(layout).await?
			}
			WorkerEvent::Entry(head) => self.on_entry(head),
			WorkerEvent::Skipped(member) => self.on_skipped(member),
			event @ (WorkerEvent::Data(_) | WorkerEvent::FileEnd) => self.retry_held(event),
			WorkerEvent::Link(link) => self.on_link(*link),
			// the feed answers asks, only a compressing codec sends a head, and only a listing's
			// lists
			WorkerEvent::Ask { .. } | WorkerEvent::Head(_) | WorkerEvent::Listed(_) => {
				debug_assert!(false, "an extracting codec sent {event:?}");
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
			&& self.items > max
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
				self.resolve_dirs(&head.path.segments, entry, head.modified);
			}
			EntryKind::File { size } => {
				let Some(file) = self.new_file(head.ordinal, &head.path, size, head.modified)
				else {
					return;
				};
				let ordinal = file.ordinal;
				self.open_file(file);
				// a tar's hard links name the files before them by path
				if matches!(self.layout, Some(ArchiveFormat::Tar { .. })) {
					self.link_target(ordinal, &head.path);
				}
			}
		}
	}

	/// Reports what an entry at `path` that is taken on is named: a path made into valid drive
	/// names, and a name that reads as something it is not.
	fn report_path(&mut self, entry: ArchiveEntryId, path: &ArchivePath) {
		let joined = self.archive_joined(&path.segments);
		if path.rewritten
			&& let Some(name) = path.segments.last()
		{
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
		joined(&[&self.base[..], segments].concat())
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
		let (name, parents) = path.segments.split_last()?;
		let parent = self.resolve_dirs(parents, entry, None)?;
		if !self.count_item() {
			return None;
		}
		let allocated = self
			.resolver
			.as_mut()
			.expect("entries follow the archive's layout")
			.file_name(parent, name.clone());
		// a keep-both name is reported once the file is registered, under the name it got then
		match allocated {
			Ok(name) => Some(NewFile {
				ordinal,
				entry,
				path: self.archive_joined(&path.segments),
				parent,
				name,
				size,
				modified,
				source: FileSource::Codec,
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
