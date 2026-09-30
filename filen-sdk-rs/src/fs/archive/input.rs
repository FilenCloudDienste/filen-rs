//! The archive as a reading codec gets it, for an extraction, a listing and a compression's
//! read-back alike: fetched chunk by chunk as the codec asks, a few chunks ahead when the
//! client's memory has room right now, and hashed as it is read; and the one loop their drivers
//! share to serve it, take its events and its result, and give it up when it stops moving.

use std::{collections::VecDeque, sync::Arc};

use filen_types::crypto::Blake3Hash;
use futures::{StreamExt, stream::FuturesOrdered};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::{
	Error, ErrorKind,
	consts::{CHUNK_SIZE_U64, FULL_CHUNK_BYTES},
	fs::{
		HasUUID,
		archive::{
			config::ArchiveConfig,
			worker::{StallWatch, WorkerEvent, WorkerLink, worker_died},
		},
		drive_job::{backend::DriveBackend, cancelled},
		file::{
			enums::RemoteFileType,
			read::{check_chunks_consistent, chunk_plaintext_len},
			traits::HasFileInfo,
		},
	},
	job::{
		JobControl, Stopped,
		report::{JobPhase, JobState, OpGuard, Ops, Reporter},
	},
	util::{MaybeArc, MaybeSendBoxFuture},
};

/// Chunks fetched ahead of a codec, memory permitting: of the archive it reads, or of the
/// sources a compression reads.
pub(crate) const PREFETCH_CHUNKS: usize = 4;

/// A fetched chunk of the archive, with the memory it holds.
type FetchedChunk = (u64, Result<Vec<u8>, Error>, OwnedSemaphorePermit, OpGuard);

/// Memory for one chunk: `slot` (the job's floor of one input or one output chunk) when free,
/// else the client's `memory` if it has room now, else nothing.
pub(crate) fn take_memory(
	slot: &Arc<Semaphore>,
	memory: &Arc<Semaphore>,
) -> Option<OwnedSemaphorePermit> {
	Arc::clone(slot).try_acquire_owned().ok().or_else(|| {
		Arc::clone(memory)
			.try_acquire_many_owned(
				u32::try_from(FULL_CHUNK_BYTES).expect(
					"a full chunk is about 1 MiB, far below u32::MAX (should be impossible)",
				),
			)
			.ok()
	})
}

/// `data`, fetched as chunk `index` of `file`, when it holds as much as that chunk does: a
/// short one would shift everything after it.
pub(crate) fn whole_chunk(
	file: &RemoteFileType<'static>,
	index: u64,
	data: Vec<u8>,
) -> Result<Vec<u8>, Error> {
	let expected = chunk_plaintext_len(file.size(), index);
	if data.len() as u64 == expected {
		return Ok(data);
	}
	Err(Error::custom(
		ErrorKind::Response,
		format!(
			"chunk {index} of {} holds {} bytes instead of {expected}",
			file.uuid(),
			data.len()
		),
	))
}

struct ArchiveInput<B> {
	backend: Arc<B>,
	archive: Arc<RemoteFileType<'static>>,
	chunks: u64,
	next_fetch: u64,
	fetches: FuturesOrdered<MaybeSendBoxFuture<'static, FetchedChunk>>,
	/// Fetched chunks the codec has not asked for yet; held only while the feed counts as
	/// running, which counts them in flight.
	ready: VecDeque<(u64, Vec<u8>, OwnedSemaphorePermit)>,
	/// The chunk the codec is reading, released when it asks for the next.
	reading: Option<OwnedSemaphorePermit>,
	/// The chunk the codec reads next.
	served: u64,
	/// Chunks served one after another since the codec last jumped.
	run: u64,
	/// The archive's plaintext as the codec read it, in order.
	hasher: blake3::Hasher,
	/// The codec read the archive front to back, once: `hasher` covers it all.
	sequential: bool,
	ask: Option<(u64, oneshot::Sender<Vec<u8>>)>,
	/// The job's floor of input: one chunk it can always take, outside the client's budget, so
	/// it never waits on memory a transfer or another job holds.
	slot: Arc<Semaphore>,
	/// The client's file-IO budget, for what goes beyond the floor.
	memory: Arc<Semaphore>,
}

