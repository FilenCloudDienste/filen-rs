//! Runs an extraction of a streaming archive: the async driver of the codec worker. It fetches
//! the archive for the codec, creates the directories and uploads the files the codec reads out
//! of it, in archive order, and registers each file once its data is up.
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

use std::{
	borrow::Cow,
	collections::{BTreeMap, HashSet, VecDeque},
	io,
	sync::Arc,
};

use chrono::{DateTime, Utc};
use filen_types::{api::v3::dir::color::DirColor, crypto::Blake3Hash, fs::Uuid};
use futures::{
	StreamExt,
	future::join_all,
	stream::{FuturesOrdered, FuturesUnordered},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::{
	Error, ErrorKind,
	connect::ConnectedTargets,
	consts::{CALLBACK_INTERVAL, CHUNK_SIZE_U64, MAX_SMALL_PARALLEL_REQUESTS},
	fs::{
		HasName, HasUUID,
		archive::{
			config::{ArchiveConfig, CHUNK_BYTES},
			dispose::{
				DisposalBackend, DisposalOutcome, ExpectedFile, KeptReason, SourceDisposal,
				SourceDisposition, Tree, dir_digest, dispose_file, file_digest, kept_on_early_end,
			},
			format::{ArchiveFormat, extract_folder_name},
			names::{DirId, PathResolver, PlannedDir, ROOT},
			worker::{
				ARCHIVE_STALL_TIMEOUT, EntryHead, EntryKind, SkippedMember, WorkerEvent, WorkerLink,
			},
		},
		categories::{DirType, NonRootItemType, Normal},
		drive_job::{
			Fatal,
			backend::{DriveBackend, UploadSpec},
			cancelled,
			counts::ItemCounts,
			dir::{CreatedDirOutcome, DirError, DirTask, create_dir},
			finalize::{FinalizeError, FinalizeTask, Finalized, finalize_new_file_unless_paused},
			lock::{LockWait, wait_for_lock},
			name_retry::NameRetry,
		},
		file::{
			enums::RemoteFileType,
			read::{check_chunks_consistent, chunk_plaintext_len},
			traits::{HasFileInfo, HasRemoteFileInfo},
			write::{RemoteFileInfo, UploadCompletion},
		},
		name::{
			ValidatedName,
			keep_both::{NameShape, TakenNames},
		},
	},
	job::{
		JobControl, Stopped,
		report::{JobReport, OpGuard},
	},
	util::{MaybeArc, MaybeSendBoxFuture, sleep},
};

use super::{
	ExtractRoot,
	codec::ArchiveEnd,
	report::{
		ArchiveEntryId, ExtractActiveFile, ExtractEvent, ExtractFailed, ExtractFailure,
		ExtractMisleadingName, ExtractPhase, ExtractRenameReason, ExtractRenamedEntry,
		ExtractReport, ExtractSkippedEntry, ExtractStage, ExtractTopLevelKey, ExtractedTopLevel,
		OmittedRecords, Reporter, keep,
	},
};

/// Chunks of one file uploading at once.
const CHUNKS_PER_FILE: usize = 4;

/// Chunks of the archive fetched ahead of the codec, memory permitting.
const PREFETCH_CHUNKS: usize = 4;

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
	pub(crate) config: ArchiveConfig,
	/// Starts the codec; called once the job holds its lease and memory floor.
	pub(crate) start: Box<dyn FnOnce() -> Result<WorkerLink<CodecResult>, Error> + Send>,
	/// How to remove the archive once the extraction is verified, and the directory it is in.
	pub(crate) dispose: Option<(SourceDisposal, Uuid)>,
	/// Whether the caller asked for the archive to be removed, which `dispose` leaves out for an
	/// archive in the trash: every way the job ends reports what became of it.
	pub(crate) disposal_requested: bool,
}

enum DirState {
	Planned,
	Creating,
	Created(Uuid),
	Failed(Arc<Error>),
}

/// A directory of the extraction; [`ROOT`] is the one entries land in.
struct DirSlot {
	uuid: Uuid,
	parent: DirId,
	/// The name it is created under.
	name: ValidatedName,
	/// The name the archive gave it, when `name` is a keep-both name instead.
	archive_name: Option<ValidatedName>,
	created: DateTime<Utc>,
	/// The entry that named it first.
	entry: ArchiveEntryId,
	state: DirState,
	children: Vec<DirId>,
}

impl DirSlot {
	fn created_uuid(&self) -> Option<Uuid> {
		match self.state {
			DirState::Created(uuid) => Some(uuid),
			_ => None,
		}
	}
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
	/// All its data has come from the codec.
	ended: bool,
	finalizing: bool,
	failed: bool,
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

/// A file entry, as [`Driver::open_file`] takes it.
struct NewFile {
	ordinal: u64,
	entry: ArchiveEntryId,
	path: String,
	parent: DirId,
	name: ValidatedName,
	size: Option<u64>,
	modified: Option<DateTime<Utc>>,
}

/// A fetched chunk of the archive, with the memory it holds.
type FetchedChunk = (u64, Result<Vec<u8>, Error>, OwnedSemaphorePermit, OpGuard);

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
	link: WorkerLink<CodecResult>,
	/// The memory floor while the job holds it, counted as an operation in flight: the job is
	/// only reported paused once it has given it back, with everything held on top of it.
	floor: Option<(OwnedSemaphorePermit, OpGuard)>,
	/// The floor's two chunks, as the input and the output slot.
	input_slot: Arc<Semaphore>,
	output_slot: Arc<Semaphore>,
	/// The client's file-IO budget, for what goes beyond the floor.
	memory: Arc<Semaphore>,
	targets: Arc<ConnectedTargets>,

	chunks: u64,
	next_fetch: u64,
	fetches: FuturesOrdered<MaybeSendBoxFuture<'static, FetchedChunk>>,
	/// Fetched chunks the codec has not asked for yet; held only with the floor, which counts
	/// them in flight.
	ready: VecDeque<(u64, Vec<u8>, OwnedSemaphorePermit)>,
	/// The chunk the codec is reading, released when it asks for the next.
	reading: Option<OwnedSemaphorePermit>,
	/// The chunk the codec reads next.
	served: u64,
	/// The archive's plaintext as the codec read it, in order.
	archive_hasher: blake3::Hasher,
	/// The codec read the archive front to back, once: `archive_hasher` covers it all.
	sequential: bool,
	/// What the archive turned out to hold.
	layout: Option<ArchiveFormat>,
	dispose: Option<(SourceDisposal, Uuid)>,
	disposal_requested: bool,
	ask: Option<(u64, oneshot::Sender<io::Result<Vec<u8>>>)>,

