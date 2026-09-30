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
				DisposalBackend, DisposalOutcome, ExpectedDir, ExpectedFile, KeptReason, Nest,
				Removing, SourceDisposal, SourceDisposition, WrittenFile, dispose_dir,
				dispose_file, kept_on_early_end, nesting,
			},
			hash::HeadLastHasher,
			input::{PREFETCH_CHUNKS, take_memory, whole_chunk},
			limits::keep,
			worker::{
				CodecStart, StallWatch, WorkerEvent, WorkerLink, codec_failed, unexpected_event,
				worker_died,
			},
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
}

/// How to remove the sources once the archive is verified, and what the job read of them.
pub(crate) struct CompressDisposal {
	pub(crate) removal: Removal,
	/// The top-level sources, in request order.
	pub(crate) sources: Vec<DisposalSource>,
}

/// How the sources are removed.
pub(crate) enum Removal {
	/// Into the trash, where they can be restored: nothing is read back.
	Trash,
	/// For good, once the archive reads back as `read_back` reads it.
	DeletePermanently { read_back: ReadBack },
}

impl Removal {
	fn how(&self) -> SourceDisposal {
		match self {
			Self::Trash => SourceDisposal::Trash,
			Self::DeletePermanently { .. } => SourceDisposal::DeletePermanently,
		}
	}
}

/// A top-level source to remove.
#[derive(Debug)]
pub(crate) struct DisposalSource {
	pub(crate) target: DisposalTarget,
	/// Every file read below it had a hash in its metadata to check it against.
	pub(crate) hashed: bool,
}

/// A top-level source, as the job works out what becomes of it.
struct SourceRemoval {
	target: DisposalTarget,
	/// Why it is kept on its own account, whatever becomes of the others: a file below it that
	/// did not match its hash, or had none to check a permanent removal against.
	own_reason: Option<KeptReason>,
	nest: Nest,
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
	ChunkKey,
	Result<Vec<u8>, Error>,
	OwnedSemaphorePermit,
	OpGuard,
);

/// The archive's hash, taken as its chunks are uploaded.
enum ArchiveHash {
	/// The chunks come in order.
	Streaming(Box<blake3::Hasher>),
	/// The first chunk comes last ([`WorkerEvent::Head`]); the others are hashed as they come.
	HeadLast(HeadLastHasher),
	/// The first chunk came, last.
	Done(blake3::Hash),
}

/// An archive chunk from the codec.
enum ArchiveChunk {
	/// The next chunk in order.
	Next(Vec<u8>),
	/// The first chunk, sent last.
	Head(Vec<u8>),
}

struct SourceState {
	file: Arc<RemoteFileType<'static>>,
	path: String,
	request: usize,
	chunks: u64,
	/// Chunks handed to the codec, which asks for them in order.
	served: u64,
	hasher: blake3::Hasher,
	/// Its data did not match the hash in its metadata.
	mismatched: bool,
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
			mismatched: false,
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
	next_fetch: ChunkKey,
	/// The chunk the codec reads next.
	next_served: ChunkKey,
	fetches: FuturesOrdered<MaybeSendBoxFuture<'static, FetchedChunk>>,
	/// Fetched chunks the codec has not read yet; each counts as in flight, so the job does not
	/// count as paused while one still holds memory.
	ready: VecDeque<(ChunkKey, Vec<u8>, OwnedSemaphorePermit, OpGuard)>,
	reading: Option<OwnedSemaphorePermit>,
	ask: Option<(ChunkKey, oneshot::Sender<Vec<u8>>)>,