impl<B: DriveBackend> ArchiveInput<B> {
	fn new(backend: Arc<B>, archive: Arc<RemoteFileType<'static>>) -> Self {
		Self {
			chunks: archive.size().div_ceil(CHUNK_SIZE_U64),
			memory: backend.memory(),
			backend,
			archive,
			next_fetch: 0,
			fetches: FuturesOrdered::new(),
			ready: VecDeque::new(),
			reading: None,
			served: 0,
			run: 0,
			hasher: blake3::Hasher::new(),
			sequential: true,
			ask: None,
			slot: Arc::new(Semaphore::new(1)),
		}
	}

	fn archive(&self) -> &Arc<RemoteFileType<'static>> {
		&self.archive
	}

	/// The codec asks for chunk `index`, which means it is done with the one before; answered
	/// by [`Self::advance`] once the chunk is in.
	fn ask(&mut self, index: u64, reply: oneshot::Sender<Vec<u8>>) {
		self.reading = None;
		if index != self.served {
			// a jump (a zip is read from its end): what was fetched ahead is of no use
			self.sequential = false;
			self.fetches = FuturesOrdered::new();
			self.ready.clear();
			self.next_fetch = index;
			self.served = index;
			self.run = 0;
		}
		self.ask = Some((index, reply));
		self.serve_ask();
	}

	/// The codec ended: it holds no chunk any more.
	fn codec_done(&mut self) {
		self.reading = None;
	}

	/// Whether the codec waits on a chunk.
	fn owes_codec(&self) -> bool {
		self.ask.is_some()
	}

	fn fetching(&self) -> bool {
		!self.fetches.is_empty()
	}

	/// The next chunk fetched, in the order they were asked for.
	async fn fetched(&mut self) -> Option<FetchedChunk> {
		self.fetches.next().await
	}

	/// Answers the codec when its chunk is in, and fetches ahead while memory is free right
	/// now; `ops` counts the fetches in flight.
	fn advance(&mut self, ops: &Ops) {
		self.serve_ask();
		// Nothing is fetched ahead of what the codec asks for until it has read two chunks one
		// after the other, since it started or last jumped: a zip or 7z reads its head, then its
		// index at the end, then entries anywhere, and chunks fetched ahead of a read that jumps
		// go unread (all of them, when only the index is listed).
		let ahead = if self.run >= 2 { PREFETCH_CHUNKS } else { 1 };
		while self.fetches.len() + self.ready.len() < ahead
			&& self.next_fetch < self.chunks
			// short of a run, only the chunk the codec waits for
			&& (ahead > 1 || self.ask.is_some() && self.next_fetch <= self.served)
		{
			let Some(permit) = take_memory(&self.slot, &self.memory) else {
				break;
			};
			let index = self.next_fetch;
			self.next_fetch += 1;
			let backend = Arc::clone(&self.backend);
			let archive = Arc::clone(&self.archive);
			let op = ops.op();
			self.fetches.push_back(Box::pin(async move {
				let result = backend.fetch_chunk(&archive, index).await;
				(index, result, permit, op)
			}) as MaybeSendBoxFuture<'static, _>);
		}
	}

	/// Takes in a fetched chunk; the error that ends the job when it failed or came back short.
	fn fetch_finished(&mut self, (index, result, permit, _op): FetchedChunk) -> Result<(), Error> {
		let data = whole_chunk(&self.archive, index, result?)?;
		self.ready.push_back((index, data, permit));
		self.serve_ask();
		Ok(())
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
		self.run += 1;
		self.hasher.update_rayon(&data);
		let _ = reply.send(data);
	}

	fn drop_prefetched(&mut self) {
		self.fetches = FuturesOrdered::new();
		self.ready.clear();
		// the codec reads in order, so fetching starts again at the chunk it needs next
		self.next_fetch = self.served;
	}

	/// Drops every fetch and what was fetched, and the codec's ask: the job is stopping.
	fn drop_all(&mut self) {
		self.drop_prefetched();
		self.ask = None;
	}

	/// Gives back everything, once the codec is done with the archive (a zip's or 7z's index
	/// chunks may have been fetched again for their entries).
	fn release(&mut self) {
		self.drop_all();
		self.reading = None;
	}

	/// Waits out a pause holding nothing but the chunk the codec is reading: prefetched chunks
	/// are given back, and so is `running`, taken again once the pause is over.
	async fn wait_out_pause<S: JobState>(
		&mut self,
		running: &mut Option<OpGuard>,
		reporter: &MaybeArc<Reporter<S>>,
		control: &JobControl,
	) -> Result<(), Stopped> {
		self.drop_prefetched();
		// The chunk the codec is reading stays with it, part of its state. Nothing else holds the
		// input slot now, so it moves there if it took from the client's budget.
		if self.reading.is_some()
			&& let Ok(slot) = Arc::clone(&self.slot).try_acquire_owned()
		{
			self.reading = Some(slot);
		}
		// last, so the job counts as paused only now
		*running = None;
		reporter.checkpoint(control).await?;
		*running = Some(reporter.op());
		Ok(())
	}

	/// The hash of the whole archive, when the codec read it front to back, once.
	fn read_whole(&self) -> Option<Blake3Hash> {
		(self.sequential && self.served == self.chunks)
			.then(|| Blake3Hash::from(self.hasher.finalize()))
	}
}

