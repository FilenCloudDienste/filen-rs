//! The codec worker: the thread an archive's synchronous codec runs on, and what it and the async
//! driver of its job say to each other.
//!
//! The codec parks its thread on every exchange, so it never runs on a thread that drives async
//! tasks: natively it gets a thread of its own per job, on wasm it runs on the archive purpose's
//! long-lived worker (see [`WorkerSlot`](crate::blocking::WorkerSlot)). The driver owns
//! everything else: the network, memory reservations, the drive lock, names and reporting.
//!
//! No chunk waits in a channel unaccounted for. The codec asks for its input one chunk at a time
//! and the driver answers each ask into a reply of its own; its output goes through a channel of
//! one event, and the codec parks until the driver takes it.
//!
//! The two sides share nothing but atomics and channels: a codec that traps on wasm runs no
//! destructor, and would leave any lock it held locked.

use std::{
	io::{self, Read, Write},
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::Duration,
};

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, oneshot};

use crate::consts::{CHUNK_SIZE, CHUNK_SIZE_U64, FILE_CHUNK_SIZE_EXTRA_USIZE};

use super::{entry_path::ArchivePath, extract::ExtractSkipReason, format::StreamCodec};

/// How long the codec may go without taking input or handing over an event, while the driver
/// owes it nothing, before it is given up on as dead. Far above any single step a healthy codec
/// takes between two exchanges; all it has to buy is turning a hang into an error.
pub(crate) const ARCHIVE_STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// What the codec tells the driver, in archive order.
#[derive(Debug)]
pub(crate) enum WorkerEvent {
	/// The codec needs chunk `index` of the plaintext of its input `source`: the archive when
	/// extracting (always 0), a source file by its place in the job's entries when compressing.
	Ask {
		source: u32,
		index: u64,
		reply: oneshot::Sender<io::Result<Vec<u8>>>,
	},
	/// What a streaming archive turned out to hold; sent before any entry.
	Opened(StreamLayout),
	Entry(EntryHead),
	/// Output: when extracting, the next data of the file entry sent last; when compressing,
	/// the next chunk of the archive. [`CHUNK_SIZE`] bytes, except for the last chunk.
	Data(Vec<u8>),
	/// The current file entry ended. When extracting, its data was checked against whatever
	/// checksum the archive carries for it (a mismatch fails the codec instead); when
	/// compressing, it is in the archive.
	FileEnd,
	Skipped(SkippedMember),
	/// When compressing into a format whose start is written last (a 7z): the archive's first
	/// chunk, sent after all the others.
	Head(Vec<u8>),
}

