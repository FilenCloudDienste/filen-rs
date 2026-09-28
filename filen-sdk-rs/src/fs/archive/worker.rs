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
use filen_types::fs::Uuid;
use tokio::sync::{mpsc, oneshot};

use crate::{
	Error, ErrorKind,
	blocking::send_catching_panic,
	consts::{CALLBACK_INTERVAL, CHUNK_SIZE, CHUNK_SIZE_U64, FILE_CHUNK_SIZE_EXTRA_USIZE},
};

use super::{
	entry_path::ArchivePath,
	extract::{ArchiveEntry, ExtractSkipReason},
	format::ArchiveFormat,
};

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
	/// What the archive turned out to be; sent before any entry.
	Opened(ArchiveFormat),
	Entry(EntryHead),
	/// Output: when extracting, the next data of the file entry sent last; when compressing,
	/// the next chunk of the archive. [`CHUNK_SIZE`] bytes, except for the last chunk.
	Data(Vec<u8>),
	/// The current file entry ended. When extracting, its data was checked against whatever
	/// checksum the archive carries for it (a mismatch fails the codec instead); when
	/// compressing, it is in the archive.
	FileEnd,
	Skipped(SkippedMember),
	/// When extracting a tar: a hard link to a file sent before, to extract as a copy of it.
	Link(Box<LinkHead>),
	/// When listing: what an entry is, and what extracting it would do.
	Listed(Box<ArchiveEntry>),
	/// When compressing into a format whose start is written last (a 7z): the archive's first
	/// chunk, sent after all the others.
	Head(Vec<u8>),
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

/// A tar hard link: a second name for a file the archive stored before it, which the driver
/// extracts as a copy of the file it created for that one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkHead {
	pub(crate) ordinal: u64,
	pub(crate) path: ArchivePath,
	pub(crate) modified: Option<DateTime<Utc>>,
	/// The path of the file it names, as sent with that file's entry.
	pub(crate) target: ArchivePath,
	/// What it is reported as when no file was created for its target (it was skipped, failed,
	/// or never came): skipped, as a hard link.
	pub(crate) unresolved: SkippedMember,
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
	/// Bytes of its input the codec has read, each counted once however often it is read (a
	/// chunk evicted from a [`SeekInput`]'s cache and fetched again, an entry read for a password
	/// probe and then again for real), so it never exceeds the input's length.
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

	/// What a real codec's port notes on each exchange, for a [`scripted`] one to note it too.
	#[cfg(test)]
	pub(crate) fn note_scripted_progress(&self) {
		self.note_progress();
	}
}

/// The error a job ends with when its codec stopped responding.
pub(crate) fn worker_died() -> Error {
	Error::custom(
		ErrorKind::ArchiveWorkerDied,
		"the archive's codec stopped responding",
	)
}

/// Tells a codec that stopped moving from one waiting on its driver, over the driver's ticks.
#[derive(Debug, Default)]
pub(crate) struct StallWatch {
	/// The codec's progress stamp last seen, and for how many ticks it has not moved.
	stamp: u64,
	still_ticks: u32,
}

impl StallWatch {
	/// One tick, every [`CALLBACK_INTERVAL`]; whether the codec has made no progress for
	/// [`ARCHIVE_STALL_TIMEOUT`] while its driver `owed` it nothing (an answer, room for an
	/// event, the end of a pause: all of which leave it rightly still).
	pub(crate) fn stalled(&mut self, shared: &WorkerShared, owed: bool) -> bool {
		let stamp = shared.progress();
		if owed || stamp != self.stamp {
			self.stamp = stamp;
			self.still_ticks = 0;
			return false;
		}
		self.still_ticks += 1;
		CALLBACK_INTERVAL * self.still_ticks >= ARCHIVE_STALL_TIMEOUT
	}

	/// One tick, as [`Self::stalled`]; a codec of `archive` that stalled is given up on: logged,
	/// and its `link` retired. Whether it was.
	pub(crate) fn give_up_if_stalled<T>(
		&mut self,
		link: &WorkerLink<T>,
		owed: bool,
		archive: Uuid,
	) -> bool {
		if !self.stalled(&link.shared, owed) {
			return false;
		}
		tracing::error!(
			"archive {archive}: the codec made no progress for {ARCHIVE_STALL_TIMEOUT:?}"
		);
		link.retire();
		true
	}
}

