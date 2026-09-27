//! The archive as a reading codec gets it, for an extraction and a listing alike: fetched chunk
//! by chunk as the codec asks, a few chunks ahead when the client's memory has room right now,
//! and hashed as it is read.

use std::{collections::VecDeque, io, sync::Arc};

use filen_types::crypto::Blake3Hash;
use futures::{StreamExt, stream::FuturesOrdered};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::{
	Error, ErrorKind,
	consts::CHUNK_SIZE_U64,
	fs::{
		archive::config::{ArchiveConfig, CHUNK_BYTES},
		drive_job::backend::DriveBackend,
		file::{enums::RemoteFileType, read::chunk_plaintext_len, traits::HasFileInfo},
	},
	job::{
		JobControl, Stopped,
		report::{JobState, OpGuard, Ops, Reporter},
	},
	util::{MaybeArc, MaybeSendBoxFuture},
};

/// Chunks of the archive fetched ahead of the codec, memory permitting.
const PREFETCH_CHUNKS: usize = 4;

/// A fetched chunk of the archive, with the memory it holds.
pub(super) type FetchedChunk = (u64, Result<Vec<u8>, Error>, OwnedSemaphorePermit, OpGuard);

/// A job's memory floor while it holds it, counted as an operation in flight: the job is only
/// reported paused once it has given it back, with everything held on top of it.
pub(super) type Floor = (OwnedSemaphorePermit, OpGuard);

/// Memory for one chunk: `slot` (one of the floor's) when free, else the client's `memory` if it
/// has room now, else nothing.
pub(super) fn take_memory(
	slot: &Arc<Semaphore>,
	memory: &Arc<Semaphore>,
) -> Option<OwnedSemaphorePermit> {
	Arc::clone(slot).try_acquire_owned().ok().or_else(|| {
		Arc::clone(memory)
			.try_acquire_many_owned(CHUNK_BYTES as u32)
			.ok()
	})
}

pub(super) struct ArchiveInput<B> {
	backend: Arc<B>,
	archive: Arc<RemoteFileType<'static>>,
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
	hasher: blake3::Hasher,
	/// The codec read the archive front to back, once: `hasher` covers it all.
	sequential: bool,
	ask: Option<(u64, oneshot::Sender<io::Result<Vec<u8>>>)>,
	/// The floor's input chunk.
	slot: Arc<Semaphore>,
	/// The client's file-IO budget, for what goes beyond the floor.
	memory: Arc<Semaphore>,
}

impl<B: DriveBackend> ArchiveInput<B> {
	pub(super) fn new(backend: Arc<B>, archive: Arc<RemoteFileType<'static>>) -> Self {
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
			hasher: blake3::Hasher::new(),
			sequential: true,
			ask: None,
			slot: Arc::new(Semaphore::new(1)),
		}
	}

	pub(super) fn archive(&self) -> &Arc<RemoteFileType<'static>> {
		&self.archive
	}

	/// The codec asks for chunk `index`, which means it is done with the one before; answered
	/// by [`Self::advance`] once the chunk is in.
	pub(super) fn ask(&mut self, index: u64, reply: oneshot::Sender<io::Result<Vec<u8>>>) {
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

	/// The codec ended: it holds no chunk any more.
	pub(super) fn codec_done(&mut self) {
		self.reading = None;
	}

	/// Whether the codec waits on a chunk.
	pub(super) fn owes_codec(&self) -> bool {
		self.ask.is_some()
	}

	pub(super) fn fetching(&self) -> bool {
		!self.fetches.is_empty()
	}

	/// The next chunk fetched, in the order they were asked for.
	pub(super) async fn fetched(&mut self) -> Option<FetchedChunk> {
		self.fetches.next().await
	}

	/// Answers the codec when its chunk is in, and fetches ahead while memory is free right
	/// now; `ops` counts the fetches in flight.
	pub(super) fn advance(&mut self, ops: &Ops) {
		self.serve_ask();
		// Nothing is fetched ahead of what the codec asks for until it has read two chunks in a
		// row: a zip or 7z reads its head, then its index at the end, and chunks fetched after
		// the head would go unread (all of them, when only the index is listed).
		let ahead = if self.served >= 2 { PREFETCH_CHUNKS } else { 1 };
		while self.fetches.len() + self.ready.len() < ahead
			&& self.next_fetch < self.chunks
			&& (ahead > 1 || self.next_fetch <= self.served)
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
	pub(super) fn fetch_finished(
		&mut self,
		(index, result, permit, _op): FetchedChunk,
	) -> Result<(), Error> {
		let expected = chunk_plaintext_len(self.archive.size(), index);
		match result {
			Ok(data) if data.len() as u64 == expected => {
				self.ready.push_back((index, data, permit));
				self.serve_ask();
				Ok(())
			}
			Ok(data) => Err(Error::custom(
				ErrorKind::Response,
				format!(
					"chunk {index} of the archive holds {} bytes instead of {expected}",
					data.len()
				),
			)),
			Err(error) => Err(error),
		}
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
		self.hasher.update_rayon(&data);
		let _ = reply.send(Ok(data));
	}

	fn drop_prefetched(&mut self) {
		self.fetches = FuturesOrdered::new();
		self.ready.clear();
		// the codec reads in order, so fetching starts again at the chunk it needs next
		self.next_fetch = self.served;
	}

	/// Drops every fetch and what was fetched, and the codec's ask: the job is stopping.
	pub(super) fn drop_all(&mut self) {
		self.drop_prefetched();
		self.ask = None;
	}

	/// Gives back everything, once the codec is done with the archive (a zip's or 7z's index
	/// chunks may have been fetched again for their entries).
	pub(super) fn release(&mut self) {
		self.drop_all();
		self.reading = None;
	}

	/// Waits out a pause holding nothing but the chunk the codec is reading: prefetched chunks
	/// and the job's `floor` are given back, and the floor taken again once the pause is over.
	pub(super) async fn wait_out_pause<S: JobState>(
		&mut self,
		floor: &mut Option<Floor>,
		reporter: &MaybeArc<Reporter<S>>,
		control: &JobControl,
		config: &ArchiveConfig,
	) -> Result<(), Stopped> {
		self.drop_prefetched();
		// The chunk the codec is reading stays with it, part of its state. Nothing else holds the
		// floor's input slot now, so it moves there if it took from the client's budget.
		if self.reading.is_some()
			&& let Ok(slot) = Arc::clone(&self.slot).try_acquire_owned()
		{
			self.reading = Some(slot);
		}
		// last, so the job counts as paused only now
		*floor = None;
		reporter.checkpoint(control).await?;
		let taken = control.until_stopping(config.floor()).await?;
		*floor = Some((taken, reporter.op()));
		Ok(())
	}

	/// The hash of the whole archive, when the codec read it front to back, once.
	pub(super) fn read_whole(&self) -> Option<Blake3Hash> {
		(self.sequential && self.served == self.chunks)
			.then(|| Blake3Hash::from(self.hasher.finalize()))
	}
}
