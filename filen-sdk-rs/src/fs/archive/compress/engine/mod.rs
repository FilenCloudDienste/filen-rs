//! Runs a compression: the async driver of the codec worker. It fetches each source file for
//! the codec in the order the codec writes them, uploads the archive the codec produces as one
//! new file, and registers it in the destination once all of it is up. Nothing becomes visible
//! before that, so a job that stops early leaves nothing behind.
//!
//! Memory, pause and cancel work as in extraction (see the extract engine): a two-chunk floor
//! for one input and one output chunk, more only from the client's budget when it is free right
//! now, and a pause that gives back the floor, the prefetched chunks and every reservation from
//! the client's budget once in-flight work is done. A paused job keeps its codec's state and the
//! source chunk the codec is reading resident.

use std::{
	collections::{BTreeSet, VecDeque},
	io,
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
			config::{ArchiveConfig, CHUNK_BYTES},
			dispose::{
				DisposalBackend, DisposalOutcome, ExpectedFile, KeptReason, SourceDisposal,
				SourceDisposition, Tree, dispose_dir, dispose_file, kept_on_early_end,
			},
			hash::HeadLastHasher,
			limits::MAX_REPORT_RECORDS,
			worker::{ARCHIVE_STALL_TIMEOUT, WorkerEvent, WorkerLink},
		},
		categories::{DirType, Normal},
		drive_job::{
			backend::{DriveBackend, UploadSpec},
			finalize::{FinalizeError, FinalizeTask, Finalized, finalize_new_file},
			name_retry::NameRetry,
		},
		file::{
			RemoteFile,
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
	job::{JobControl, Stopped, report::OpGuard},
	util::{MaybeArc, MaybeSendBoxFuture, sleep},
};

use super::report::{
	CompressEvent, CompressFailed, CompressPhase, CompressReport, HashMismatch, Reporter,
};

/// Archive chunks uploading at once.
const UPLOADS_AT_ONCE: usize = 4;