/// What a streaming archive holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamLayout {
	/// A tar, bare or inside a compressed stream.
	Tar { codec: Option<StreamCodec> },
	/// A zip, read from its central directory; every entry's data is checked against its
	/// CRC-32 or authentication code.
	Zip,
	/// A 7z, read from its header; entries are checked against the CRC-32s it lists.
	SevenZ,
	/// One compressed file.
	Single { codec: StreamCodec },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EntryHead {
	/// The entry's position among the archive's members, which identifies it.
	pub(crate) ordinal: u64,
	pub(crate) path: ArchivePath,
	pub(crate) modified: Option<DateTime<Utc>>,
	pub(crate) kind: EntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
	Dir,
	/// A file, with its size when the archive states it; its data follows as
	/// [`WorkerEvent::Data`], then a [`WorkerEvent::FileEnd`].
	File {
		size: Option<u64>,
	},
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkippedMember {
	pub(crate) ordinal: u64,
	/// The path as stored, cut to fit (see [`display_path`](super::limits::display_path)).
	pub(crate) path: String,
	pub(crate) path_truncated: bool,
	/// Data the archive stores for the entry, which is not extracted.
	pub(crate) bytes: u64,
	pub(crate) reason: ExtractSkipReason,
}

/// What the codec and its driver share.
#[derive(Debug, Default)]
pub(crate) struct WorkerShared {
	/// Set when the driver is gone or gave up; the codec's next exchange fails.
	cancelled: AtomicBool,
	/// Moved whenever the codec takes input or hands over an event: the driver's evidence that
	/// the codec is alive.
	progress: AtomicU64,
	/// Bytes of the archive the codec has read.
	input_bytes: AtomicU64,
}

impl WorkerShared {
	pub(crate) fn progress(&self) -> u64 {
		self.progress.load(Ordering::Relaxed)
	}

	pub(crate) fn input_bytes(&self) -> u64 {
		self.input_bytes.load(Ordering::Relaxed)
	}

	fn note_progress(&self) {
		self.progress.fetch_add(1, Ordering::Relaxed);
	}
}

/// Marks the error of an exchange that failed because the driver went away or cancelled, which
/// the driver never needs reported back.
#[derive(Debug, thiserror::Error)]
#[error("the archive job ended")]
pub(crate) struct JobEnded;

fn ended() -> io::Error {
	io::Error::other(JobEnded)
}

/// A fetch the driver could not answer: an error of the archive's source, which every other
/// error a codec reads through is not (those are the archive's own: damaged data).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct SourceFailed(io::Error);

/// The codec's end of the exchange.
pub(crate) struct WorkerPort {
	events: mpsc::Sender<WorkerEvent>,
	shared: Arc<WorkerShared>,
}

impl WorkerPort {
	/// Hands `event` to the driver, parking until it has taken the previous one.
	pub(crate) fn send(&self, event: WorkerEvent) -> io::Result<()> {
		if self.shared.cancelled.load(Ordering::Relaxed) {
			return Err(ended());
		}
		self.events.blocking_send(event).map_err(|_| ended())?;
		self.shared.note_progress();
		Ok(())
	}

	/// Chunk `index` of `source`'s plaintext, parking until the driver has fetched it.
	pub(crate) fn fetch(&self, source: u32, index: u64) -> io::Result<Vec<u8>> {
		let (reply, answer) = oneshot::channel();
		self.send(WorkerEvent::Ask {
			source,
			index,
			reply,
		})?;
		let chunk = answer
			.blocking_recv()
			.map_err(|_| ended())?
			.map_err(|error| io::Error::new(error.kind(), SourceFailed(error)))?;
		self.shared.note_progress();
		Ok(chunk)
	}

	/// Shows the driver the codec is alive through work that exchanges nothing with it (a long
	/// key derivation); fails as an exchange would once the driver is gone.
	pub(crate) fn keep_alive(&self) -> io::Result<()> {
		if self.shared.cancelled.load(Ordering::Relaxed) {
			return Err(ended());
		}
		self.shared.note_progress();
		Ok(())
	}

	pub(crate) fn shared(&self) -> &WorkerShared {
		&self.shared
	}
}

/// The driver's end of the exchange with a started codec.
pub(crate) struct WorkerLink<T> {
	pub(crate) events: mpsc::Receiver<WorkerEvent>,
	/// The codec's result; closed without one when the codec died.
	pub(crate) done: oneshot::Receiver<T>,
	pub(crate) shared: Arc<WorkerShared>,
	/// The wasm worker generation that took the job, to retire if it stalls.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	generation: u64,
}

impl<T> WorkerLink<T> {
	/// Gives up on a codec that stopped moving. A stalled thread is never killed (it could hold
	/// the allocator's lock): natively it is left to exit at its next exchange once the link is
	/// dropped, and on wasm the worker is retired so the next job gets a fresh one.
	pub(crate) fn retire(&self) {
		self.shared.cancelled.store(true, Ordering::Relaxed);
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		ARCHIVE_CODECS.retire(self.generation);
	}
}

impl<T> Drop for WorkerLink<T> {
	fn drop(&mut self) {
		// a codec parked in an exchange wakes up as the channels close; one that is decoding
		// stops at its next exchange
		self.shared.cancelled.store(true, Ordering::Relaxed);
	}
}

/// The codec's end of a new exchange, and the driver's two receiving ends of it.
fn channels() -> (WorkerPort, mpsc::Receiver<WorkerEvent>, Arc<WorkerShared>) {
	let (events_tx, events) = mpsc::channel(1);
	let shared = Arc::new(WorkerShared::default());
	let port = WorkerPort {
		events: events_tx,
		shared: Arc::clone(&shared),
	};
	(port, events, shared)
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
static ARCHIVE_CODECS: crate::blocking::WorkerSlot = crate::blocking::WorkerSlot::new();

/// Runs `job` on a codec worker: a thread of its own natively, the archive worker on wasm. The
/// caller holds the archive job lease, so on wasm no other job is queued on the worker.
pub(crate) fn start<T: Send + 'static>(
	job: impl FnOnce(WorkerPort) -> T + Send + 'static,
) -> Result<WorkerLink<T>, crate::Error> {
	let (port, events, shared) = channels();
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	{
		let (result, done) = oneshot::channel();
		std::thread::Builder::new()
			.name("filen-archive-codec".to_owned())
			.spawn(move || {
				let _ = result.send(job(port));
			})?;
		Ok(WorkerLink {
			events,
			done,
			shared,
		})
	}
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	{
		let (generation, done) = ARCHIVE_CODECS.submit(move || job(port));
		Ok(WorkerLink {
			events,
			done,
			shared,
			generation,
		})
	}
}

/// A link driven by a test instead of a codec: events are sent with the async `send`, and the
/// result through the returned sender.
#[cfg(test)]
pub(crate) fn scripted<T>() -> (mpsc::Sender<WorkerEvent>, oneshot::Sender<T>, WorkerLink<T>) {
	let (port, events, shared) = channels();
	let (result, done) = oneshot::channel();
	let link = WorkerLink {
		events,
		done,
		shared,
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		generation: 0,
	};
	(port.events, result, link)
}

/// An input's plaintext, fetched through the driver one chunk at a time.
pub(crate) struct ChunkInput<'p> {
	port: &'p WorkerPort,
	source: u32,
	len: u64,
	next: u64,
	chunk: Vec<u8>,
	pos: usize,
}

