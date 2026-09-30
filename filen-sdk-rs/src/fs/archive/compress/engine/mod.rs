//! Runs a compression: the async driver of the codec worker. It fetches each source file for
//! the codec in the order the codec writes them, uploads the archive the codec produces as one
//! new file, and registers it in the destination once all of it is up. Nothing becomes visible
//! before that, so a job that stops early leaves nothing behind.
//!
//! Memory, pause and cancel work as in extraction (see the extract engine): a two-chunk floor
//! for one input and one output chunk, more only from the client's budget when it is free right
//! now, and a pause that gives back the floor, the prefetched chunks and every reservation from
//! the client's budget once in-flight work is done. A paused job keeps its codec's state and the
//! source chunk the codec is reading resident. Once the codec is done the floor is given back;
//! reading the archive back before a permanent disposal takes it again, and pauses the same way
//! (see [`read_back`](super::read_back)).

use std::{
	collections::{BTreeSet, HashMap, VecDeque},
	sync::Arc,
};

use chrono::Utc;
use filen_types::{crypto::Blake3Hash, fs::Uuid};
use futures::{
	StreamExt,
	stream::{FuturesOrdered, FuturesUnordered},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::{
	Error, ErrorKind,
	connect::ConnectedTargets,
	consts::{CALLBACK_INTERVAL, CHUNK_SIZE_U64},
	fs::{
		HasUUID,
		archive::{
			config::ArchiveConfig,
			dispose::{
				DisposalBackend, DisposalOutcome, ExpectedDir, ExpectedFile, KeptReason, Nesting,
				Removing, SourceDisposal, SourceDisposition, WrittenFile, dispose_dir,
				dispose_file, kept_on_early_end, nesting,
			},
			hash::HeadLastHasher,
			input::{PREFETCH_CHUNKS, take_memory, whole_chunk},
			limits::MAX_REPORT_RECORDS,
			worker::{CodecStart, StallWatch, WorkerEvent, WorkerLink, codec_failed, worker_died},
		},
		categories::{DirType, Normal},
		drive_job::{
			self, Fatal,
			backend::{DriveBackend, UploadSpec},
			finalize::{FinalizeError, FinalizeTask, Finalized, finalize_new_file},
			name_retry::NameRetry,
		},
		file::{
			RemoteFile,
			enums::RemoteFileType,
			read::check_chunks_consistent,
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

use super::read_back::{ReadBack, reads_back};
use super::report::{
	CompressActiveFile, CompressEvent, CompressFailed, CompressPhase, CompressReport, HashMismatch,
	Reporter,
};

/// Archive chunks uploading at once.
const UPLOADS_AT_ONCE: usize = 4;

/// What the codec returns: the archive's length.
pub(crate) type CodecResult = Result<u64, Error>;

/// A source file, by its place in the codec's entries.
#[derive(Debug)]
pub(crate) struct Source {
	pub(crate) file: RemoteFileType<'static>,
	/// Its path in the archive, for reporting.
	pub(crate) path: String,
	/// The top-level source (the job's request) it was listed under.
	pub(crate) request: usize,
}

/// What [`run_compress`] needs.
pub(crate) struct CompressTask<B> {
	pub(crate) backend: Arc<B>,
	pub(crate) control: JobControl,
	pub(crate) reporter: MaybeArc<Reporter>,
	pub(crate) destination: DirType<'static, Normal>,
	/// The archive's name, whose last `extension_len` bytes are its format's extension.
	pub(crate) name: ValidatedName,
	pub(crate) extension_len: usize,
	/// Every file the codec reads, by source number.
	pub(crate) sources: Vec<Source>,
	pub(crate) max_bytes: Option<u64>,
	pub(crate) config: ArchiveConfig,
	/// The codec sends the archive's first chunk last ([`WorkerEvent::Head`]).
	pub(crate) head_last: bool,
	/// Starts the codec; called once the job holds its lease and memory floor.
	pub(crate) start: CodecStart<CodecResult>,
	/// The report so far: the plan's totals, skips and renames.
	pub(crate) report: CompressReport,
	pub(crate) disposal: Option<CompressDisposal>,
	/// Reads the archive back before a permanent disposal.
	pub(crate) read_back: Option<ReadBack>,
}

/// How to remove the sources once the archive is verified, and what the job read of them.
#[derive(Debug)]
pub(crate) struct CompressDisposal {
	pub(crate) how: SourceDisposal,
	pub(crate) targets: Vec<DisposalTarget>,
	/// Per target: every file read below it had a hash in its metadata to check it against.
	pub(crate) hashed: Vec<bool>,
}

#[derive(Debug)]
pub(crate) enum DisposalTarget {
	File(ExpectedFile),
	Dir(ExpectedDir),
	/// A source whose state the job cannot compare against (it is in the trash).
	Unavailable {
		uuid: Uuid,
	},
}

/// A chunk of a source: its source number and index.
type ChunkKey = (u32, u64);

/// A fetched chunk of a source, with the memory it holds.
type FetchedChunk = (
	(u32, u64),
	Result<Vec<u8>, Error>,
	OwnedSemaphorePermit,
	OpGuard,
);

struct SourceState {
	file: Arc<RemoteFileType<'static>>,
	path: String,
	request: usize,
	chunks: u64,
	/// Chunks handed to the codec, which asks for them in order.
	served: u64,
	hasher: blake3::Hasher,
}

impl SourceState {
	fn new(source: Source) -> Self {
		Self {
			chunks: source.file.size().div_ceil(CHUNK_SIZE_U64),
			file: Arc::new(source.file),
			path: source.path,
			request: source.request,
			served: 0,
			hasher: blake3::Hasher::new(),
		}
	}
}

struct Driver<B: DriveBackend> {
	backend: Arc<B>,
	control: JobControl,
	reporter: MaybeArc<Reporter>,
	config: ArchiveConfig,
	link: WorkerLink<CodecResult>,
	floor: Option<OwnedSemaphorePermit>,
	input_slot: Arc<Semaphore>,
	output_slot: Arc<Semaphore>,
	memory: Arc<Semaphore>,

	sources: Vec<SourceState>,
	/// The chunks the codec will read, in order: every chunk of every source with data.
	next_fetch: (usize, u64),
	/// The chunk the codec reads next.
	next_served: (usize, u64),
	fetches: FuturesOrdered<MaybeSendBoxFuture<'static, FetchedChunk>>,
	/// Fetched chunks the codec has not read yet; each counts as in flight, so the job does not
	/// count as paused while one still holds memory.
	ready: VecDeque<(ChunkKey, Vec<u8>, OwnedSemaphorePermit, OpGuard)>,
	reading: Option<OwnedSemaphorePermit>,
	ask: Option<(ChunkKey, oneshot::Sender<Vec<u8>>)>,

	destination: Uuid,
	upload: Arc<B::Upload>,
	archive_uuid: Uuid,
	hasher: blake3::Hasher,
	/// Hashes the archive when its first chunk comes last; `None` once it has.
	head_last: Option<HeadLastHasher>,
	/// The archive's hash, once the first chunk came last.
	head_last_hash: Option<blake3::Hash>,
	written: u64,
	next_index: u64,
	info: Option<RemoteFileInfo>,
	uploads: FuturesUnordered<MaybeSendBoxFuture<'static, (u64, Result<RemoteFileInfo, Error>)>>,
	/// An archive chunk waiting for memory or for an upload slot, and whether it is the
	/// first chunk sent last; the codec parks meanwhile.
	held: Option<(Vec<u8>, bool)>,
	events_closed: bool,
	/// The codec returned, or was given up on.
	codec_done: bool,
	max_bytes: Option<u64>,
	stall: StallWatch,
	/// The top-level sources a file of which did not match the hash in its metadata.
	mismatched: BTreeSet<usize>,
	/// The files behind `mismatched`, for the report.
	hash_mismatches: Vec<HashMismatch>,
	read_back: Option<ReadBack>,
	fatal: Fatal,
}

/// Runs a compression: waits for a job slot, starts the codec, uploads the archive and
/// registers it.
pub(crate) async fn run_compress<B: DisposalBackend>(
	task: CompressTask<B>,
) -> Result<CompressReport, CompressFailed> {
	let CompressTask {
		backend,
		control,
		reporter,
		destination,
		name,
		extension_len,
		sources,
		max_bytes,
		config,
		head_last,
		start,
		mut report,
		disposal,
		read_back,
	} = task;
	let shape = NameShape::FileWithExtension { len: extension_len };
	// every source a disposal was asked for is reported, however the job ends
	let requested: Vec<Uuid> = disposal
		.as_ref()
		.map(|disposal| disposal.targets.iter().map(DisposalTarget::uuid).collect())
		.unwrap_or_default();
	let fail = |report, phase, error| end_early(&reporter, report, &requested, phase, error);
	for source in &sources {
		if let Err(error) = check_chunks_consistent(source.file.chunks(), source.file.size()) {
			return Err(fail(report, CompressPhase::Failed, Arc::new(error)));
		}
	}

	// Leased and floored before anything is read, so a waiting job holds nothing and does
	// nothing. A pause asked for before then is recorded first: the job reads paused while it
	// waits it out, never running.
	reporter.set_pause_requested(control.is_pause_requested());
	reporter.set_phase(CompressPhase::WaitingForWorker);
	let Ok((_lease, floor)) = config.admit(&control, &reporter.ops()).await else {
		reporter.wind_down(&control);
		return Err(fail(report, CompressPhase::Cancelled, cancelled()));
	};
	reporter.set_phase(CompressPhase::Compressing);

	let prepared = named_in(&*backend, &destination, name, shape);
	let (name, targets) = match control.until_stopping(prepared).await {
		Ok(Ok(prepared)) => prepared,
		Ok(Err(error)) => return Err(fail(report, CompressPhase::Failed, Arc::new(error))),
		Err(Stopped) => return Err(fail(report, CompressPhase::Cancelled, cancelled())),
	};
	let link = match start() {
		Ok(link) => link,
		Err(error) => return Err(fail(report, CompressPhase::Failed, Arc::new(error))),
	};

	let archive_uuid = Uuid::new_v4();
	let upload = backend.begin_upload(UploadSpec {
		uuid: archive_uuid,
		parent: destination.uuid(),
		name: name.clone(),
		mime: None,
	});
	let memory = backend.memory();
	let sources = sources.into_iter().map(SourceState::new).collect();
	let mut driver = Driver {
		backend,
		control,
		reporter,
		config,
		link,
		floor: Some(floor),
		input_slot: Arc::new(Semaphore::new(1)),
		output_slot: Arc::new(Semaphore::new(1)),
		memory,
		sources,
		next_fetch: (0, 0),
		next_served: (0, 0),
		fetches: FuturesOrdered::new(),
		ready: VecDeque::new(),
		reading: None,
		ask: None,
		destination: destination.uuid(),
		upload: Arc::new(upload),
		archive_uuid,
		hasher: blake3::Hasher::new(),
		head_last: head_last.then(HeadLastHasher::new),
		head_last_hash: None,
		written: 0,
		// the first chunk's index is kept for when it comes
		next_index: u64::from(head_last),
		info: None,
		uploads: FuturesUnordered::new(),
		held: None,
		events_closed: false,
		codec_done: false,
		max_bytes,
		stall: StallWatch::default(),
		mismatched: BTreeSet::new(),
		hash_mismatches: Vec::new(),
		read_back,
		fatal: Fatal::default(),
	};
	let incomplete = !report.skipped.is_empty();
	driver.next_fetch = driver.first_chunk_from(0);
	driver.next_served = driver.next_fetch;
	let outcome = async {
		driver.compress().await?;
		if driver.fatal.error().is_some() {
			return Err(Stopped);
		}
		driver
			.finish(name, shape, targets, disposal, &requested, incomplete)
			.await
	}
	.await;
	let Driver {
		reporter,
		fatal,
		control,
		hash_mismatches,
		..
	} = driver;
	report.hash_mismatches = hash_mismatches;
	match fatal.end(outcome, &control, CompressReport::NAME) {
		(_, Ok((archive, dispositions))) => {
			report.archive = Some(archive);
			report.dispositions = dispositions;
			// a cancel once the archive exists keeps the sources, but the job is done; it still
			// ends as cancelled jobs do, however late it was seen
			if control.is_cancelled() {
				reporter.set_cancelling();
			}
			reporter.finish(CompressPhase::Done);
			report.counts = reporter.counts();
			Ok(report)
		}
		(phase, Err(error)) => Err(end_early(&reporter, report, &requested, phase, error)),
	}
}

/// The archive's name, picked against what `destination` holds up front (it is checked again when
/// the archive is registered), and the shares and links the archive is propagated to.
async fn named_in<B: DriveBackend>(
	backend: &B,
	destination: &DirType<'static, Normal>,
	name: ValidatedName,
	shape: NameShape,
) -> Result<(ValidatedName, ConnectedTargets), Error> {
	let listed = backend.list_dir_names(destination).await?;
	let targets = backend.connected_targets(destination.uuid()).await?;
	let name = TakenNames::new(listed.names.iter().map(String::as_str)).allocate(name, shape)?;
	Ok((name, targets))
}

/// Ends a job early, before it removed any source: every source a disposal was asked for is
/// kept, and told of.
pub(crate) fn end_early(
	reporter: &Reporter,
	mut report: CompressReport,
	requested: &[Uuid],
	phase: CompressPhase,
	error: Arc<Error>,
) -> CompressFailed {
	let cancelled = phase == CompressPhase::Cancelled;
	// before the dispositions go out, so their update reads as cancelling
	if cancelled {
		reporter.set_cancelling();
	}
	report.dispositions = kept_on_early_end(requested, cancelled);
	reporter.dispositions(&report.dispositions);
	reporter.finish_with_totals(phase, report.totals);
	report.counts = reporter.counts();
	CompressFailed { report, error }
}

pub(crate) fn cancelled() -> Arc<Error> {
	drive_job::cancelled(CompressReport::NAME)
}

impl<B: DisposalBackend> Driver<B> {
	/// The first chunk of the first source with data at or after `source`, or the end.
	fn first_chunk_from(&self, mut source: usize) -> (usize, u64) {
		while source < self.sources.len() && self.sources[source].chunks == 0 {
			source += 1;
		}
		(source, 0)
	}

	/// The chunk after `chunk`, in the order the codec reads them.
	fn after(&self, (source, index): (usize, u64)) -> (usize, u64) {
		if index + 1 < self.sources[source].chunks {
			(source, index + 1)
		} else {
			self.first_chunk_from(source + 1)
		}
	}

	fn stop_with(&mut self, error: Error) {
		self.fatal
			.stop(Arc::new(error), &self.control, &*self.reporter);
	}

	/// Runs the codec to its end with every archive chunk uploaded; `Err` when stopped.
	async fn compress(&mut self) -> Result<(), Stopped> {
		loop {
			let pause_requested = self.control.is_pause_requested();
			self.reporter.set_pause_requested(pause_requested);
			let stopping = self.control.is_stopping();
			if stopping {
				return Err(Stopped);
			}
			if !pause_requested {
				self.advance();
			}
			if self.codec_done && self.uploads.is_empty() && self.held.is_none() {
				return Ok(());
			}
			if pause_requested && self.uploads.is_empty() && self.fetches.is_empty() {
				self.pause().await?;
				continue;
			}
			let take_events = !pause_requested && self.held.is_none() && !self.events_closed;
			let await_result = self.events_closed && !self.codec_done;
			tokio::select! {
				biased;
				() = self.control.stopping() => {},
				() = self.control.pause_changed(pause_requested) => {},
				Some((len, result)) = self.uploads.next(), if !self.uploads.is_empty() => {
					match result {
						Ok(info) => {
							self.info = Some(info);
							self.reporter.archive_written(len);
						}
						Err(error) => self.stop_with(error),
					}
				}
				Some(fetched) = self.fetches.next(), if !self.fetches.is_empty() => {
					self.fetch_finished(fetched);
				}
				event = self.link.events.recv(), if take_events => match event {
					Some(event) => self.on_event(event),
					None => self.events_closed = true,
				},
				result = &mut self.link.done, if await_result => {
					self.codec_finished(result.unwrap_or_else(|_| Err(worker_died())));
				}
				() = sleep(CALLBACK_INTERVAL) => self.tick(pause_requested),
			}
		}
	}

	/// Waits out a pause, giving back the floor and every prefetched chunk. The chunk the codec
	/// is reading stays resident until it asks again, held against the job's own input slot
	/// instead of the client's memory budget.
	async fn pause(&mut self) -> Result<(), Stopped> {
		// the job counts as paused only once everything below is given back
		let releasing = self.reporter.op();
		self.fetches = FuturesOrdered::new();
		self.ready.clear();
		self.next_fetch = self.next_served;
		self.floor = None;
		if self.reading.take().is_some() {
			// nothing else holds the slot once the prefetched chunks are dropped
			self.reading = Arc::clone(&self.input_slot).try_acquire_owned().ok();
			debug_assert!(
				self.reading.is_some(),
				"the input slot is free while pausing"
			);
		}
		drop(releasing);
		self.reporter.checkpoint(&self.control).await?;
		let floor = self.control.until_stopping(self.config.floor()).await?;
		self.floor = Some(floor);
		Ok(())
	}

	fn advance(&mut self) {
		self.serve_ask();
		while self.fetches.len() + self.ready.len() < PREFETCH_CHUNKS
			&& self.next_fetch.0 < self.sources.len()
		{
			let Some(permit) = take_memory(&self.input_slot, &self.memory) else {
				break;
			};
			let (source, index) = self.next_fetch;
			self.next_fetch = self.after(self.next_fetch);
			let backend = Arc::clone(&self.backend);
			let file = Arc::clone(&self.sources[source].file);
			let op = self.reporter.op();
			self.fetches.push_back(Box::pin(async move {
				let result = backend.fetch_chunk(&file, index).await;
				let source = u32::try_from(source).expect(
					"the client numbers sources in u32 and refuses more (should be impossible)",
				);
				((source, index), result, permit, op)
			}) as MaybeSendBoxFuture<'static, _>);
		}
		if let Some((data, head)) = self.held.take() {
			self.take_data(data, head);
		}
	}

	fn fetch_finished(&mut self, ((source, index), result, permit, op): FetchedChunk) {
		let file = &self.sources[source as usize].file;
		match result.and_then(|data| whole_chunk(file, index, data)) {
			Ok(data) => {
				self.ready.push_back(((source, index), data, permit, op));
				self.serve_ask();
			}
			Err(error) => self.stop_with(error),
		}
	}

	fn serve_ask(&mut self) {
		let Some((key, _)) = &self.ask else {
			return;
		};
		if self.ready.front().is_none_or(|(ready, ..)| ready != key) {
			return;
		}
		let ((source, index), data, permit, _op) = self.ready.pop_front().expect("just checked");
		let (_, reply) = self.ask.take().expect("just checked");
		self.reading = Some(permit);
		self.next_served = self.after((source as usize, index));
		let len = data.len() as u64;
		let state = &mut self.sources[source as usize];
		state.hasher.update_rayon(&data);
		state.served += 1;
		if state.served == state.chunks
			&& let Some(expected) = state.file.hash()
			&& Blake3Hash::from(state.hasher.finalize()) != expected
		{
			tracing::warn!(
				"source {} of an archive does not match the hash in its metadata",
				state.file.uuid()
			);
			self.mismatched.insert(state.request);
			let mismatch = HashMismatch {
				source_uuid: state.file.uuid(),
				path: state.path.clone(),
			};
			if self.hash_mismatches.len() < MAX_REPORT_RECORDS {
				self.hash_mismatches.push(mismatch.clone());
			}
			self.reporter
				.event(CompressEvent::SourceHashMismatch(mismatch));
		}
		let (source_uuid, size) = (state.file.uuid(), state.file.size());
		self.reporter
			.source_read(source_uuid, len, || CompressActiveFile {
				source_uuid,
				name: state
					.path
					.rsplit_once('/')
					.map_or(&*state.path, |(_, name)| name)
					.to_owned(),
				path: state.path.clone(),
				size,
				bytes_done: 0,
			});
		let _ = reply.send(data);
	}

	fn on_event(&mut self, event: WorkerEvent) {
		match event {
			WorkerEvent::Ask {
				source,
				index,
				reply,
			} => {
				self.reading = None;
				debug_assert_eq!(
					(source as usize, index),
					self.next_served,
					"the codec reads the sources in order"
				);
				self.ask = Some(((source, index), reply));
				self.serve_ask();
			}
			WorkerEvent::Data(data) => self.take_data(data, false),
			WorkerEvent::Head(data) => self.take_data(data, true),
			WorkerEvent::FileEnd => self.reporter.file_done(),
			// the compressing codec sends nothing else
			WorkerEvent::Opened(_)
			| WorkerEvent::Entry(_)
			| WorkerEvent::Skipped(_)
			| WorkerEvent::Link(_)
			| WorkerEvent::Listed(_) => {}
		}
	}

	/// Uploads an archive chunk (the first one when `head`), or holds it until memory or an
	/// upload slot is free.
	fn take_data(&mut self, data: Vec<u8>, head: bool) {
		let permit = if self.uploads.len() < UPLOADS_AT_ONCE {
			take_memory(&self.output_slot, &self.memory)
		} else {
			None
		};
		let Some(permit) = permit else {
			self.held = Some((data, head));
			return;
		};
		let len = data.len() as u64;
		if let Some(max) = self.max_bytes
			&& drive_job::exceeds_limit(self.written + len, max)
		{
			self.stop_with(Error::custom(
				ErrorKind::MaxStorageReached,
				format!("the archive needs more than the {max} bytes that are free"),
			));
			return;
		}
		let index = match (&mut self.head_last, head) {
			(None, false) => {
				self.hasher.update_rayon(&data);
				self.next_index += 1;
				self.next_index - 1
			}
			(Some(hasher), false) => {
				hasher.update(&data);
				self.next_index += 1;
				self.next_index - 1
			}
			(Some(_), true) => {
				let hasher = self.head_last.take().expect("matched above");
				self.head_last_hash = Some(hasher.finalize(&data));
				0
			}
			(None, true) => {
				self.stop_with(Error::custom(
					ErrorKind::Internal,
					"the codec sent the archive's first chunk twice",
				));
				return;
			}
		};
		self.written += len;
		let backend = Arc::clone(&self.backend);
		let upload = Arc::clone(&self.upload);
		let op = self.reporter.op();
		self.uploads.push(Box::pin(async move {
			let result = backend.upload_chunk(&upload, index, data).await;
			drop((permit, op));
			(len, result)
		}) as MaybeSendBoxFuture<'static, _>);
	}

	fn tick(&mut self, pause_requested: bool) {
		self.reporter.tick();
		let owed = self.ask.is_some() || self.held.is_some() || pause_requested || self.codec_done;
		if self
			.stall
			.give_up_if_stalled(&self.link, owed, self.archive_uuid)
		{
			self.codec_done = true;
			self.stop_with(worker_died());
		}
	}

	fn codec_finished(&mut self, result: CodecResult) {
		// the codec reads nothing more
		self.reading = None;
		self.codec_done = true;
		match result {
			Ok(len) if len != self.written => self.stop_with(Error::custom(
				ErrorKind::Internal,
				format!(
					"the codec wrote {len} archive bytes, {} were handed over",
					self.written
				),
			)),
			Ok(_) => {}
			Err(error) => {
				let ended = self.fatal.error().is_some() || self.control.is_stopping();
				if codec_failed(self.archive_uuid, &error, ended) {
					self.stop_with(error);
				}
			}
		}
	}

	/// Registers the archive, once the codec is done, and removes the sources if asked to: the
	/// archive, and what became of each source.
	async fn finish(
		&mut self,
		name: ValidatedName,
		shape: NameShape,
		targets: ConnectedTargets,
		disposal: Option<CompressDisposal>,
		requested: &[Uuid],
		incomplete: bool,
	) -> Result<(RemoteFile, Vec<SourceDisposition>), Stopped> {
		// the codec is done: registering and removing the sources hold no memory floor, and
		// reading the archive back takes its own
		self.floor = None;
		self.reporter.set_phase(CompressPhase::Finishing);
		// the destination may have been shared or linked since the job started
		let refetched = self
			.control
			.until_stopping(self.backend.connected_targets(self.destination))
			.await?;
		let targets = match refetched {
			Ok(current) => current,
			Err(error) => {
				tracing::warn!("failed to re-check the archive destination's shares: {error}");
				targets
			}
		};
		let archive = self.register(name, shape, targets).await?;
		// the archive exists from here on: a cancel now keeps the sources, but the job is done
		let Some(disposal) = disposal else {
			return Ok((archive, Vec::new()));
		};
		// a permanent disposal reads the archive back first
		self.reporter
			.set_phase(if disposal.how == SourceDisposal::DeletePermanently {
				CompressPhase::Verifying
			} else {
				CompressPhase::DisposingSources
			});
		let dispositions = match self.reporter.checkpoint(&self.control).await {
			Ok(()) => self.dispose(disposal, &archive, incomplete).await,
			Err(Stopped) => {
				let kept = kept_on_early_end(requested, true);
				self.reporter.dispositions(&kept);
				kept
			}
		};
		Ok((archive, dispositions))
	}

	/// Removes the sources if the archive is verified; what became of each.
	async fn dispose(
		&mut self,
		disposal: CompressDisposal,
		archive: &RemoteFile,
		incomplete: bool,
	) -> Vec<SourceDisposition> {
		let CompressDisposal {
			how,
			targets,
			hashed,
		} = disposal;
		let read_back = self.read_back.take();
		// a source whose own files could not be checked is kept on its own; anything wrong
		// with the archive keeps them all
		let mismatched = std::mem::take(&mut self.mismatched);
		let own_reason = |request: usize| {
			if mismatched.contains(&request) {
				Some(KeptReason::HashMismatch)
			} else if how == SourceDisposal::DeletePermanently && !hashed[request] {
				Some(KeptReason::HashUnavailable)
			} else {
				None
			}
		};
		let every_source_kept = (0..targets.len()).all(|request| own_reason(request).is_some());
		let archive_reason = self
			.archive_kept(how, archive, read_back, incomplete, every_source_kept)
			.await;
		self.reporter.set_phase(CompressPhase::DisposingSources);
		let Nesting { outermost, cyclic } = nesting(&enclosing(&targets));
		let uuids: Vec<Uuid> = targets.iter().map(DisposalTarget::uuid).collect();
		let files: Vec<bool> = targets
			.iter()
			.map(|target| matches!(target, DisposalTarget::File(_)))
			.collect();
		let mut dispositions: Vec<Option<SourceDisposition>> = vec![None; targets.len()];
		// the sources each outermost one goes with, itself included
		let mut going = vec![Vec::new(); targets.len()];
		for (source, &outer) in outermost.iter().enumerate() {
			going[outer].push(source);
		}
		for (request, target) in targets.into_iter().enumerate() {
			if outermost[request] != request {
				continue;
			}
			let held_back = if cyclic[request] {
				Some(KeptReason::Changed)
			} else {
				archive_reason.clone().or_else(|| own_reason(request))
			};
			// the files the folder's permanent removal deleted, even when it stopped part way
			let mut deleted = BTreeSet::new();
			let outcome = match held_back {
				Some(reason) => DisposalOutcome::kept(reason),
				None => {
					let ops = self.reporter.ops();
					let removing = Removing {
						control: &self.control,
						ops: &ops,
						output: Some(self.written_archive(archive)),
					};
					dispose_target(&*self.backend, removing, target, how, &mut deleted).await
				}
			};
			// the source and those that go with it are told of as soon as its outcome is final
			let told = std::mem::take(&mut going[request]);
			for &nested in &told {
				let outcome = if nested == request {
					outcome.clone()
				} else {
					going_with(&outcome, files[nested] && deleted.contains(&uuids[nested]))
				};
				dispositions[nested] = Some(SourceDisposition {
					uuid: uuids[nested],
					outcome,
				});
			}
			let told: Vec<SourceDisposition> = told
				.iter()
				.filter_map(|&nested| dispositions[nested].clone())
				.collect();
			self.reporter.dispositions(&told);
		}
		dispositions
			.into_iter()
			.map(|disposition| disposition.expect("every source goes with an outermost one"))
			.collect()
	}

	/// Why every source is kept for what is wrong with the archive, if anything is: the job left
	/// entries out, a source was not read to its end, or the archive the server holds is not the
	/// one uploaded, or does not read back. The server is not asked when `every_source_kept` for
	/// a reason of its own.
	async fn archive_kept(
		&mut self,
		how: SourceDisposal,
		archive: &RemoteFile,
		read_back: Option<ReadBack>,
		incomplete: bool,
		every_source_kept: bool,
	) -> Option<KeptReason> {
		if incomplete {
			return Some(KeptReason::Incomplete);
		}
		if self
			.sources
			.iter()
			.any(|source| source.served != source.chunks)
		{
			// a source not read to its end had its hash never checked
			return Some(KeptReason::Unconfirmed);
		}
		if every_source_kept {
			return None;
		}
		// the archive as the server holds it
		let state = self
			.control
			.until_stopping(self.backend.file_state(archive.uuid()))
			.await;
		match state {
			Ok(Ok(state)) if self.written_archive(archive).stands(&state) => {
				self.read_back(how, archive, read_back).await
			}
			Ok(Ok(_)) => Some(KeptReason::Unconfirmed),
			Ok(Err(error)) => {
				tracing::warn!(
					"archive {}: failed to confirm it before removing its sources: {error}",
					archive.uuid()
				);
				Some(KeptReason::Unconfirmed)
			}
			Err(Stopped) => Some(KeptReason::Interrupted),
		}
	}

	/// `archive` as the job wrote it, which the sources are only removed while it still stands.
	fn written_archive(&self, archive: &RemoteFile) -> WrittenFile {
		WrittenFile {
			uuid: archive.uuid(),
			size: self.written,
			chunks: self.next_index,
		}
	}

	/// Why the sources are kept after reading `archive` back, which a permanent disposal needs:
	/// the encoders are the SDK's own, and nothing else would hold the data if one were wrong.
	async fn read_back(
		&mut self,
		how: SourceDisposal,
		archive: &RemoteFile,
		read_back: Option<ReadBack>,
	) -> Option<KeptReason> {
		if how == SourceDisposal::Trash {
			return None;
		}
		let Some(read_back) = read_back else {
			return Some(KeptReason::Unconfirmed);
		};
		// every source was read to its end, so each hash is of all of it; nothing reads the
		// sources' paths again
		let files = self
			.sources
			.iter_mut()
			.map(|source| (std::mem::take(&mut source.path), source.hasher.finalize()))
			.collect();
		let verdict = reads_back(
			&self.backend,
			&self.control,
			&self.reporter,
			archive,
			read_back,
			files,
		)
		.await;
		match verdict {
			Ok(true) => None,
			Ok(false) => Some(KeptReason::Unconfirmed),
			Err(Stopped) => Some(KeptReason::Interrupted),
		}
	}

	/// Registers the uploaded archive in the destination.
	async fn register(
		&mut self,
		name: ValidatedName,
		shape: NameShape,
		targets: ConnectedTargets,
	) -> Result<RemoteFile, Stopped> {
		let now = Utc::now();
		let completion = UploadCompletion {
			written: self.written,
			num_chunks: self.next_index,
			hash: Blake3Hash::from(
				self.head_last_hash
					.unwrap_or_else(|| self.hasher.finalize()),
			),
			final_times: (now, now),
		};
		let mut retry = NameRetry::new(shape, "item");
		let result = finalize_new_file(FinalizeTask {
			backend: &*self.backend,
			control: &self.control,
			ops: &self.reporter.ops(),
			upload: &self.upload,
			parent: self.destination,
			name,
			// the destination may hold the name by now
			recheck: Some(&mut retry),
			completion,
			info: self.info.clone().unwrap_or_default(),
			targets: &targets,
		})
		.await;
		match result {
			Ok(Finalized {
				file,
				propagation_errors,
				..
			}) => {
				for error in propagation_errors {
					self.reporter.event(CompressEvent::PropagationFailed {
						dest_uuid: file.uuid(),
						error: Arc::new(error),
					});
				}
				self.reporter.archive_created(file.clone(), self.written);
				Ok(file)
			}
			Err(FinalizeError::Stopped) => Err(Stopped),
			Err(FinalizeError::RegisteredAsVersion { file, .. }) => {
				self.fatal.record(Arc::new(Error::custom(
					ErrorKind::InvalidState,
					format!(
						"the archive was registered as a new version of the existing file {}",
						Uuid::from(file.stable_uuid)
					),
				)));
				Err(Stopped)
			}
			Err(FinalizeError::Failed(error)) => {
				self.fatal.record(Arc::new(error));
				Err(Stopped)
			}
		}
	}
}

impl DisposalTarget {
	fn uuid(&self) -> Uuid {
		match self {
			Self::File(file) => file.uuid,
			Self::Dir(dir) => dir.uuid,
			Self::Unavailable { uuid } => *uuid,
		}
	}
}

/// Removes `target`, which nothing holds back; what became of it. A folder's permanent removal
/// adds the files it deleted to `deleted`, even when it stopped part way.
async fn dispose_target<B: DisposalBackend>(
	backend: &B,
	removing: Removing<'_>,
	target: DisposalTarget,
	how: SourceDisposal,
	deleted: &mut BTreeSet<Uuid>,
) -> DisposalOutcome {
	match target {
		DisposalTarget::File(file) => dispose_file(backend, file, how, removing).await,
		DisposalTarget::Dir(dir) => dispose_dir(backend, &dir, how, removing, deleted).await,
		DisposalTarget::Unavailable { .. } => DisposalOutcome::kept(KeptReason::Changed),
	}
}

/// For each of `targets`, the one it goes with, if any: a source inside another (a file and its
/// folder both given) goes with that one, whose removal removes it, and its own attempt would only
/// find it gone. The first such one, when several are: the same item given twice goes with its
/// first. Linear in the targets and what the folders among them read, however many are given.
fn enclosing(targets: &[DisposalTarget]) -> Vec<Option<usize>> {
	// where each item is given, first to last
	let mut given: HashMap<Uuid, Vec<usize>> = HashMap::new();
	for (index, target) in targets.iter().enumerate() {
		given.entry(target.uuid()).or_default().push(index);
	}
	let mut outer: Vec<Option<usize>> = targets
		.iter()
		.enumerate()
		.map(|(index, target)| {
			given[&target.uuid()]
				.first()
				.filter(|&&first| first < index)
				.copied()
		})
		.collect();
	for (holder, target) in targets.iter().enumerate() {
		let DisposalTarget::Dir(dir) = target else {
			continue;
		};
		let read = dir
			.read
			.files
			.keys()
			.map(|uuid| (uuid, true))
			.chain(dir.read.dirs.iter().map(|uuid| (uuid, false)));
		for (uuid, is_file) in read {
			for &inner in given.get(uuid).into_iter().flatten() {
				// a file source is only ever among a folder's files
				let held = inner != holder
					&& (is_file || !matches!(targets[inner], DisposalTarget::File(_)));
				if held && outer[inner].is_none_or(|first| holder < first) {
					outer[inner] = Some(holder);
				}
			}
		}
	}
	outer
}

/// What became of a source that goes with another, whose removal ended in `outcome`: removed with
/// it, freeing nothing of its own, or kept with it, unless it is a file the other's permanent
/// removal `deleted` before it stopped.
fn going_with(outcome: &DisposalOutcome, deleted: bool) -> DisposalOutcome {
	match outcome {
		DisposalOutcome::Disposed { how, .. } => DisposalOutcome::Disposed {
			how: *how,
			bytes_freed: 0,
		},
		// a folder removed for good only in part may have taken a file given on its own too
		DisposalOutcome::Kept { .. } if deleted => DisposalOutcome::Disposed {
			how: SourceDisposal::DeletePermanently,
			bytes_freed: 0,
		},
		DisposalOutcome::Kept { reason, .. } => DisposalOutcome::kept(reason.clone()),
	}
}

#[cfg(test)]
mod tests;