	/// Set up when the codec reports what the archive holds.
	resolver: Option<PathResolver>,
	/// Entries land in the destination itself, so items at the top are created in a directory
	/// the job did not create.
	into_destination: bool,
	/// The destination listing could not name every item.
	unverified: bool,
	dirs: Vec<DirSlot>,
	/// Directories planned and neither created nor failed.
	uncreated_dirs: usize,
	ready_dirs: VecDeque<DirId>,
	dir_creates:
		FuturesUnordered<MaybeSendBoxFuture<'static, (DirId, Result<CreatedDirOutcome, DirError>)>>,
	/// Open file entries by ordinal, so ready ones register in archive order.
	files: BTreeMap<u64, FileSlot<B::Upload>>,
	/// The file receiving data.
	current: Option<u64>,
	uploads: FuturesUnordered<MaybeSendBoxFuture<'static, UploadedChunk>>,
	finalizes: FuturesUnordered<MaybeSendBoxFuture<'static, Registration>>,
	/// An event the driver cannot take on yet, and so the last it took: while it waits, the
	/// codec parks.
	held: Option<WorkerEvent>,
	events_closed: bool,
	codec_result: Option<CodecResult>,
	/// The order-free digest of the files and directories the job created, which the output
	/// has to hold exactly before the archive is removed.
	created_digest: u128,
	/// Top-level items past the report's records, by uuid and whether each is a directory:
	/// what propagating them to targets the destination gains takes.
	top_level_beyond: Vec<(Uuid, bool)>,
	/// Entries the archive holds no checksum for, which were extracted unverified.
	unchecked_entries: u64,
	/// Plaintext bytes committed to uploads, for `max_bytes`.
	committed: u64,
	items: u64,
	/// The codec's progress stamp last seen, and for how many idle ticks it has not moved.
	stamp: u64,
	stalled_ticks: u32,

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
		config,
		start,
		dispose,
		disposal_requested,
	} = task;
	let totals = super::ArchiveTotals::Streaming {
		archive_bytes: archive.size(),
	};
	let report = ExtractReport {
		top_level: Vec::new(),
		failures: Vec::new(),
		skipped: Vec::new(),
		renamed: Vec::new(),
		misleading_names: Vec::new(),
		omitted: OmittedRecords::default(),
		totals,
		counts: Default::default(),
		unaccounted_bytes: 0,
		duplicates: None,
		dispositions: Vec::new(),
	};
	let archive_uuid = archive.uuid();
	// ended before anything was extracted: the archive to remove is kept
	let fail = |mut report: ExtractReport, phase, error| {
		if disposal_requested {
			let cancelled = phase == ExtractPhase::Cancelled;
			for disposition in kept_on_early_end(&[archive_uuid], cancelled) {
				report_disposition(&reporter, &mut report, disposition);
			}
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

	if let Err(error) = check_chunks_consistent(archive.chunks(), archive.size()) {
		return Err(fail(report, ExtractPhase::Failed, Arc::new(error)));
	}
	// Leased and floored before the codec starts, so a waiting job holds nothing.
	let Ok((_lease, floor)) = config.admit(&control, &reporter.ops()).await else {
		reporter.set_cancelling();
		return Err(fail(
			report,
			ExtractPhase::Cancelled,
			cancelled(ExtractReport::NAME),
		));
	};
	reporter.set_phase(ExtractPhase::Scanning);
	let link = match start() {
		Ok(link) => link,
		Err(error) => return Err(fail(report, ExtractPhase::Failed, Arc::new(error))),
	};

	let floor = (floor, reporter.op());
	let chunks = archive.size().div_ceil(CHUNK_SIZE_U64);
	let memory = backend.memory();
	let mut driver = Driver {
		backend,
		control,
		reporter,
		archive: Arc::new(archive),
		destination,
		root,
		max_bytes,
		max_items,
		config,
		link,
		floor: Some(floor),
		input_slot: Arc::new(Semaphore::new(1)),
		output_slot: Arc::new(Semaphore::new(1)),
		memory,
		targets: Arc::default(),
		chunks,
		next_fetch: 0,
		fetches: FuturesOrdered::new(),
		ready: VecDeque::new(),
		reading: None,
		served: 0,
		archive_hasher: blake3::Hasher::new(),
		sequential: true,
		layout: None,
		dispose,
		disposal_requested,
		ask: None,
		resolver: None,
		into_destination: false,
		unverified: false,
		dirs: Vec::new(),
		uncreated_dirs: 0,
		ready_dirs: VecDeque::new(),
		dir_creates: FuturesUnordered::new(),
		files: BTreeMap::new(),
		current: None,
		uploads: FuturesUnordered::new(),
		finalizes: FuturesUnordered::new(),
		held: None,
		events_closed: false,
		codec_result: None,
		top_level_beyond: Vec::new(),
		created_digest: 0,
		unchecked_entries: 0,
		committed: 0,
		items: 0,
		stamp: 0,
		stalled_ticks: 0,
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

fn worker_died() -> Error {
	Error::custom(
		ErrorKind::ArchiveWorkerDied,
		"the archive's codec stopped responding",
	)
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

/// Records `failure` in `report`; the event's copy of it, while the report keeps records.
fn record_failure(report: &mut ExtractReport, failure: ExtractFailure) -> Option<ExtractFailure> {
	keep(
		&mut report.failures,
		&mut report.omitted.failures,
		failure.clone(),
	)
	.then_some(failure)
}

/// Records `file` as failed at `stage`, in `report` and as an event.
fn report_file_failure<U>(
	report: &mut ExtractReport,
	reporter: &Reporter,
	file: &FileSlot<U>,
	stage: ExtractStage,
	error: Arc<Error>,
) {
	let failure = ExtractFailure {
		entry: file.entry,
		path: file.path.clone(),
		dest_parent: file.active.dest_parent,
		dest_name: file.name.as_ref().to_owned(),
		stage,
		error,
	};
	reporter.file_failed(
		Some(file.active.dest_uuid),
		file.bytes(),
		record_failure(report, failure),
	);
}

fn joined(segments: &[ValidatedName]) -> String {
	segments
		.iter()
		.map(AsRef::as_ref)
		.collect::<Vec<&str>>()
		.join("/")
}

impl<B: DisposalBackend> Driver<B> {
	/// `Err` with [`ErrorKind::Cancelled`] when cancelled, or the error that ended the job.
	async fn run(&mut self) -> Result<(), Arc<Error>> {
		let outcome = async {
			self.extract().await?;
			self.release_input();
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
			let dispositions = if result.is_ok() {
				let counts = self.reporter.counts();
				let complete =
					counts.files_failed + counts.dirs_failed + counts.entries_skipped == 0;
				// only an archive in the trash is not removed after a complete extraction
				let reason = if complete {
					KeptReason::Changed
				} else {
					KeptReason::Incomplete
				};
				vec![SourceDisposition {
					uuid: self.archive.uuid(),
					outcome: DisposalOutcome::Kept {
						reason,
						bytes_freed: 0,
					},
				}]
			} else {
				kept_on_early_end(&[self.archive.uuid()], phase == ExtractPhase::Cancelled)
			};
			for disposition in dispositions {
				report_disposition(&self.reporter, &mut self.report, disposition);
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

	/// A wrong password that only showed once entries were read (no entry was small enough to
	/// check it on first) leaves the directories created so far and no file: they go to the
	/// trash, so a retry with the right password starts clean, and out of the report's top-level
	/// items. Trashed, never deleted: they can be restored.
	async fn trash_created_dirs(&mut self) {
		let dirs = self
			.report
			.top_level
			.iter()
			.filter_map(|top| match &top.item {
				NonRootItemType::Dir(dir) => Some(dir.uuid()),
				NonRootItemType::File(_) => None,
			})
			.chain(
				self.top_level_beyond
					.iter()
					.filter(|(_, is_dir)| *is_dir)
					.map(|(uuid, _)| *uuid),
			)
			.collect::<Vec<_>>();
		let mut trashed = HashSet::new();
		for uuid in dirs {
			// the job created no file: one in there now is someone else's, and keeps the folder
			match self.backend.list_tree(uuid).await {
				Ok(tree) if tree.files.is_empty() => {}
				Ok(_) => continue,
				Err(error) => {
					tracing::warn!(
						"archive {}: failed to list a directory before trashing it: {error}",
						self.archive.uuid()
					);
					continue;
				}
			}
			match self.backend.trash_dir(uuid).await {
				Ok(()) => {
					trashed.insert(uuid);
				}
				Err(error) => tracing::warn!(
					"archive {}: failed to trash a directory created before the wrong password \
					 showed: {error}",
					self.archive.uuid()
				),
			}
		}
		self.report
			.top_level
			.retain(|top| !trashed.contains(&top.item.uuid()));
		self.top_level_beyond
			.retain(|(uuid, _)| !trashed.contains(uuid));
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
			let take_events = !stopping
				&& !pause_requested
				&& self.held.is_none()
				&& !self.backlogged()
				&& !self.events_closed;
			let await_result = self.events_closed && self.codec_result.is_none() && !stopping;

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
				Some(fetched) = self.fetches.next(), if !self.fetches.is_empty() => {
					self.fetch_finished(fetched);
				}
				event = self.link.events.recv(), if take_events => match event {
					Some(event) => self.on_event(event).await?,
					None => self.events_closed = true,
				},
				result = &mut self.link.done, if await_result => {
					self.codec_finished(result.unwrap_or_else(|_| Err(worker_died())));
				}
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
			&& self.codec_result.is_some()
	}

	/// Whether the codec waits for what it sent so far to be worked off first.
	fn backlogged(&self) -> bool {
		self.uncreated_dirs >= MAX_UNCREATED_DIRS || self.files.len() >= MAX_OPEN_FILES
	}

	fn idle(&self) -> bool {
		self.uploads.is_empty()
			&& self.fetches.is_empty()
			&& self.dir_creates.is_empty()
			&& self.finalizes.is_empty()
	}

	/// Gives back what only the codec needed, once it is done: chunks prefetched past its last
	/// read (a zip's or 7z's index chunks, fetched again for its entries) and the floor. What
	/// follows (the checks, the disposal) holds nothing while it waits out a pause.
	fn release_input(&mut self) {
		self.drop_prefetched();
		self.ask = None;
		self.reading = None;
		self.floor = None;
	}

	/// Waits out a pause holding nothing: prefetched chunks and the floor are given back.
	async fn pause(&mut self) -> Result<(), Stopped> {
		self.drop_prefetched();
		// The chunk the codec is reading stays with it, part of its state. Nothing else holds
		// the floor's input slot now, so it moves there if it took from the client's budget.
		if self.reading.is_some()
			&& let Ok(slot) = Arc::clone(&self.input_slot).try_acquire_owned()
		{
			self.reading = Some(slot);
		}
		// last, so the job counts as paused only now
		self.floor = None;
		self.reporter.checkpoint(&self.control).await?;
		let floor = self.control.until_stopping(self.config.floor()).await?;
		self.floor = Some((floor, self.reporter.op()));
		Ok(())
	}

	fn drop_prefetched(&mut self) {
		self.fetches = FuturesOrdered::new();
		self.ready.clear();
		// the codec reads in order, so fetching starts again at the chunk it needs next
		self.next_fetch = self.served;
	}

	/// Drops the transfers in flight; the files they belonged to are abandoned.
	fn drop_transfers(&mut self) {
		self.reporter.set_cancelling();
		self.fetches = FuturesOrdered::new();
		self.ready.clear();
		self.ask = None;
		self.held = None;
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
		self.serve_ask();
		while self.fetches.len() + self.ready.len() < PREFETCH_CHUNKS
			&& self.next_fetch < self.chunks
		{
			let Some(permit) = self.take_memory(&self.input_slot) else {
				break;
			};
			let index = self.next_fetch;
			self.next_fetch += 1;
			let backend = Arc::clone(&self.backend);
			let archive = Arc::clone(&self.archive);
			let op = self.reporter.op();
			self.fetches.push_back(Box::pin(async move {
				let result = backend.fetch_chunk(&archive, index).await;
				(index, result, permit, op)
			}) as MaybeSendBoxFuture<'static, _>);
		}
		while self.dir_creates.len() < MAX_SMALL_PARALLEL_REQUESTS
			&& let Some(dir) = self.ready_dirs.pop_front()
		{
			self.start_dir(dir);
		}
		if let Some(event) = self.held.take() {
			self.retry_held(event);
		}
		// files held back while a pause was requested: a pause lifted before the job went idle
		// has no resume of its own to start them
		self.finalize_ready();
	}

	/// Memory for one chunk: the floor's slot when free, else the client's budget if it has
	/// room now, else nothing.
	fn take_memory(&self, slot: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
		Arc::clone(slot).try_acquire_owned().ok().or_else(|| {
			Arc::clone(&self.memory)
				.try_acquire_many_owned(CHUNK_BYTES as u32)
				.ok()
		})
	}

	fn serve_ask(&mut self) {
		let Some((index, _)) = &self.ask else {
			return;
		};
		if self.ready.front().is_none_or(|(ready, ..)| ready != index) {
			return;
		}
		let (_, data, permit) = self.ready.pop_front().expect("just checked");
		let (_, reply) = self.ask.take().expect("just checked");
		self.reading = Some(permit);
		self.served += 1;
		self.archive_hasher.update_rayon(&data);
		let _ = reply.send(Ok(data));
		// progress follows the archive read, not only the idle ticks, which a busy job skips
		self.report_bytes_read();
	}

	/// Reports the bytes of the archive the codec has read, which its inputs count once each.
	fn report_bytes_read(&self) {
		let read = self.link.shared.input_bytes();
		debug_assert!(
			read <= self.archive.size(),
			"the codec read {read} bytes of a {}-byte archive",
			self.archive.size()
		);
		self.reporter.set_bytes_read(read);
	}

	fn fetch_finished(&mut self, (index, result, permit, _op): FetchedChunk) {
		let expected = chunk_plaintext_len(self.archive.size(), index);
		match result {
			Ok(data) if data.len() as u64 == expected => {
				self.ready.push_back((index, data, permit));
				self.serve_ask();
			}
			Ok(data) => self.stop_with(Error::custom(
				ErrorKind::Response,
				format!(
					"chunk {index} of the archive holds {} bytes instead of {expected}",
					data.len()
				),
			)),
			Err(error) => self.stop_with(error),
		}
	}

	fn tick(&mut self, pause_requested: bool) {
		self.report_bytes_read();
		self.reporter.tick();
		let stamp = self.link.shared.progress();
		// frozen while the driver owes the codec something: an answer, room for an event, or
		// the end of a pause
		let owed =
			self.ask.is_some() || self.held.is_some() || self.backlogged() || pause_requested;
		if owed || stamp != self.stamp || self.codec_result.is_some() {
			self.stamp = stamp;
			self.stalled_ticks = 0;
			return;
		}
		self.stalled_ticks += 1;
		if CALLBACK_INTERVAL * self.stalled_ticks >= ARCHIVE_STALL_TIMEOUT {
			tracing::error!(
				"archive {}: the codec made no progress for {ARCHIVE_STALL_TIMEOUT:?}",
				self.archive.uuid()
			);
			self.link.retire();
			self.codec_result = Some(Err(worker_died()));
			self.stop_with(worker_died());
		}
	}

	fn codec_finished(&mut self, result: CodecResult) {
		// the codec holds no chunk any more
		self.reading = None;
		match &result {
			Ok(end) => {
				self.report.unaccounted_bytes = end.unaccounted_bytes;
				self.report.duplicates = end.duplicates.clone();
				self.unchecked_entries = end.unchecked_entries;
			}
			// An error the driver caused (it failed a fetch, or stopped) is already the job's.
			Err(error) if self.fatal.error().is_some() || self.control.is_stopping() => {
				tracing::debug!("archive codec ended after the job did: {error}");
			}
			// The archive is damaged from here on, but what came before it is whole: the files
			// whose data is complete still finish, and only the one being read is dropped.
			Err(error) => {
				// a dead codec is a bug to hear of, where a damaged archive is only the user's
				if error.kind() == ErrorKind::ArchiveWorkerDied {
					tracing::error!("archive {}: {error}", self.archive.uuid());
				} else {
					tracing::warn!("archive {}: {error}", self.archive.uuid());
				}
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
		self.codec_result = Some(result);
	}

	async fn on_event(&mut self, event: WorkerEvent) -> Result<(), Stopped> {
		match event {
			WorkerEvent::Ask {
				source: _,
				index,
				reply,
			} => {
				// asking for another chunk means the codec is done with the one before
				self.reading = None;
				if index != self.served {
					// a jump (a zip is read from its end): what was fetched ahead is of no use
					self.sequential = false;
					self.fetches = FuturesOrdered::new();
					self.ready.clear();
					self.next_fetch = index;
					self.served = index;
				}
				self.ask = Some((index, reply));
				self.serve_ask();
			}
			WorkerEvent::Opened(layout) => {
				self.layout = Some(layout);
				self.open(layout).await?
			}
			WorkerEvent::Entry(head) => self.on_entry(head),
			WorkerEvent::Skipped(member) => self.on_skipped(member),
			event @ (WorkerEvent::Data(_) | WorkerEvent::FileEnd) => self.retry_held(event),
			// only a compressing codec sends a head
			WorkerEvent::Head(_) => debug_assert!(false, "an extracting codec sent a head"),
		}
		Ok(())
	}

	/// Sets up where entries go, once the codec has told what the archive holds.
	async fn open(&mut self, layout: ArchiveFormat) -> Result<(), Stopped> {
		let destination = self.destination.uuid();
		let listed = self
			.control
			.until_stopping(self.backend.list_dir_names(&self.destination))
			.await?;
		let targets = self
			.control
			.until_stopping(self.backend.connected_targets(destination))
			.await?;
		let (listed, targets) = match (listed, targets) {
			(Ok(listed), Ok(targets)) => (listed, targets),
			(Err(error), _) | (_, Err(error)) => {
				self.stop_with(error);
				return Err(Stopped);
			}
		};
		self.targets = Arc::new(targets);
		self.unverified = listed.unverified;
		let root_entry = self.entry_id(0);
		let new_folder = match (&self.root, layout) {
			(
				ExtractRoot::NewFolder { name },
				ArchiveFormat::Tar { .. } | ArchiveFormat::Zip | ArchiveFormat::SevenZ,
			) => Some(name.clone()),
			(_, ArchiveFormat::Single { .. }) | (ExtractRoot::Destination, _) => None,
		};
		let root_uuid = match new_folder {
			None => {
				self.into_destination = true;
				self.resolver = Some(PathResolver::new(listed.names.iter().map(String::as_str)));
				destination
			}
			Some(name) => {
				let wanted = match name {
					Some(name) => name,
					None => self.default_folder_name(),
				};
				let mut taken = TakenNames::new(listed.names.iter().map(String::as_str));
				let name = match taken.allocate(wanted, NameShape::Dir) {
					Ok(name) => name,
					Err(error) => {
						self.stop_with(error.into());
						return Err(Stopped);
					}
				};
				let folder = self.create_root(name).await?;
				self.resolver = Some(PathResolver::new(std::iter::empty()));
				folder
			}
		};
		self.dirs.push(DirSlot {
			uuid: root_uuid,
			parent: ROOT,
			name: ValidatedName::try_from("root").expect("a valid name"),
			archive_name: None,
			created: Utc::now(),
			entry: root_entry,
			state: DirState::Created(root_uuid),
			children: Vec::new(),
		});
		self.reporter.set_phase(ExtractPhase::Extracting);
		Ok(())
	}

	fn default_folder_name(&self) -> ValidatedName {
		extract_folder_name(self.archive.name())
	}

	/// Creates the folder entries are extracted into.
	async fn create_root(&mut self, name: ValidatedName) -> Result<Uuid, Stopped> {
		loop {
			// No entry is read yet, so only prefetched chunks are in flight: waited out holding
			// nothing, as between entries.
			if self.control.is_pause_requested() {
				self.pause().await?;
			}
			if self.control.is_stopping() {
				self.reporter.set_cancelling();
				return Err(Stopped);
			}
			let task = DirTask {
				backend: Arc::clone(&self.backend),
				control: self.control.clone(),
				ops: self.reporter.ops(),
				targets: Arc::clone(&self.targets),
				parent: self.destination.uuid(),
				uuid: Uuid::new_v4(),
				name: name.clone(),
				created: Utc::now(),
				color: DirColor::Default,
				top_level: true,
				verify_name: self.unverified,
			};
			match create_dir(task).await {
				Ok(outcome) => {
					let dir = outcome.dir;
					self.report_propagation(dir.uuid(), outcome.propagation_errors);
					self.reporter.dir_created(
						dir.uuid(),
						self.destination.uuid(),
						outcome.name.as_ref(),
					);
					self.created_digest = self.created_digest.wrapping_add(dir_digest(dir.uuid()));
					let uuid = dir.uuid();
					self.top_level_created(
						ExtractTopLevelKey::Root,
						NonRootItemType::Dir(Cow::Owned(dir)),
					);
					return Ok(uuid);
				}
				Err(DirError::NotStarted) => {}
				Err(DirError::Failed(error)) => {
					self.stop_with(error);
					return Err(Stopped);
				}
			}
		}
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
		let record = ExtractRenamedEntry {
			entry,
			path,
			name: name.as_ref().to_owned(),
			reason,
		};
		if keep(
			&mut self.report.renamed,
			&mut self.report.omitted.renamed,
			record.clone(),
		) {
			self.reporter.event(ExtractEvent::Renamed(record));
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
		let record = ExtractSkippedEntry {
			entry: self.entry_id(member.ordinal),
			path: member.path,
			path_truncated: member.path_truncated,
			bytes: member.bytes,
			reason: member.reason,
		};
		let kept = keep(
			&mut self.report.skipped,
			&mut self.report.omitted.skipped,
			record.clone(),
		);
		self.reporter.skipped(member.bytes, kept.then_some(record));
	}

	fn on_entry(&mut self, head: EntryHead) {
		let entry = self.entry_id(head.ordinal);
		let segments = &head.path.segments;
		let path = joined(segments);
		if head.path.rewritten
			&& let Some(name) = segments.last()
		{
			self.renamed(
				entry,
				path.clone(),
				name,
				ExtractRenameReason::PathRewritten,
			);
		}
		if head.path.suspicious {
			let record = ExtractMisleadingName {
				entry,
				path: path.clone(),
			};
			if keep(
				&mut self.report.misleading_names,
				&mut self.report.omitted.misleading_names,
				record.clone(),
			) {
				self.reporter.event(ExtractEvent::MisleadingName(record));
			}
		}
		match head.kind {
			EntryKind::Dir => {
				self.resolve_dirs(segments, entry, head.modified);
			}
			EntryKind::File { size } => {
				let Some((name, parents)) = segments.split_last() else {
					return;
				};
				let Some(parent) = self.resolve_dirs(parents, entry, None) else {
					return;
				};
				if !self.count_item() {
					return;
				}
				self.open_file(NewFile {
					ordinal: head.ordinal,
					entry,
					path,
					parent,
					name: name.clone(),
					size,
					modified: head.modified,
				});
			}
		}
	}

	/// The directory `segments` names, planning the ones not seen before; `None` once the job
	/// ended.
	fn resolve_dirs(
		&mut self,
		segments: &[ValidatedName],
		entry: ArchiveEntryId,
		modified: Option<DateTime<Utc>>,
	) -> Option<DirId> {
		let mut planned = Vec::new();
		let resolved = self
			.resolver
			.as_mut()
			.expect("entries follow the archive's layout")
			.resolve_dirs(segments, &mut planned);
		let dir = match resolved {
			Ok(dir) => dir,
			Err(error) => {
				self.stop_with(error.into());
				return None;
			}
		};
		for PlannedDir {
			id,
			parent,
			name,
			archive_name,
		} in planned
		{
			if !self.count_item() {
				return None;
			}
			// every planned directory is kept for the whole job, and one entry can imply 256:
			// they are capped like members, before they cost more
			if self.dirs.len() as u64 > self.config.max_members {
				self.stop_with(Error::custom(
					ErrorKind::ArchiveTooLarge,
					format!(
						"the archive names more than {} directories",
						self.config.max_members
					),
				));
				return None;
			}
			debug_assert_eq!(id, self.dirs.len());
			let state = match &self.dirs[parent].state {
				DirState::Failed(error) => DirState::Failed(Arc::clone(error)),
				DirState::Created(_) => {
					self.ready_dirs.push_back(id);
					DirState::Planned
				}
				DirState::Planned | DirState::Creating => DirState::Planned,
			};
			match state {
				DirState::Failed(_) => self.reporter.dir_failed(None),
				_ => self.uncreated_dirs += 1,
			}
			self.dirs[parent].children.push(id);
			self.dirs.push(DirSlot {
				uuid: Uuid::new_v4(),
				parent,
				name,
				archive_name,
				// Filen directories keep a creation time only; the archive's modification time
				// is the closest it has
				created: if id == dir {
					modified.unwrap_or_else(Utc::now)
				} else {
					Utc::now()
				},
				entry,
				state,
				children: Vec::new(),
			});
		}
		Some(dir)
	}

	fn start_dir(&mut self, dir: DirId) {
		let slot = &self.dirs[dir];
		let parent = self.dirs[slot.parent]
			.created_uuid()
			.expect("a directory is only created once its parent exists");
		let top_level = slot.parent == ROOT && self.into_destination;
		let task = DirTask {
			backend: Arc::clone(&self.backend),
			control: self.control.clone(),
			ops: self.reporter.ops(),
			targets: Arc::clone(&self.targets),
			parent,
			uuid: slot.uuid,
			name: slot.name.clone(),
			created: slot.created,
			color: DirColor::Default,
			top_level,
			verify_name: top_level && self.unverified,
		};
		self.dirs[dir].state = DirState::Creating;
		self.dir_creates
			.push(Box::pin(async move { (dir, create_dir(task).await) }));
	}

	fn dir_finished(&mut self, dir: DirId, result: Result<CreatedDirOutcome, DirError>) {
		let parent = self.dirs[self.dirs[dir].parent].uuid;
		match result {
			Ok(CreatedDirOutcome {
				dir: created,
				name,
				color_error: _,
				propagation_errors,
			}) => {
				self.report_propagation(created.uuid(), propagation_errors);
				// one record, with the name it got in the end (a keep-both name the resolver
				// picked, then possibly another the destination turned out to need)
				let slot = &self.dirs[dir];
				let entry = slot.entry;
				if name.as_ref() != slot.archive_name.as_ref().unwrap_or(&slot.name).as_ref() {
					let path = self.archive_path(dir);
					self.renamed(entry, path, &name, ExtractRenameReason::DuplicateName);
				}
				self.reporter
					.dir_created(created.uuid(), parent, name.as_ref());
				self.created_digest = self.created_digest.wrapping_add(dir_digest(created.uuid()));
				self.dirs[dir].state = DirState::Created(created.uuid());
				self.uncreated_dirs -= 1;
				self.ready_dirs
					.extend(self.dirs[dir].children.iter().copied());
				if self.dirs[dir].parent == ROOT && self.into_destination {
					self.top_level_created(
						ExtractTopLevelKey::Entry { id: entry },
						NonRootItemType::Dir(Cow::Owned(created)),
					);
				}
				self.finalize_ready();
			}
			// tried again once the pause is over
			Err(DirError::NotStarted) => {
				self.dirs[dir].state = DirState::Planned;
				self.ready_dirs.push_front(dir);
			}
			Err(DirError::Failed(error)) => {
				let error = Arc::new(error);
				self.note_error(&error);
				let failure = ExtractFailure {
					entry: self.dirs[dir].entry,
					path: self.archive_path(dir),
					dest_parent: parent,
					dest_name: self.dirs[dir].name.as_ref().to_owned(),
					stage: ExtractStage::CreateDirectory,
					error: Arc::clone(&error),
				};
				self.reporter
					.dir_failed(record_failure(&mut self.report, failure));
				self.fail_subtree(dir, &error);
			}
		}
	}

	/// Marks `root`'s planned subdirectories failed, and fails the files waiting in them.
	fn fail_subtree(&mut self, root: DirId, error: &Arc<Error>) {
		let mut stack = vec![root];
		while let Some(dir) = stack.pop() {
			if dir != root {
				self.reporter.dir_failed(None);
			}
			if matches!(self.dirs[dir].state, DirState::Planned | DirState::Creating) {
				self.uncreated_dirs -= 1;
			}
			self.dirs[dir].state = DirState::Failed(Arc::clone(error));
			stack.extend(self.dirs[dir].children.iter().copied());
		}
		self.ready_dirs
			.retain(|dir| !matches!(self.dirs[*dir].state, DirState::Failed(_)));
		let waiting: Vec<u64> = self
			.files
			.iter()
			.filter(|(_, file)| {
				!file.failed && matches!(self.dirs[file.parent].state, DirState::Failed(_))
			})
			.map(|(ordinal, _)| *ordinal)
			.collect();
		for ordinal in waiting {
			self.fail_file(ordinal, ExtractStage::CreateDirectory, Arc::clone(error));
		}
	}

	/// A directory's path in the archive, as drive names.
	fn archive_path(&self, mut dir: DirId) -> String {
		let mut names = Vec::new();
		while dir != ROOT {
			let slot = &self.dirs[dir];
			names.push(slot.archive_name.as_ref().unwrap_or(&slot.name).as_ref());
			dir = slot.parent;
		}
		names.reverse();
		names.join("/")
	}

	fn open_file(&mut self, new: NewFile) {
		let NewFile {
			ordinal,
			entry,
			path,
			parent,
			name,
			size,
			modified,
		} = new;
		let allocated = self
			.resolver
			.as_mut()
			.expect("entries follow the archive's layout")
			.file_name(parent, name.clone());
		// a keep-both name is reported once the file is registered, under the name it got then
		let name = match allocated {
			Ok(allocated) => allocated,
			Err(error) => {
				self.stop_with(error.into());
				return;
			}
		};
		let dest_uuid = Uuid::new_v4();
		let parent_uuid = self.dirs[parent].uuid;
		let upload = self.backend.begin_upload(UploadSpec {
			uuid: dest_uuid,
			parent: parent_uuid,
			// the upload owns its name; the file may be renamed again before it is registered
			name: name.clone(),
			mime: None,
		});
		let active = ExtractActiveFile {
			entry,
			dest_uuid,
			dest_parent: parent_uuid,
			name: name.as_ref().to_owned(),
			size,
			bytes_done: 0,
		};
		self.files.insert(
			ordinal,
			FileSlot {
				entry,
				path,
				parent,
				upload: Arc::new(upload),
				active: active.clone(),
				name,
				hasher: blake3::Hasher::new(),
				written: 0,
				next_index: 0,
				uploading: 0,
				info: None,
				modified,
				ended: false,
				finalizing: false,
				failed: false,
			},
		);
		self.current = Some(ordinal);
		if let DirState::Failed(error) = &self.dirs[parent].state {
			let error = Arc::clone(error);
			self.fail_file(ordinal, ExtractStage::CreateDirectory, error);
		} else {
			self.reporter.file_started(active);
		}
	}

	/// Takes up a data or file-end event, or holds it until it can be.
	fn retry_held(&mut self, event: WorkerEvent) {
		// a file's events after the job stopped taking it are dropped
		let Some(ordinal) = self.current else {
			return;
		};
		let Some(file) = self.files.get(&ordinal) else {
			return;
		};
		let data = match event {
			WorkerEvent::FileEnd => {
				self.files.get_mut(&ordinal).expect("looked up above").ended = true;
				self.current = None;
				self.finalize_ready();
				return;
			}
			WorkerEvent::Data(data) => data,
			// only data and file ends are ever held
			_ => return,
		};
		if file.failed {
			return;
		}
		let waits =
			self.dirs[file.parent].created_uuid().is_none() || file.uploading >= CHUNKS_PER_FILE;
		let permit = if waits {
			None
		} else {
			self.take_memory(&self.output_slot)
		};
		let Some(permit) = permit else {
			self.held = Some(WorkerEvent::Data(data));
			return;
		};
		let len = data.len() as u64;
		if let Some(error) = super::storage_exceeded(self.max_bytes, self.committed + len) {
			self.stop_with(error);
			return;
		}
		self.committed += len;
		let file = self.files.get_mut(&ordinal).expect("looked up above");
		file.hasher.update_rayon(&data);
		file.written += len;
		file.uploading += 1;
		let index = file.next_index;
		file.next_index += 1;
		let upload = Arc::clone(&file.upload);
		let backend = Arc::clone(&self.backend);
		let op = self.reporter.op();
		self.uploads.push(Box::pin(async move {
			let result = backend.upload_chunk(&upload, index, data).await;
			drop((permit, op));
			(ordinal, len, result)
		}) as MaybeSendBoxFuture<'static, _>);
	}

	fn upload_finished(&mut self, ordinal: u64, len: u64, result: Result<RemoteFileInfo, Error>) {
		let Some(file) = self.files.get_mut(&ordinal) else {
			return;
		};
		file.uploading -= 1;
		let (failed, dest_uuid) = (file.failed, file.active.dest_uuid);
		match result {
			Ok(info) => {
				file.info = Some(info);
				if !failed {
					self.reporter.chunk_uploaded(dest_uuid, len);
				}
			}
			Err(error) => {
				let error = Arc::new(error);
				self.note_error(&error);
				if !failed {
					self.fail_file(ordinal, ExtractStage::Upload, error);
				}
			}
		}
		self.finalize_ready();
	}

	/// Reports file `ordinal` as failed; its later data is dropped.
	fn fail_file(&mut self, ordinal: u64, stage: ExtractStage, error: Arc<Error>) {
		let file = self
			.files
			.get_mut(&ordinal)
			.expect("a failed file is known");
		file.failed = true;
		report_file_failure(&mut self.report, &self.reporter, file, stage, error);
	}

	/// Registers the files whose data is all up and whose directory exists, as many at once as
	/// other small requests; forgets failed files with nothing left in flight.
	fn finalize_ready(&mut self) {
		// a finalize started now would park on the pause holding the job busy, so the job
		// could never go idle and give back its memory: it starts on resume instead
		if self.control.is_pause_requested() {
			return;
		}
		let ready: Vec<u64> = self
			.files
			.iter()
			.filter(|(_, file)| {
				file.ended
					&& file.uploading == 0
					&& !file.finalizing
					&& (file.failed || self.dirs[file.parent].created_uuid().is_some())
			})
			.map(|(ordinal, _)| *ordinal)
			.collect();
		for ordinal in ready {
			if self.files[&ordinal].failed {
				self.files.remove(&ordinal);
			} else if self.finalizes.len() < MAX_SMALL_PARALLEL_REQUESTS {
				self.start_finalize(ordinal);
			}
		}
	}

	fn start_finalize(&mut self, ordinal: u64) {
		let top_level = {
			let file = &self.files[&ordinal];
			file.parent == ROOT && self.into_destination
		};
		let parent = self.dirs[self.files[&ordinal].parent]
			.created_uuid()
			.expect("checked by the caller");
		let file = self.files.get_mut(&ordinal).expect("checked by the caller");
		file.finalizing = true;
		let modified = file.modified.unwrap_or_else(Utc::now);
		let completion = UploadCompletion {
			written: file.written,
			num_chunks: file.next_index,
			hash: Blake3Hash::from(file.hasher.finalize()),
			final_times: (modified, modified),
		};
		let upload = Arc::clone(&file.upload);
		let info = file.info.clone().unwrap_or_default();
		let name = file.name.clone();
		let backend = Arc::clone(&self.backend);
		let control = self.control.clone();
		let ops = self.reporter.ops();
		let targets = Arc::clone(&self.targets);
		self.finalizes.push(Box::pin(async move {
			let mut retry = NameRetry::new(NameShape::File);
			let result = finalize_new_file_unless_paused(FinalizeTask {
				backend: &*backend,
				control: &control,
				ops: &ops,
				upload: &upload,
				parent,
				name,
				// a directory the job did not create may hold the name by now
				recheck: top_level.then_some(&mut retry),
				completion,
				info,
				targets: &targets,
			})
			.await;
			(ordinal, result)
		}) as MaybeSendBoxFuture<'static, _>);
	}

	fn finalize_finished(
		&mut self,
		ordinal: u64,
		result: Option<Result<Finalized, FinalizeError>>,
	) {
		let Some(result) = result else {
			// a pause came before the drive lock: started again on resume
			if let Some(file) = self.files.get_mut(&ordinal) {
				file.finalizing = false;
			}
			return;
		};
		let Some(file) = self.files.remove(&ordinal) else {
			return;
		};
		match result {
			Ok(Finalized {
				file: registered,
				name,
				propagation_errors,
			}) => {
				self.report_propagation(registered.uuid(), propagation_errors);
				if name.as_ref() != file.archive_name() {
					self.renamed(
						file.entry,
						file.path.clone(),
						&name,
						ExtractRenameReason::DuplicateName,
					);
				}
				let active = ExtractActiveFile {
					name: name.as_ref().to_owned(),
					..file.active
				};
				self.reporter.file_done(&active, file.written);
				self.created_digest = self
					.created_digest
					.wrapping_add(file_digest(active.dest_uuid, file.written));
				if file.parent == ROOT && self.into_destination {
					self.top_level_created(
						ExtractTopLevelKey::Entry { id: file.entry },
						NonRootItemType::File(Cow::Owned(registered)),
					);
				}
			}
			Err(FinalizeError::Stopped) => self
				.reporter
				.file_abandoned(file.active.dest_uuid, file.bytes()),
			Err(FinalizeError::RegisteredAsVersion {
				file: registered,
				propagation_errors,
			}) => {
				self.report_propagation(registered.uuid(), propagation_errors);
				let error = Error::custom(
					ErrorKind::InvalidState,
					"the entry was registered as a new version of an existing file",
				);
				let stage = ExtractStage::RegisteredAsVersion {
					existing_file: registered.stable_uuid.into(),
				};
				report_file_failure(&mut self.report, &self.reporter, &file, stage, error.into());
			}
			Err(FinalizeError::Failed(error)) => {
				let error = Arc::new(error);
				self.note_error(&error);
				report_file_failure(
					&mut self.report,
					&self.reporter,
					&file,
					ExtractStage::Finalize,
					error,
				);
			}
		}
	}

	/// Removes the archive if the extraction is verified; what became of it.
	async fn dispose_archive(&mut self, how: SourceDisposal, parent: Uuid) -> DisposalOutcome {
		let kept = |reason| DisposalOutcome::Kept {
			reason,
			bytes_freed: 0,
		};
		let counts = self.reporter.counts();
		if counts.files_failed + counts.dirs_failed + counts.entries_skipped > 0 {
			return kept(KeptReason::Incomplete);
		}
		if self.report.unaccounted_bytes > 0 {
			return kept(KeptReason::UnaccountedData {
				bytes: self.report.unaccounted_bytes,
			});
		}
		if self.report.duplicates.is_some() {
			return kept(KeptReason::Incomplete);
		}
		if self.unchecked_entries > 0 {
			return kept(KeptReason::Unconfirmed);
		}
		if !matches!(
			self.layout,
			Some(ArchiveFormat::Zip | ArchiveFormat::SevenZ)
		) {
			// A streaming archive's entries carry no checksum of their own (a tar's) or share
			// one for the whole stream: the whole archive, read front to back, has to match the
			// hash in its metadata. A zip's or a 7z's entries were each checked as they were
			// read.
			if !self.sequential || self.served != self.chunks {
				return kept(KeptReason::Unconfirmed);
			}
			let read = Blake3Hash::from(self.archive_hasher.finalize());
			match self.archive.hash() {
				Some(expected) if expected != read => return kept(KeptReason::HashMismatch),
				None if how == SourceDisposal::DeletePermanently => {
					return kept(KeptReason::HashUnavailable);
				}
				_ => {}
			}
		}
		if !self.output_confirmed(counts).await {
			return kept(if self.control.is_stopping() {
				KeptReason::Interrupted
			} else {
				KeptReason::Unconfirmed
			});
		}
		let archive = ExpectedFile::of(&*self.archive, self.archive.uuid(), parent);
		dispose_file(&*self.backend, archive, how, &self.control).await
	}

	/// Whether the server holds exactly what the counts say was created: every file at its size
	/// and every directory, listed again below the items created in the destination. Listed
	/// as many at once as other small requests; a pause is waited out between them, holding
	/// nothing, and a cancel drops those in flight and ends the check unconfirmed.
	async fn output_confirmed(&mut self, counts: ItemCounts) -> bool {
		let mut found = Tree::default();
		if !self.into_destination {
			// each request counts in flight, so a pause is only reported once it is over
			let _listing = self.reporter.op();
			let listed = self
				.control
				.until_stopping(self.backend.list_tree(self.dirs[ROOT].uuid))
				.await;
			match listed {
				Ok(Ok(tree)) => found = tree,
				Ok(Err(_)) | Err(Stopped) => return false,
			}
			// the folder itself
			found.dirs.insert(self.dirs[ROOT].uuid);
		}
		// the new folder is listed whole above; the items past the report's records are
		// checked as recheck_targets goes through them
		let items: Vec<(Uuid, bool)> = self
			.report
			.top_level
			.iter()
			.filter(|top| top.key != ExtractTopLevelKey::Root)
			.map(|top| (top.item.uuid(), matches!(top.item, NonRootItemType::Dir(_))))
			.chain(self.top_level_beyond.iter().copied())
			.collect();
		for batch in items.chunks(MAX_SMALL_PARALLEL_REQUESTS) {
			if self.reporter.checkpoint(&self.control).await.is_err() {
				return false;
			}
			let listing = self.reporter.op();
			// a listing removes nothing: a cancel drops the ones in flight
			let listed = self
				.control
				.until_stopping(join_all(
					batch
						.iter()
						.map(|&(uuid, is_dir)| created_tree(&*self.backend, uuid, is_dir)),
				))
				.await;
			drop(listing);
			let Ok(listed) = listed else {
				return false;
			};
			for tree in listed {
				let Some(tree) = tree else {
					return false;
				};
				found.dirs.extend(tree.dirs);
				found.files.extend(tree.files);
			}
			self.reporter.tick();
		}
		// the very items the job created, each file at the size it wrote
		found.files.len() as u64 == counts.files_done
			&& found.files.values().sum::<u64>() == counts.bytes_done
			&& found.dirs.len() as u64 == counts.dirs_created
			&& found.digest() == self.created_digest
	}

	/// The destination may have been shared or linked while the extraction ran; items created
	/// before that were propagated to the old targets only. Propagate everything created (each
	/// top-level item with its subtree) to the new ones, as many items at once as other small
	/// requests, under the drive lock, which a pause gives back until it is over.
	async fn recheck_targets(&mut self) -> Result<(), Stopped> {
		self.reporter.checkpoint(&self.control).await?;
		let destination = self.destination.uuid();
		let current = match self
			.control
			.until_stopping(self.backend.connected_targets(destination))
			.await?
		{
			Ok(current) => current,
			Err(error) => {
				// the extraction itself succeeded; only report
				tracing::warn!("failed to re-check the extraction destination's shares: {error}");
				return Ok(());
			}
		};
		let added = current.without(&self.targets);
		if added.is_empty() || self.report.top_level.is_empty() {
			return Ok(());
		}
		let items = self.report.top_level.len() + self.top_level_beyond.len();
		let mut next = 0;
		while next < items {
			let _lock = loop {
				match wait_for_lock(&*self.backend, &self.control, &self.reporter.ops()).await? {
					LockWait::Locked(held) => break held,
					LockWait::Paused => self.reporter.checkpoint(&self.control).await?,
					LockWait::Failed(error) => {
						tracing::warn!(
							"failed to lock the drive to propagate extracted items: {error}"
						);
						return Ok(());
					}
				}
			};
			while next < items && !self.control.is_pause_requested() {
				// a cancel is not kept waiting for every item
				if self.control.is_stopping() {
					return Err(Stopped);
				}
				let end = (next + MAX_SMALL_PARALLEL_REQUESTS).min(items);
				let propagated = join_all((next..end).map(|index| {
					propagate_top_level(
						&*self.backend,
						&self.report.top_level,
						&self.top_level_beyond,
						index,
						&added,
					)
				}))
				.await;
				for (dest_uuid, errors) in propagated {
					for error in errors {
						self.reporter.event(ExtractEvent::PropagationFailed {
							dest_uuid,
							error: Arc::new(error),
						});
					}
				}
				next = end;
				self.reporter.tick();
			}
		}
		Ok(())
	}
}

/// Propagates top-level item `index` of `kept` followed by `beyond` (those past the report's
/// records, fetched again: only when the destination changed, which is rare, rather than
/// holding every one of them for the whole job) with its subtree to `added`; its uuid, and
/// what failed.
async fn propagate_top_level<B: DisposalBackend>(
	backend: &B,
	kept: &[ExtractedTopLevel],
	beyond: &[(Uuid, bool)],
	index: usize,
	added: &ConnectedTargets,
) -> (Uuid, Vec<Error>) {
	if let Some(top) = kept.get(index) {
		return (
			top.item.uuid(),
			backend.propagate_tree(added, &top.item).await,
		);
	}
	let (uuid, is_dir) = beyond[index - kept.len()];
	let errors = match backend.normal_item(uuid, is_dir).await {
		Ok(item) => backend.propagate_tree(added, &item).await,
		Err(error) => vec![error],
	};
	(uuid, errors)
}

/// What the server holds of a top-level item the job created: itself, and everything below a
/// directory; `None` if it could not be listed, or is a file in the trash.
async fn created_tree<B: DisposalBackend>(backend: &B, uuid: Uuid, is_dir: bool) -> Option<Tree> {
	let mut tree = Tree::default();
	if is_dir {
		tree = backend.list_tree(uuid).await.ok()?;
		tree.dirs.insert(uuid);
	} else {
		let state = backend.file_state(uuid).await.ok()?;
		if state.trash {
			return None;
		}
		tree.files.insert(uuid, state.size);
	}
	Some(tree)
}

#[cfg(test)]
mod tests;