impl<'p> ChunkInput<'p> {
	pub(crate) fn new(port: &'p WorkerPort, source: u32, len: u64) -> Self {
		Self {
			port,
			source,
			len,
			next: 0,
			chunk: Vec::new(),
			pos: 0,
		}
	}
}

impl Read for ChunkInput<'_> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		if self.pos == self.chunk.len() {
			if self.next * CHUNK_SIZE_U64 >= self.len {
				return Ok(0);
			}
			self.chunk = self.port.fetch(self.source, self.next)?;
			self.next += 1;
			self.pos = 0;
			if self.chunk.is_empty() {
				// the driver checks every chunk's length against the file's
				return Err(io::ErrorKind::UnexpectedEof.into());
			}
		}
		let n = buf.len().min(self.chunk.len() - self.pos);
		buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
		self.pos += n;
		self.port
			.shared
			.input_bytes
			.fetch_add(n as u64, Ordering::Relaxed);
		Ok(n)
	}
}

/// An input's plaintext with random access, for formats read from their end (zip): chunks are
/// fetched through the driver as they are needed, and the last two stay cached, so a record that
/// straddles a chunk boundary, or a header read before its data, costs no second fetch.
pub(crate) struct SeekInput<'p> {
	port: &'p WorkerPort,
	source: u32,
	len: u64,
	pos: u64,
	/// The most recent chunks, most recent first.
	cache: Vec<(u64, Vec<u8>)>,
	slots: usize,
}

impl<'p> SeekInput<'p> {
	pub(crate) fn new(port: &'p WorkerPort, source: u32, len: u64) -> Self {
		Self {
			port,
			source,
			len,
			pos: 0,
			cache: Vec::new(),
			slots: 2,
		}
	}

	/// Keeps up to `slots` chunks (at least 2: a read across a chunk boundary needs both), for
	/// readers that take turns at several places of the source.
	pub(crate) fn set_slots(&mut self, slots: usize) {
		self.slots = slots.max(2);
		self.cache.truncate(self.slots);
	}

	fn chunk(&mut self, index: u64) -> io::Result<&[u8]> {
		match self.cache.iter().position(|(cached, _)| *cached == index) {
			Some(at) => self.cache[..=at].rotate_right(1),
			None => {
				let data = self.port.fetch(self.source, index)?;
				self.port
					.shared
					.input_bytes
					.fetch_add(data.len() as u64, Ordering::Relaxed);
				self.cache.truncate(self.slots - 1);
				self.cache.insert(0, (index, data));
			}
		}
		Ok(&self.cache[0].1)
	}
}

impl Read for SeekInput<'_> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() || self.pos >= self.len {
			return Ok(0);
		}
		let index = self.pos / CHUNK_SIZE_U64;
		let within = (self.pos % CHUNK_SIZE_U64) as usize;
		let chunk = self.chunk(index)?;
		if within >= chunk.len() {
			return Err(io::ErrorKind::UnexpectedEof.into());
		}
		let n = buf.len().min(chunk.len() - within);
		buf[..n].copy_from_slice(&chunk[within..within + n]);
		self.pos += n as u64;
		Ok(n)
	}
}

impl std::io::Seek for SeekInput<'_> {
	fn seek(&mut self, to: std::io::SeekFrom) -> io::Result<u64> {
		let target = match to {
			std::io::SeekFrom::Start(offset) => Some(offset),
			std::io::SeekFrom::End(delta) => self.len.checked_add_signed(delta),
			std::io::SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
		};
		self.pos = target.ok_or_else(|| {
			io::Error::new(io::ErrorKind::InvalidInput, "a seek before the start")
		})?;
		Ok(self.pos)
	}
}