/// Source chunks fetched ahead of the codec, memory permitting.
const PREFETCH_CHUNKS: usize = 4;

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
	pub(crate) start: Box<dyn FnOnce() -> Result<WorkerLink<CodecResult>, Error> + Send>,
	/// The report so far: the plan's totals, skips and renames.
	pub(crate) report: CompressReport,
	pub(crate) disposal: Option<CompressDisposal>,
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
	Dir {
		uuid: Uuid,
		read: Tree,
	},
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
	ask: Option<(ChunkKey, oneshot::Sender<io::Result<Vec<u8>>>)>,

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
	codec_result: Option<CodecResult>,
	max_bytes: Option<u64>,
	stamp: u64,
	stalled_ticks: u32,
	/// The top-level sources a file of which did not match the hash in its metadata.
	mismatched: BTreeSet<usize>,
	/// The files behind `mismatched`, for the report.
	hash_mismatches: Vec<HashMismatch>,
	fatal: Option<Arc<Error>>,
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
		.map(|disposal| disposal.targets.iter().map(DisposalTarget::uuid).collect())
		.unwrap_or_default();
	let fail = |report, phase, error| end_early(&reporter, report, &requested, phase, error);
	for source in &sources {
		if let Err(error) = check_chunks_consistent(source.file.chunks(), source.file.size()) {
			return Err(fail(report, CompressPhase::Failed, Arc::new(error)));
		}
	}

	// the archive's name is picked against the destination up front, and checked again when it
	// is registered
	let prepared = async {
		let listed = backend.list_dir_names(&destination).await?;
		let targets = backend.connected_targets(destination.uuid()).await?;
		let name =
			TakenNames::new(listed.names.iter().map(String::as_str)).allocate(name, shape)?;
		Ok::<_, Error>((name, targets))
	};
	let (name, targets) = match control.until_stopping(prepared).await {
		Ok(Ok(prepared)) => prepared,
		Ok(Err(error)) => return Err(fail(report, CompressPhase::Failed, Arc::new(error))),
		Err(Stopped) => return Err(fail(report, CompressPhase::Cancelled, cancelled())),
	};

	reporter.set_phase(CompressPhase::WaitingForWorker);
	let Ok((_lease, floor)) = config.admit(&control, &reporter.ops()).await else {
		reporter.set_cancelling();
		return Err(fail(report, CompressPhase::Cancelled, cancelled()));
	};
	reporter.set_phase(CompressPhase::Compressing);
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
	let sources = sources
		.into_iter()
		.map(|source| SourceState {
			chunks: source.file.size().div_ceil(CHUNK_SIZE_U64),
			file: Arc::new(source.file),
			path: source.path,
			request: source.request,
			served: 0,
			hasher: blake3::Hasher::new(),
		})
		.collect();
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
		codec_result: None,
		max_bytes,
		stamp: 0,
		stalled_ticks: 0,
		mismatched: BTreeSet::new(),
		hash_mismatches: Vec::new(),
		fatal: None,
	};
	let incomplete = !report.skipped.is_empty();
	let mut dispositions = Vec::new();
	driver.next_fetch = driver.first_chunk_from(0);
	driver.next_served = driver.next_fetch;
	let outcome = async {
		driver.compress().await?;
		if driver.fatal.is_some() {
			return Err(Stopped);
		}
		driver.reporter.set_phase(CompressPhase::Finishing);
		// the destination may have been shared or linked since the job started
		let refetched = driver
			.control
			.until_stopping(driver.backend.connected_targets(driver.destination))
			.await?;
		let targets = match refetched {
			Ok(current) => current,
			Err(error) => {
				tracing::warn!("failed to re-check the archive destination's shares: {error}");
				targets
			}
		};
		let archive = driver.register(name, shape, targets).await?;
		// the archive exists from here on: a cancel now keeps the sources, but the job is done
		if let Some(disposal) = disposal {
			driver.reporter.set_phase(CompressPhase::DisposingSources);
			dispositions = match driver.reporter.checkpoint(&driver.control).await {
				Ok(()) => driver.dispose(disposal, &archive, incomplete).await,
				Err(Stopped) => {
					let kept = kept_on_early_end(&requested, true);
					driver.reporter.dispositions(&kept);
					kept
				}
			};
		}
		Ok(archive)
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
	match (outcome, fatal) {
		(Ok(archive), None) => {
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
		(_, Some(error)) => Err(end_early(
			&reporter,
			report,
			&requested,
			CompressPhase::Failed,
			error,
		)),
		(Err(Stopped), None) if control.is_cancelled() => Err(end_early(
			&reporter,
			report,
			&requested,
			CompressPhase::Cancelled,
			cancelled(),
		)),
		(Err(Stopped), None) => Err(end_early(
			&reporter,
			report,
			&requested,
			CompressPhase::Failed,
			Arc::new(Error::custom(ErrorKind::Internal, "compression stopped")),
		)),
	}
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
	reporter.finish_early(phase, report.totals);
	report.counts = reporter.counts();
	CompressFailed { report, error }
}

pub(crate) fn cancelled() -> Arc<Error> {
	Arc::new(Error::custom(ErrorKind::Cancelled, "compression cancelled"))
}

fn worker_died() -> Error {
	Error::custom(
		ErrorKind::ArchiveWorkerDied,
		"the archive's codec stopped responding",
	)
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
		if self.fatal.is_none() {
			self.fatal = Some(Arc::new(error));
		}
		self.control.stop();
		self.reporter.set_cancelling();
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
			if self.codec_result.is_some() && self.uploads.is_empty() && self.held.is_none() {
				return Ok(());
			}
			if pause_requested && self.uploads.is_empty() && self.fetches.is_empty() {
				self.pause().await?;
				continue;
			}
			let take_events = !pause_requested && self.held.is_none() && !self.events_closed;
			let await_result = self.events_closed && self.codec_result.is_none();
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
			let Some(permit) = self.take_memory(&self.input_slot) else {
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

	fn take_memory(&self, slot: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
		Arc::clone(slot).try_acquire_owned().ok().or_else(|| {
			Arc::clone(&self.memory)
				.try_acquire_many_owned(u32::try_from(CHUNK_BYTES).expect(
					"a full chunk is about 1 MiB, far below u32::MAX (should be impossible)",
				))
				.ok()
		})
	}

	fn fetch_finished(&mut self, ((source, index), result, permit, op): FetchedChunk) {
		let file = &self.sources[source as usize].file;
		let expected = chunk_plaintext_len(file.size(), index);
		match result {
			Ok(data) if data.len() as u64 == expected => {
				self.ready.push_back(((source, index), data, permit, op));
				self.serve_ask();
			}
			Ok(data) => self.stop_with(Error::custom(
				ErrorKind::Response,
				format!(
					"chunk {index} of a source holds {} bytes instead of {expected}",
					data.len()
				),
			)),
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
			if self.hash_mismatches.len() < MAX_REPORT_RECORDS {
				self.hash_mismatches.push(HashMismatch {
					source_uuid: state.file.uuid(),
					path: state.path.clone(),
				});
			}
			let event = CompressEvent::SourceHashMismatch {
				source_uuid: state.file.uuid(),
				path: state.path.clone(),
			};
			self.reporter.event(event);
		}
		self.reporter.source_read(len);
		let _ = reply.send(Ok(data));
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
			WorkerEvent::Opened(_) | WorkerEvent::Entry(_) | WorkerEvent::Skipped(_) => {}
		}
	}

	/// Uploads an archive chunk (the first one when `head`), or holds it until memory or an
	/// upload slot is free.
	fn take_data(&mut self, data: Vec<u8>, head: bool) {
		let permit = if self.uploads.len() < UPLOADS_AT_ONCE {
			self.take_memory(&self.output_slot)
		} else {
			None
		};
		let Some(permit) = permit else {
			self.held = Some((data, head));
			return;
		};
		let len = data.len() as u64;
		if let Some(max) = self.max_bytes
			&& self.written + len >= max
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
					ErrorKind::ArchiveCorrupt,
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
		let stamp = self.link.shared.progress();
		let owed = self.ask.is_some() || self.held.is_some() || pause_requested;
		if owed || stamp != self.stamp || self.codec_result.is_some() {
			self.stamp = stamp;
			self.stalled_ticks = 0;
			return;
		}
		self.stalled_ticks += 1;
		if CALLBACK_INTERVAL * self.stalled_ticks >= ARCHIVE_STALL_TIMEOUT {
			tracing::error!(
				"archive {}: the codec made no progress for {ARCHIVE_STALL_TIMEOUT:?}",
				self.archive_uuid
			);
			self.link.retire();
			self.codec_result = Some(Err(worker_died()));
			self.stop_with(worker_died());
		}
	}

	fn codec_finished(&mut self, result: CodecResult) {
		// the codec reads nothing more
		self.reading = None;
		match &result {
			Ok(len) if *len != self.written => self.stop_with(Error::custom(
				ErrorKind::Internal,
				format!(
					"the codec wrote {len} archive bytes, {} were handed over",
					self.written
				),
			)),
			Ok(_) => {}
			// An error the driver caused (it failed a fetch, or stopped) is already the job's.
			Err(error) if self.fatal.is_some() || self.control.is_stopping() => {
				tracing::debug!("archive codec ended after the job did: {error}");
			}
			Err(error) => {
				// a dead codec is a bug to hear of, where a damaged archive is only the user's
				if error.kind() == ErrorKind::ArchiveWorkerDied {
					tracing::error!("archive {}: {error}", self.archive_uuid);
				} else {
					tracing::warn!("archive {}: {error}", self.archive_uuid);
				}
				self.stop_with(Error::custom(error.kind(), error.to_string()));
			}
		}
		self.codec_result = Some(result);
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
		// a source whose own files could not be checked is kept on its own; anything wrong
		// with the archive keeps them all
		let own_reason = |request: usize| {
			if self.mismatched.contains(&request) {
				Some(KeptReason::HashMismatch)
			} else if how == SourceDisposal::DeletePermanently && !hashed[request] {
				Some(KeptReason::HashUnavailable)
			} else {
				None
			}
		};
		let archive_reason = if incomplete {
			Some(KeptReason::Incomplete)
		} else if self
			.sources
			.iter()
			.any(|source| source.served != source.chunks)
		{
			// a source not read to its end had its hash never checked
			Some(KeptReason::Unconfirmed)
		} else if (0..targets.len()).all(|request| own_reason(request).is_some()) {
			None
		} else {
			// the archive as the server holds it
			match self.backend.file_state(archive.uuid()).await {
				Ok(state)
					if !state.trash
						&& !state.versioned
						&& state.size == self.written
						&& state.chunks == self.next_index =>
				{
					None
				}
				_ => Some(KeptReason::Unconfirmed),
			}
		};
		// a source inside another (a file and its folder both given) goes with that one: its
		// removal removes it, and its own attempt would only find it gone
		let mut within: Vec<Option<usize>> = targets
			.iter()
			.enumerate()
			.map(|(index, target)| {
				targets.iter().enumerate().position(|(other, outer)| {
					other != index
						// the same item given twice goes with its first
						&& (other < index && outer.uuid() == target.uuid()
							|| match (target, outer) {
							(DisposalTarget::File(file), DisposalTarget::Dir { read, .. }) => {
								read.files.contains_key(&file.uuid)
							}
							(
								DisposalTarget::Dir { uuid, .. }
								| DisposalTarget::Unavailable { uuid },
								DisposalTarget::Dir { read, .. },
							) => read.dirs.contains(uuid) || read.files.contains_key(uuid),
							_ => false,
						})
				})
			})
			.collect();
		// a move between two folders' listings can leave each read holding the other: a chain
		// like that has no outermost source, so every source on or behind it is kept as changed
		let cyclic: Vec<bool> = (0..within.len())
			.map(|request| {
				let mut outer = request;
				for _ in 0..within.len() {
					match within[outer] {
						Some(next) => outer = next,
						None => return false,
					}
				}
				true
			})
			.collect();
		for (within, _) in within
			.iter_mut()
			.zip(&cyclic)
			.filter(|(_, cyclic)| **cyclic)
		{
			*within = None;
		}
		// the outermost source each one goes with (itself, if none): the chains are acyclic, the
		// cycles were cut above
		let outermost: Vec<usize> = (0..within.len())
			.map(|mut outer| {
				while let Some(next) = within[outer] {
					outer = next;
				}
				outer
			})
			.collect();
		let uuids: Vec<Uuid> = targets.iter().map(DisposalTarget::uuid).collect();
		let files: Vec<bool> = targets
			.iter()
			.map(|target| matches!(target, DisposalTarget::File(_)))
			.collect();
		let mut dispositions: Vec<Option<SourceDisposition>> = vec![None; targets.len()];
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
			let outcome = match (held_back, target) {
				(Some(reason), _) => DisposalOutcome::Kept {
					reason,
					bytes_freed: 0,
				},
				(None, DisposalTarget::File(file)) => {
					dispose_file(&*self.backend, file, how, &self.control).await
				}
				(None, DisposalTarget::Dir { uuid, read }) => {
					dispose_dir(
						&*self.backend,
						uuid,
						&read,
						how,
						&self.control,
						&mut deleted,
					)
					.await
				}
				(None, DisposalTarget::Unavailable { .. }) => DisposalOutcome::Kept {
					reason: KeptReason::Changed,
					bytes_freed: 0,
				},
			};
			// the source and those that go with it are told of as soon as its outcome is final
			let told: Vec<usize> = (0..uuids.len())
				.filter(|&nested| outermost[nested] == request)
				.collect();
			for &nested in &told {
				let outcome = match &outcome {
					_ if nested == request => outcome.clone(),
					DisposalOutcome::Disposed { how, .. } => DisposalOutcome::Disposed {
						how: *how,
						bytes_freed: 0,
					},
					// a folder removed for good only in part may have taken a file given on
					// its own too
					DisposalOutcome::Kept { .. }
						if files[nested] && deleted.contains(&uuids[nested]) =>
					{
						DisposalOutcome::Disposed {
							how: SourceDisposal::DeletePermanently,
							bytes_freed: 0,
						}
					}
					DisposalOutcome::Kept { reason, .. } => DisposalOutcome::Kept {
						reason: reason.clone(),
						bytes_freed: 0,
					},
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
				self.fatal = Some(Arc::new(Error::custom(
					ErrorKind::InvalidState,
					format!(
						"the archive was registered as a new version of the existing file {}",
						Uuid::from(file.stable_uuid)
					),
				)));
				Err(Stopped)
			}
			Err(FinalizeError::Failed(error)) => {
				self.fatal = Some(Arc::new(error));
				Err(Stopped)
			}
		}
	}
}

impl DisposalTarget {
	fn uuid(&self) -> Uuid {
		match self {
			Self::File(file) => file.uuid,
			Self::Dir { uuid, .. } | Self::Unavailable { uuid } => *uuid,
		}
	}
}

#[cfg(test)]
mod tests;