/// Logs how the codec of `archive` failed with `error`; whether that error is the job's, which
/// it is not once the job had `ended` (the driver failed a fetch, or stopped): the codec then
/// only failed for it.
pub(crate) fn codec_failed(archive: Uuid, error: &Error, ended: bool) -> bool {
	if ended {
		tracing::debug!("archive codec ended after the job did: {error}");
		return false;
	}
	// a dead codec is a bug to hear of, where a damaged archive is only the user's
	if error.kind() == ErrorKind::ArchiveWorkerDied {
		tracing::error!("archive {archive}: {error}");
	} else {
		tracing::warn!("archive {archive}: {error}");
	}
	true
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

/// The codec's events. They end when the codec does: when it returned, or when it panicked, which
/// on wasm traps its thread without closing its end of the channel.
pub(crate) struct WorkerEvents {
	events: mpsc::Receiver<WorkerEvent>,
	/// Answered when the codec panicked; closed unanswered once it can no longer panic.
	panicked: Option<oneshot::Receiver<()>>,
}

impl WorkerEvents {
	fn new(events: mpsc::Receiver<WorkerEvent>, panicked: Option<oneshot::Receiver<()>>) -> Self {
		Self { events, panicked }
	}

	pub(crate) async fn recv(&mut self) -> Option<WorkerEvent> {
		if let Some(panicked) = &mut self.panicked {
			tokio::select! {
				biased;
				event = self.events.recv() => return event,
				answer = panicked => {
					self.panicked = None;
					if answer.is_ok() {
						// an event already sent is still taken; then the channel reads as ended
						self.events.close();
					}
				}
			}
		}
		self.events.recv().await
	}

	/// Natively a panic unwinds through the codec's end of the channel, closing it.
	#[cfg(test)]
	pub(crate) fn blocking_recv(&mut self) -> Option<WorkerEvent> {
		self.events.blocking_recv()
	}
}

/// The driver's end of the exchange with a started codec.
pub(crate) struct WorkerLink<T> {
	pub(crate) events: WorkerEvents,
	/// The codec's result. A codec that dies without a panic to report (on wasm, a trap outside
	/// Rust) leaks its sender, so this stays pending: the driver's stall deadline ends the wait.
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
///
/// A codec that panics (a parser bug an archive ran into) ends its events and fails with the
/// panic's message: natively the panic would otherwise reach only stderr, lost on mobile, and on
/// wasm the driver would wait out [`ARCHIVE_STALL_TIMEOUT`] first.
pub(crate) fn start<R: Send + 'static>(
	job: impl FnOnce(WorkerPort) -> Result<R, Error> + Send + 'static,
) -> Result<WorkerLink<Result<R, Error>>, Error> {
	let (port, events, shared) = channels();
	let (result, done) = oneshot::channel();
	let (panicked_tx, panicked) = oneshot::channel();
	// runs inside the panic hook on wasm, so it logs nothing (the panic may have struck while
	// the log's lock was held); the driver logs the error it fails the job with
	let on_panic = move |message: String| {
		let _ = panicked_tx.send(());
		Err(Error::custom(
			ErrorKind::ArchiveWorkerDied,
			format!("the archive's codec panicked: {message}"),
		))
	};
	let run = move || send_catching_panic(move || job(port), result, on_panic);
	let events = WorkerEvents::new(events, Some(panicked));
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	{
		std::thread::Builder::new()
			.name("filen-archive-codec".to_owned())
			.spawn(run)?;
		Ok(WorkerLink {
			events,
			done,
			shared,
		})
	}
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	{
		// the result goes through the channel above, which a panicking codec still answers
		let (generation, _) = ARCHIVE_CODECS.submit(run);
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
		events: WorkerEvents::new(events, None),
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
	/// Bytes read and counted: the source's first ones, as it is read in order.
	read: u64,
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
			read: 0,
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
		self.read += n as u64;
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
	/// One bit per chunk, set once it was fetched, so one fetched again counts as read once: 16
	/// KiB for a 128 GiB archive. Grown only as far as a fetch that succeeded reaches, so a
	/// length the archive does not have never sizes it.
	fetched: Vec<u64>,
	/// Bytes at the source's start already counted by the reader before this one.
	counted_prefix: u64,
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
			fetched: Vec::new(),
			counted_prefix: 0,
		}
	}

	/// Reads `input`'s source again from its start, as a zip or 7z told by the head a stream
	/// reader read. The bytes `input` counted are not counted again when their chunks are
	/// fetched here, which would take the bytes read past the source's length.
	pub(crate) fn rereading(input: ChunkInput<'p>) -> Self {
		Self {
			counted_prefix: input.read,
			..Self::new(input.port, input.source, input.len)
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
				if self.first_fetch(index) {
					let len = data.len() as u64;
					let counted = self
						.counted_prefix
						.saturating_sub(index * CHUNK_SIZE_U64)
						.min(len);
					self.port
						.shared
						.input_bytes
						.fetch_add(len - counted, Ordering::Relaxed);
				}
				self.cache.truncate(self.slots - 1);
				self.cache.insert(0, (index, data));
			}
		}
		Ok(&self.cache[0].1)
	}

	/// Marks chunk `index` fetched; whether it was not yet.
	fn first_fetch(&mut self, index: u64) -> bool {
		let word = usize::try_from(index / u64::BITS as u64)
			.expect("an archive has fewer chunks than 64 times the address space");
		let bit = 1 << (index % u64::BITS as u64);
		if word >= self.fetched.len() {
			self.fetched.resize(word + 1, 0);
		}
		let first = self.fetched[word] & bit == 0;
		self.fetched[word] |= bit;
		first
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

#[cfg(test)]
mod tests {
	use std::io::{Seek, SeekFrom};

	use super::*;
	use crate::fs::archive::test_support::pattern;

	/// Runs `read` on a codec worker over a source of `len` bytes, answering every ask; the chunk
	/// indices asked for, and what `read` returned.
	async fn with_source<R: Send + 'static>(
		len: u64,
		read: impl FnOnce(&WorkerPort) -> Result<R, Error> + Send + 'static,
	) -> (Vec<u64>, Result<R, Error>) {
		let source = pattern(usize::try_from(len).unwrap(), 0);
		let mut link = start(move |port| read(&port)).unwrap();
		let mut asked = Vec::new();
		while let Some(event) = link.events.recv().await {
			let WorkerEvent::Ask { index, reply, .. } = event else {
				panic!("the reader only asks, sent {event:?}");
			};
			asked.push(index);
			let chunk = source.chunks(CHUNK_SIZE).nth(index as usize).unwrap();
			let _ = reply.send(Ok(chunk.to_vec()));
		}
		(asked, (&mut link.done).await.unwrap())
	}

	#[tokio::test]
	async fn a_chunk_fetched_again_counts_once_toward_the_bytes_read() {
		let len = 2 * CHUNK_SIZE_U64 + 100;
		let (asked, read) = with_source(len, move |port| {
			let mut source = SeekInput::new(port, 0, len);
			let mut byte = [0u8];
			// the first two chunks evict the last one from the cache of two before it is read again
			for at in [len - 1, 0, CHUNK_SIZE_U64, len - 1] {
				source.seek(SeekFrom::Start(at))?;
				source.read_exact(&mut byte)?;
			}
			Ok(port.shared().input_bytes())
		})
		.await;
		assert_eq!(asked, [2, 0, 1, 2]);
		assert_eq!(read.unwrap(), len);
	}

	#[tokio::test]
	async fn a_source_read_again_from_its_start_counts_once_toward_the_bytes_read() {
		let len = 2 * CHUNK_SIZE_U64 + 100;
		let (asked, read) = with_source(len, move |port| {
			// into the second chunk, then read again from the start and to the end
			let mut input = ChunkInput::new(port, 0, len);
			input.read_exact(&mut vec![0u8; CHUNK_SIZE + 7])?;
			let mut source = SeekInput::rereading(input);
			source.read_to_end(&mut Vec::new())?;
			Ok(port.shared().input_bytes())
		})
		.await;
		assert_eq!(asked, [0, 1, 0, 1, 2]);
		assert_eq!(read.unwrap(), len);
	}

	#[tokio::test]
	async fn a_codec_that_panics_ends_its_events_and_fails_with_the_message() {
		let mut link = start(|port| -> Result<(), Error> {
			port.send(WorkerEvent::Opened(ArchiveFormat::Zip))?;
			panic!("a header of {} bytes", 7);
		})
		.unwrap();
		let mut events = Vec::new();
		while let Some(event) = link.events.recv().await {
			events.push(event);
		}
		assert!(
			matches!(events[..], [WorkerEvent::Opened(ArchiveFormat::Zip)]),
			"{events:?}"
		);
		let error = (&mut link.done).await.unwrap().unwrap_err();
		assert_eq!(error.kind(), ErrorKind::ArchiveWorkerDied);
		assert!(
			error.to_string().contains("panicked: a header of 7 bytes"),
			"{error}"
		);
	}

	#[tokio::test]
	async fn a_panic_ends_the_events_even_while_the_channel_stays_open() {
		// what a trapped wasm worker leaves behind: its end of the channel never drops
		let (sender, events) = mpsc::channel(1);
		let (panicked_tx, panicked) = oneshot::channel();
		let mut events = WorkerEvents::new(events, Some(panicked));
		sender.send(WorkerEvent::FileEnd).await.unwrap();
		panicked_tx.send(()).unwrap();
		assert!(matches!(events.recv().await, Some(WorkerEvent::FileEnd)));
		let end = tokio::time::timeout(Duration::from_secs(10), events.recv()).await;
		assert!(matches!(end, Ok(None)), "the events did not end");
		drop(sender);
	}
}