/// What the codec writes, handed to the driver in whole chunks. Flushing does nothing: only
/// [`ChunkSink::finish`] sends a last, short chunk.
///
/// An error sticks: once a chunk could not be handed over, every later write fails too, and so
/// does [`ChunkSink::finish`], whatever a writer in front of the sink did with the first error
/// (a writer that finalizes its format when dropped, or one that swallows errors on finishing,
/// can never make a failed archive look complete).
pub(crate) struct ChunkSink<'p> {
	port: &'p WorkerPort,
	chunk: Vec<u8>,
	written: u64,
	failed: bool,
	/// Whether the first chunk is held back, to be patched and sent last.
	holds_head: bool,
	head: Option<Vec<u8>>,
}

impl<'p> ChunkSink<'p> {
	pub(crate) fn new(port: &'p WorkerPort) -> Self {
		Self {
			port,
			chunk: new_chunk(),
			written: 0,
			failed: false,
			holds_head: false,
			head: None,
		}
	}

	/// A sink that keeps the first chunk until [`ChunkSink::finish_with_head`].
	pub(crate) fn holding_head(port: &'p WorkerPort) -> Self {
		Self {
			holds_head: true,
			..Self::new(port)
		}
	}

	/// Sends what is left; the bytes written in all.
	pub(crate) fn finish(mut self) -> io::Result<u64> {
		debug_assert!(!self.holds_head, "a held head is sent by finish_with_head");
		if self.failed {
			return Err(ended());
		}
		if !self.chunk.is_empty() {
			let last = std::mem::take(&mut self.chunk);
			self.port.send(WorkerEvent::Data(last))?;
		}
		Ok(self.written)
	}

	/// Sends the last chunk, then the held first one with `start` written over its first
	/// bytes (which the archive already holds, zeroed); the bytes written in all.
	pub(crate) fn finish_with_head(mut self, start: &[u8]) -> io::Result<u64> {
		debug_assert!(self.holds_head);
		if self.failed {
			return Err(ended());
		}
		let last = std::mem::take(&mut self.chunk);
		let (mut head, last) = match self.head.take() {
			Some(head) => (head, Some(last)),
			None => (last, None),
		};
		if head.len() < start.len() {
			return Err(io::Error::other(
				"the archive is shorter than its start header",
			));
		}
		head[..start.len()].copy_from_slice(start);
		if let Some(last) = last.filter(|last| !last.is_empty()) {
			self.port.send(WorkerEvent::Data(last))?;
		}
		self.port.send(WorkerEvent::Head(head))?;
		Ok(self.written)
	}
}

impl Write for ChunkSink<'_> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		if self.failed {
			return Err(ended());
		}
		let n = buf.len().min(CHUNK_SIZE - self.chunk.len());
		self.chunk.extend_from_slice(&buf[..n]);
		self.written += n as u64;
		if self.chunk.len() == CHUNK_SIZE {
			let full = std::mem::replace(&mut self.chunk, new_chunk());
			if self.holds_head && self.head.is_none() {
				self.head = Some(full);
				return Ok(n);
			}
			if let Err(e) = self.port.send(WorkerEvent::Data(full)) {
				self.failed = true;
				return Err(e);
			}
		}
		Ok(n)
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

/// An empty chunk with room for the chunk's encryption overhead, so encrypting it in place never
/// reallocates.
fn new_chunk() -> Vec<u8> {
	Vec::with_capacity(CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE)
}

/// Hands the driver everything `reader` yields as the current file's data, in whole chunks;
/// returns how many bytes that was.
pub(crate) fn send_file_data(port: &WorkerPort, reader: &mut dyn Read) -> io::Result<u64> {
	let mut total = 0;
	loop {
		let mut chunk = new_chunk();
		chunk.resize(CHUNK_SIZE, 0);
		let filled = read_full(reader, &mut chunk)?;
		chunk.truncate(filled);
		total += filled as u64;
		if filled > 0 {
			port.send(WorkerEvent::Data(chunk))?;
		}
		if filled < CHUNK_SIZE {
			return Ok(total);
		}
	}
}

/// Reads until `buf` is full or the stream ends; the number of bytes read.
pub(crate) fn read_full(reader: &mut (impl Read + ?Sized), buf: &mut [u8]) -> io::Result<usize> {
	let mut filled = 0;
	while filled < buf.len() {
		match reader.read(&mut buf[filled..]) {
			Ok(0) => break,
			Ok(n) => filled += n,
			Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
			Err(e) => return Err(e),
		}
	}
	Ok(filled)
}
