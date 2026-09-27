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
	io::{self, Read},
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
};

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, oneshot};

use crate::consts::{CHUNK_SIZE, CHUNK_SIZE_U64, FILE_CHUNK_SIZE_EXTRA_USIZE};

use super::{entry_path::ArchivePath, extract::ExtractSkipReason, format::StreamCodec};

/// What the codec tells the driver, in archive order.
#[derive(Debug)]
pub(crate) enum WorkerEvent {
	/// The codec needs chunk `index` of the archive's plaintext.
	Ask {
		index: u64,
		reply: oneshot::Sender<io::Result<Vec<u8>>>,
	},
	/// What a streaming archive turned out to hold; sent before any entry.
	Opened(StreamLayout),
	Entry(EntryHead),
	/// The next data of the file entry sent last: [`CHUNK_SIZE`] bytes, except for a file's last
	/// chunk.
	Data(Vec<u8>),
	/// The end of the file entry sent last. Its data was checked against whatever checksum the
	/// archive carries for it: a mismatch fails the codec instead.
	FileEnd,
	Skipped(SkippedMember),
}

/// What a streaming archive holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamLayout {
	/// A tar, bare or inside a compressed stream.
	Tar { codec: Option<StreamCodec> },
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

	/// Chunk `index` of the archive's plaintext, parking until the driver has fetched it.
	pub(crate) fn fetch(&self, index: u64) -> io::Result<Vec<u8>> {
		let (reply, answer) = oneshot::channel();
		self.send(WorkerEvent::Ask { index, reply })?;
		let chunk = answer.blocking_recv().map_err(|_| ended())??;
		self.shared.note_progress();
		Ok(chunk)
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

/// The archive's plaintext, fetched through the driver one chunk at a time.
pub(crate) struct ChunkInput<'p> {
	port: &'p WorkerPort,
	len: u64,
	next: u64,
	chunk: Vec<u8>,
	pos: usize,
}

impl<'p> ChunkInput<'p> {
	pub(crate) fn new(port: &'p WorkerPort, len: u64) -> Self {
		Self {
			port,
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
			self.chunk = self.port.fetch(self.next)?;
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

/// Hands the driver everything `reader` yields as the current file's data, in whole chunks;
/// returns how many bytes that was.
pub(crate) fn send_file_data(port: &WorkerPort, reader: &mut dyn Read) -> io::Result<u64> {
	let mut total = 0;
	loop {
		// room for the chunk's encryption overhead, so encrypting it in place never reallocates
		let mut chunk = Vec::with_capacity(CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE);
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