	destination: Uuid,
	upload: Arc<B::Upload>,
	archive_uuid: Uuid,
	hash: ArchiveHash,
	written: u64,
	next_index: u64,
	info: Option<RemoteFileInfo>,
	uploads: FuturesUnordered<MaybeSendBoxFuture<'static, (u64, Result<RemoteFileInfo, Error>)>>,
	/// An archive chunk waiting for memory or for an upload slot; the codec parks meanwhile.
	held: Option<ArchiveChunk>,
	events_closed: bool,
	/// The codec returned, or was given up on.
	codec_done: bool,
	max_bytes: Option<u64>,
	stall: StallWatch,
	/// The source files that did not match the hash in their metadata, for the report, and
	/// how many more there were past what it lists.
	hash_mismatches: Vec<HashMismatch>,
	omitted_hash_mismatches: u64,
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
	} = task;
	let shape = NameShape::FileWithExtension { len: extension_len };
	// every source a disposal was asked for is reported, however the job ends
	let requested: Vec<Uuid> = disposal
		.as_ref()
		.map(|disposal| {
			disposal
				.sources
				.iter()
				.map(|source| source.target.uuid())
				.collect()
		})
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
		hash: if head_last {
			ArchiveHash::HeadLast(HeadLastHasher::new())
		} else {
			ArchiveHash::Streaming(Box::default())
		},
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
		hash_mismatches: Vec::new(),
		omitted_hash_mismatches: 0,
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
		omitted_hash_mismatches,
		..
	} = driver;
	report.hash_mismatches = hash_mismatches;
	report.omitted_hash_mismatches = omitted_hash_mismatches;
	match fatal.end(outcome, &control, CompressReport::NAME) {
		(_, Ok((archive, dispositions))) => {
			report.dispositions = dispositions;
			// a cancel once the archive exists keeps the sources, but the job is done; it still
			// ends as cancelled jobs do, however late it was seen
			if control.is_cancelled() {
				reporter.set_cancelling();
			}
			reporter.finish(CompressPhase::Done);
			report.counts = reporter.counts();
			report.archive = Some(archive);
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
	fn first_chunk_from(&self, mut source: u32) -> ChunkKey {
		while source_at(&self.sources, source).is_some_and(|state| state.chunks == 0) {
			source += 1;
		}
		(source, 0)
	}

	/// The chunk after `chunk`, in the order the codec reads them.
	fn after(&self, (source, index): ChunkKey) -> ChunkKey {
		if source_at(&self.sources, source).is_some_and(|state| index + 1 < state.chunks) {
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
			&& let Some(source) = source_at(&self.sources, self.next_fetch.0)
		{
			let Some(permit) = take_memory(&self.input_slot, &self.memory) else {
				break;
			};
			let file = Arc::clone(&source.file);
			let key = self.next_fetch;
			self.next_fetch = self.after(key);
			let backend = Arc::clone(&self.backend);
			let op = self.reporter.op();
			self.fetches.push_back(Box::pin(async move {
				let result = backend.fetch_chunk(&file, key.1).await;
				(key, result, permit, op)
			}) as MaybeSendBoxFuture<'static, _>);
		}
		if let Some(chunk) = self.held.take() {
			self.take_chunk(chunk);
		}
	}

	fn fetch_finished(&mut self, (key, result, permit, op): FetchedChunk) {
		let source = source_at(&self.sources, key.0).expect("the driver fetches only its sources");
		match result.and_then(|data| whole_chunk(&source.file, key.1, data)) {
			Ok(data) => {
				self.ready.push_back((key, data, permit, op));
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
		let (key, data, permit, _op) = self.ready.pop_front().expect("just checked");
		let (_, reply) = self.ask.take().expect("just checked");
		self.reading = Some(permit);
		self.next_served = self.after(key);
		let len = data.len() as u64;
		let state = usize::try_from(key.0)
			.ok()
			.and_then(|source| self.sources.get_mut(source))
			.expect("the driver fetches only its sources");
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
			state.mismatched = true;
			let mismatch = HashMismatch {
				source_uuid: state.file.uuid(),
				path: state.path.clone(),
			};
			keep(
				&mut self.hash_mismatches,
				&mut self.omitted_hash_mismatches,
				mismatch.clone(),
			);
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
				// an ask out of order would never be served: the chunks are fetched in order
				if (source, index) != self.next_served {
					self.stop_with(Error::custom(
						ErrorKind::Internal,
						"the codec read its sources out of order",
					));
					return;
				}
				self.ask = Some(((source, index), reply));
				self.serve_ask();
			}
			WorkerEvent::Data(data) => self.take_chunk(ArchiveChunk::Next(data)),
			WorkerEvent::Head(data) => self.take_chunk(ArchiveChunk::Head(data)),
			WorkerEvent::FileEnd => self.reporter.file_done(),
			// the compressing codec sends nothing else
			WorkerEvent::Opened(_)
			| WorkerEvent::Entry(_)
			| WorkerEvent::Skipped(_)
			| WorkerEvent::Link(_)
			| WorkerEvent::Listed(_) => self.stop_with(unexpected_event()),
		}
	}

	/// Uploads an archive chunk (the first one when `head`), or holds it until memory or an
	/// upload slot is free.
	fn take_chunk(&mut self, chunk: ArchiveChunk) {
		let permit = if self.uploads.len() < UPLOADS_AT_ONCE {
			take_memory(&self.output_slot, &self.memory)
		} else {
			None
		};
		let Some(permit) = permit else {
			self.held = Some(chunk);
			return;
		};
		let (ArchiveChunk::Next(data) | ArchiveChunk::Head(data)) = &chunk;
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
		let (index, data) = match (chunk, &mut self.hash) {
			(ArchiveChunk::Next(data), ArchiveHash::Streaming(hasher)) => {
				hasher.update_rayon(&data);
				self.next_index += 1;
				(self.next_index - 1, data)
			}
			(ArchiveChunk::Next(data), ArchiveHash::HeadLast(hasher)) => {
				hasher.update(&data);
				self.next_index += 1;
				(self.next_index - 1, data)
			}
			(ArchiveChunk::Head(data), ArchiveHash::HeadLast(hasher)) => {
				self.hash = ArchiveHash::Done(hasher.finalize(&data));
				(0, data)
			}
			(ArchiveChunk::Next(_), ArchiveHash::Done(_))
			| (ArchiveChunk::Head(_), ArchiveHash::Streaming(_) | ArchiveHash::Done(_)) => {
				self.stop_with(Error::custom(
					ErrorKind::Internal,
					"the codec sent the archive's chunks out of order",
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
		self.reporter.set_phase(match disposal.removal {
			Removal::DeletePermanently { .. } => CompressPhase::Verifying,
			Removal::Trash => CompressPhase::DisposingSources,
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
		let CompressDisposal { removal, sources } = disposal;
		let how = removal.how();
		let nests = nesting(&enclosing(&sources));
		// a source whose own files could not be checked is kept on its own; anything wrong
		// with the archive keeps them all
		let mut removals: Vec<SourceRemoval> = sources
			.into_iter()
			.zip(nests)
			.map(|(source, nest)| SourceRemoval {
				own_reason: (how == SourceDisposal::DeletePermanently && !source.hashed)
					.then_some(KeptReason::HashUnavailable),
				target: source.target,
				nest,
			})
			.collect();
		for source in self.sources.iter().filter(|source| source.mismatched) {
			removals[source.request].own_reason = Some(KeptReason::HashMismatch);
		}
		let every_source_kept = removals.iter().all(|removal| removal.own_reason.is_some());
		let archive_reason = self
			.archive_kept(removal, archive, incomplete, every_source_kept)
			.await;
		self.reporter.set_phase(CompressPhase::DisposingSources);
		let mut dispositions: Vec<Option<SourceDisposition>> = vec![None; removals.len()];
		// the sources each outermost one goes with, itself included
		let mut going = vec![Vec::new(); removals.len()];
		for (source, removal) in removals.iter().enumerate() {
			going[removal.nest.outermost(source)].push(source);
		}
		for (request, removal) in removals.iter().enumerate() {
			let held_back = match removal.nest {
				Nest::Within(_) => continue,
				Nest::Cyclic => Some(KeptReason::Changed),
				Nest::Outermost => archive_reason
					.clone()
					.or_else(|| removal.own_reason.clone()),
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
					dispose_target(&*self.backend, removing, &removal.target, how, &mut deleted)
						.await
				}
			};
			// the source and those that go with it are told of as soon as its outcome is final
			let told = std::mem::take(&mut going[request]);
			for &nested in &told {
				let target = &removals[nested].target;
				let outcome = if nested == request {
					outcome.clone()
				} else {
					let file = matches!(target, DisposalTarget::File(_));
					going_with(&outcome, file && deleted.contains(&target.uuid()))
				};
				dispositions[nested] = Some(SourceDisposition {
					uuid: target.uuid(),
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
		removal: Removal,
		archive: &RemoteFile,
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
				self.read_back(removal, archive).await
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
	async fn read_back(&mut self, removal: Removal, archive: &RemoteFile) -> Option<KeptReason> {
		let Removal::DeletePermanently { read_back } = removal else {
			// trashed sources can be restored
			return None;
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
			hash: Blake3Hash::from(match &self.hash {
				ArchiveHash::Streaming(hasher) => hasher.finalize(),
				ArchiveHash::Done(hash) => *hash,
				// the codec's length would not match what was handed over
				ArchiveHash::HeadLast(_) => {
					unreachable!("an archive is registered without its head")
				}
			}),
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

/// Source `n`, by the number the codec asks for it by; `None` past the last.
fn source_at(sources: &[SourceState], n: u32) -> Option<&SourceState> {
	sources.get(usize::try_from(n).ok()?)
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
	target: &DisposalTarget,
	how: SourceDisposal,
	deleted: &mut BTreeSet<Uuid>,
) -> DisposalOutcome {
	match target {
		DisposalTarget::File(file) => dispose_file(backend, *file, how, removing).await,
		DisposalTarget::Dir(dir) => dispose_dir(backend, dir, how, removing, deleted).await,
		DisposalTarget::Unavailable { .. } => DisposalOutcome::kept(KeptReason::Changed),
	}
}

/// For each of `targets`, the one it goes with, if any: a source inside another (a file and its
/// folder both given) goes with that one, whose removal removes it, and its own attempt would only
/// find it gone. The first such one, when several are: the same item given twice goes with its
/// first. Linear in the targets and what the folders among them read, however many are given.
fn enclosing(sources: &[DisposalSource]) -> Vec<Option<usize>> {
	// where each item is given, first to last
	let mut given: HashMap<Uuid, Vec<usize>> = HashMap::new();
	for (index, source) in sources.iter().enumerate() {
		given.entry(source.target.uuid()).or_default().push(index);
	}
	let mut outer: Vec<Option<usize>> = sources
		.iter()
		.enumerate()
		.map(|(index, source)| {
			given[&source.target.uuid()]
				.first()
				.filter(|&&first| first < index)
				.copied()
		})
		.collect();
	for (holder, source) in sources.iter().enumerate() {
		let DisposalTarget::Dir(dir) = &source.target else {
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
					&& (is_file || !matches!(sources[inner].target, DisposalTarget::File(_)));
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