/// A reading codec and the archive it reads, as the drivers of an extraction, a listing and a
/// compression's read-back all serve it: its asks answered from the archive, its events and its
/// result taken, and the codec given up on once it stops moving.
pub(crate) struct CodecFeed<B, T> {
	input: ArchiveInput<B>,
	link: WorkerLink<Result<T, Error>>,
	/// Counts the feed as an operation in flight while the codec reads, so the job is only
	/// reported paused once the feed gave back everything it held on top of the codec's chunk.
	running: Option<OpGuard>,
	stall: StallWatch,
	stage: Stage,
}

/// How far the feed has taken what the codec sends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
	/// Its events still come.
	Events,
	/// Its events ended: its result comes next.
	Result,
	/// Its result was taken, or the codec given up on.
	Done,
}

/// What [`CodecFeed::next`] took.
pub(crate) enum Fed<T> {
	/// A chunk fetched came in; `Err` with the error that ends the job when it failed or came
	/// back short.
	Fetched(Result<(), Error>),
	/// The codec asked for a chunk, handed over once it is in (at once, when it already was).
	Asked,
	Event(WorkerEvent),
	/// The codec sends no more events.
	EventsClosed,
	/// The codec returned: how, or [`worker_died`] when it went without saying.
	Finished(Result<T, Error>),
}

impl<B: DriveBackend, T> CodecFeed<B, T> {
	/// A feed of `archive` to the codec at the other end of `link`, counted as `running`.
	pub(crate) fn new(
		backend: Arc<B>,
		archive: Arc<RemoteFileType<'static>>,
		link: WorkerLink<Result<T, Error>>,
		running: OpGuard,
	) -> Self {
		Self {
			input: ArchiveInput::new(backend, archive),
			link,
			running: Some(running),
			stall: StallWatch::default(),
			stage: Stage::Events,
		}
	}

	/// The next of: a fetch finishing, the codec's next event when `take_events`, and its result
	/// once its events ended, when `take_result`. Cancel-safe, as a branch of the driver's own
	/// `select!`: nothing is taken until it is returned.
	pub(crate) async fn next(&mut self, take_events: bool, take_result: bool) -> Fed<T> {
		let take_events = take_events && self.stage == Stage::Events;
		let take_result = take_result && self.stage == Stage::Result;
		tokio::select! {
			biased;
			Some(fetched) = self.input.fetched(), if self.input.fetching() => {
				Fed::Fetched(self.input.fetch_finished(fetched))
			}
			event = self.link.events.recv(), if take_events => match event {
				Some(WorkerEvent::Ask {
					source: _,
					index,
					reply,
				}) => {
					self.input.ask(index, reply);
					Fed::Asked
				}
				Some(event) => Fed::Event(event),
				None => {
					self.stage = Stage::Result;
					Fed::EventsClosed
				}
			},
			result = &mut self.link.done, if take_result => {
				self.stage = Stage::Done;
				self.input.codec_done();
				Fed::Finished(result.unwrap_or_else(|_| Err(worker_died())))
			}
			else => std::future::pending().await,
		}
	}

	/// Answers the codec when its chunk is in, and fetches ahead while memory is free right now;
	/// `ops` counts the fetches in flight.
	pub(crate) fn advance(&mut self, ops: &Ops) {
		self.input.advance(ops);
	}

	pub(crate) fn fetching(&self) -> bool {
		self.input.fetching()
	}

	/// Whether the codec waits on a chunk.
	pub(crate) fn owes_codec(&self) -> bool {
		self.input.owes_codec()
	}

	pub(crate) fn archive(&self) -> &Arc<RemoteFileType<'static>> {
		self.input.archive()
	}

	pub(crate) fn events_closed(&self) -> bool {
		self.stage != Stage::Events
	}

	/// Bytes of the archive the codec has read, each counted once.
	pub(crate) fn bytes_read(&self) -> u64 {
		self.link.shared.input_bytes()
	}

	/// The hash of the whole archive, when the codec read it front to back, once.
	pub(crate) fn read_whole(&self) -> Option<Blake3Hash> {
		self.input.read_whole()
	}

	/// Drops every fetch and what was fetched, and the codec's ask: the job is stopping.
	pub(crate) fn drop_all(&mut self) {
		self.input.drop_all();
	}

	/// Gives back what only the codec needed, once it is done: chunks prefetched past its last
	/// read (a zip's or 7z's index chunks, fetched again for its entries). What follows holds
	/// nothing while it waits out a pause.
	pub(crate) fn release(&mut self) {
		self.input.release();
		self.running = None;
	}

	/// Waits out a pause holding nothing but the chunk the codec is reading (see
	/// [`ArchiveInput::wait_out_pause`]).
	pub(crate) async fn wait_out_pause<S: JobState>(
		&mut self,
		reporter: &MaybeArc<Reporter<S>>,
		control: &JobControl,
	) -> Result<(), Stopped> {
		self.input
			.wait_out_pause(&mut self.running, reporter, control)
			.await
	}

	/// One tick, every [`CALLBACK_INTERVAL`](crate::consts::CALLBACK_INTERVAL): whether the
	/// codec was given up on, having made no progress while the driver `owed` it nothing (a
	/// chunk it waits on counts as owed, and so does anything once it returned).
	pub(crate) fn give_up_if_stalled(&mut self, owed: bool) -> bool {
		let owed = owed || self.input.owes_codec() || self.stage == Stage::Done;
		let archive = self.input.archive().uuid();
		if !self.stall.give_up_if_stalled(&self.link, owed, archive) {
			return false;
		}
		self.stage = Stage::Done;
		true
	}
}

/// The parts of a job that reads an archive which [`start_reading`] borrows for its one call.
pub(crate) struct ReadingJob<'a, S: JobState> {
	pub(crate) config: &'a ArchiveConfig,
	pub(crate) control: &'a JobControl,
	pub(crate) reporter: &'a MaybeArc<Reporter<S>>,
	/// The phase the job reports once the codec starts.
	pub(crate) reading: S::Phase,
	/// What the job's report calls it.
	pub(crate) name: &'a str,
}

/// Starts a job that reads `archive` through a codec: checks its chunks, waits for a job slot
/// (holding nothing meanwhile, see [`ArchiveConfig::admit`]), enters the job's `reading` phase
/// and starts the codec. The slot, held until dropped, and the codec's feed; `Err` with the
/// phase the job ends in and why.
pub(crate) async fn start_reading<B: DriveBackend, S: JobState, T>(
	backend: Arc<B>,
	archive: Arc<RemoteFileType<'static>>,
	job: ReadingJob<'_, S>,
	start: impl FnOnce() -> Result<WorkerLink<Result<T, Error>>, Error>,
) -> Result<(OwnedSemaphorePermit, CodecFeed<B, T>), (S::Phase, Arc<Error>)> {
	let ReadingJob {
		config,
		control,
		reporter,
		reading,
		name,
	} = job;
	if let Err(error) = check_chunks_consistent(archive.chunks(), archive.size()) {
		return Err((S::Phase::FAILED, Arc::new(error)));
	}
	// leased before the codec starts, so a waiting job holds nothing
	let Ok(lease) = config.admit(control, &reporter.ops()).await else {
		reporter.wind_down(control);
		return Err((S::Phase::CANCELLED, cancelled(name)));
	};
	reporter.set_phase(reading);
	let link = start().map_err(|error| (S::Phase::FAILED, Arc::new(error)))?;
	Ok((lease, CodecFeed::new(backend, archive, link, reporter.op())))
}

#[cfg(test)]
mod tests {
	use filen_types::fs::Uuid;

	use super::*;
	use crate::{
		consts::CHUNK_SIZE,
		fs::{
			archive::{
				extract::{
					ExtractCallback, ExtractUpdate, ExtractedTopLevel,
					report::Reporter as ExtractReporter,
				},
				test_support::pattern,
			},
			drive_job::test_support::{FakeBackend, remote_file},
		},
	};

	struct Ignore;

	impl ExtractCallback for Ignore {
		fn on_top_level_batch(&self, _: Vec<ExtractedTopLevel>) {}
		fn on_update(&self, _: ExtractUpdate) {}
	}

	/// Answers the codec asking for chunk `index`, then lets `input` fetch ahead; the chunk it
	/// fetches next.
	async fn read(input: &mut ArchiveInput<FakeBackend>, ops: &Ops, index: u64) -> u64 {
		let (reply, answer) = oneshot::channel();
		input.ask(index, reply);
		while input.owes_codec() {
			input.advance(ops);
			let fetched = input
				.fetched()
				.await
				.expect("the chunk asked for is fetched");
			input.fetch_finished(fetched).unwrap();
		}
		answer.await.unwrap();
		input.advance(ops);
		input.next_fetch
	}

	#[tokio::test]
	async fn chunks_are_fetched_ahead_only_after_two_read_one_after_the_other() {
		let bytes = pattern(20 * CHUNK_SIZE, 1);
		let archive = remote_file(
			Uuid::from_u128(1),
			Uuid::from_u128(2),
			"a.zip",
			&bytes,
			None,
		);
		let mut backend = FakeBackend::new(Uuid::from_u128(3)).with_memory(8);
		backend.contents.insert(archive.uuid(), bytes);
		let mut input = ArchiveInput::new(Arc::new(backend), Arc::new(archive));
		let reporter = ExtractReporter::new(Ignore, 0);
		let ops = reporter.ops();
		// a zip's head, then its index at the end: nothing fetched ahead of either
		assert_eq!(read(&mut input, &ops, 0).await, 1);
		assert_eq!(read(&mut input, &ops, 18).await, 19);
		// two in a row: four ahead
		assert_eq!(read(&mut input, &ops, 19).await, 20);
		assert_eq!(read(&mut input, &ops, 3).await, 4);
		assert_eq!(read(&mut input, &ops, 4).await, 9);
	}
}
