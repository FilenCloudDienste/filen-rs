#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use std::{
	io,
	pin::Pin,
	task::{Context, Poll, ready},
};

use futures::{AsyncRead, AsyncReadExt};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use futures::{StreamExt, future::Either};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use tokio::sync::mpsc;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_bindgen::{JsCast, JsValue};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_streams::readable::IntoStream;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use web_sys::js_sys::Uint8Array;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use self::cross_thread::{ToCommander, feed_channel};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use crate::{
	Error, ErrorKind,
	js::js_error_message,
	runtime::{DropOnCommander, port_from_worker, spawn_local, spawn_on_commander},
};

/// The most a pump reads from its stream at a time.
const MAX_READ_LEN: usize = 64 * 1024;

/// The chunks [`spawn_stream_pump`] reads, in a channel whose closing only ever means the end of
/// the stream: a read error arrives in-band as the last item.
///
/// Only the commander may receive from the channel or drop it, both of which lock a mutex the
/// commander also locks as it sends. A consumer takes the receiver out on the commander with
/// [`DropOnCommander::into_inner`] before it builds its future; dropped anywhere else, the
/// receiver is dropped on the commander.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) type PumpedChunks = DropOnCommander<mpsc::Receiver<Result<Vec<u8>, Error>>>;

/// Pumps a JS `ReadableStream` into a channel: a task on the CALLING thread, which is where the
/// stream's JS object lives (a `ReadableStream` cannot be moved to the commander), reads it, and
/// the commander puts what it reads into the channel.
///
/// Reads at most 64 KiB at a time, and no more than `size_hint` when the caller knows how much
/// there is, and reads the next only once the last is in the channel. `progress` gets the bytes
/// handed over so far after each read.
///
/// A consumer that stops reading drops the receiver; the pump notices when its next read is done,
/// and ends, cancelling the stream.
///
/// Each read crosses to the commander over a `MessageChannel`, its buffer transferred, and the
/// commander answers once it is in the channel. This thread, the page's main thread as a rule, then
/// only takes messages, never a lock: sending into the channel locks a mutex its receiver locks on
/// every receive, and the page may not wait for one the commander holds (Chromium throws
/// "Atomics.wait cannot be called in this context", WebKit traps). The commander posts the
/// channel's port to its parent, so this is called on the thread that spawned the commander: the
/// one JS calls the SDK on.
///
/// Fails with `Conversion` only when the stream cannot be read at all, because it is already
/// locked to a reader.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) fn spawn_stream_pump(
	stream: web_sys::ReadableStream,
	size_hint: Option<u64>,
	progress: Option<impl Fn(u64) + 'static>,
) -> Result<PumpedChunks, Error> {
	let reader = stream_reader(stream)?;
	// Making the channel locks nothing; from here on only the commander touches it, but for an
	// `UnfedSender` dropped where the commander was given up on.
	let (sender, receiver) = mpsc::channel(10);
	let sender = UnfedSender(Some(sender));
	let request = port_from_worker(move |port| match port {
		Some(port) => spawn_local(pump_stream(
			reader,
			size_hint,
			progress,
			ToCommander::new(port),
		)),
		// The commander was given up on before it opened the channel: nothing takes the reads any
		// more, and the stream is cancelled as for a consumer that is gone.
		None => drop(reader),
	});
	spawn_on_commander(move || feed_channel(request, sender.into_inner()));
	Ok(DropOnCommander::new(receiver))
}

/// The pump's sender until the commander starts feeding it. Dropped before that, when the commander
/// it was sent to is given up on, it sends an error first: the consumer's call may still run, on the
/// commander spawned next, and would otherwise take the closed channel for an empty stream.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
struct UnfedSender(
	// `None` only once taken
	Option<mpsc::Sender<Result<Vec<u8>, Error>>>,
);

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl UnfedSender {
	fn into_inner(mut self) -> mpsc::Sender<Result<Vec<u8>, Error>> {
		self.0
			.take()
			.expect("only Drop and this take the sender (should be impossible)")
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl Drop for UnfedSender {
	fn drop(&mut self) {
		if let Some(sender) = self.0.take() {
			// On the page, as a rule. Nothing was sent yet, so there is room, and a `try_send` with
			// room takes no lock: it takes a permit with a compare-and-swap. It fails only once the
			// receiver is gone too, when nobody is left to tell.
			let _ = sender.try_send(Err(Error::custom(
				ErrorKind::Internal,
				"the commander worker was given up on before it took the stream",
			)));
		}
	}
}

/// Reads a byte stream (`Blob.stream()`, a `fetch` body) through a BYOB reader, straight into the
/// pump's buffer, and any other `ReadableStream` (one built by hand, the output of `pipeThrough`
/// or a `TransformStream`) through its default reader, a buffer at a time out of each chunk. Both
/// readers cancel the stream when dropped.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn stream_reader(
	stream: web_sys::ReadableStream,
) -> Result<impl AsyncRead + Unpin + 'static, Error> {
	match wasm_streams::ReadableStream::from_raw(stream).try_into_async_read() {
		Ok(bytes) => Ok(Either::Left(bytes)),
		// Not a byte stream, or locked: `getReader({ mode: "byob" })` threw without locking it,
		// and the default reader below fails in turn if it is locked.
		Err((_, stream)) => {
			let chunks = stream.try_into_stream().map_err(|(e, _)| {
				// as text, since a `JsValue` is not `Send`; the thumbnail calls' existing message
				Error::custom(
					ErrorKind::Conversion,
					format!("got error when converting to ReadableStream: {e:?}"),
				)
			})?;
			Ok(Either::Right(ChunkReader {
				chunks,
				current: None,
			}))
		}
	}
}

/// A default reader's chunks as an `AsyncRead`. A read copies at most the caller's buffer out of
/// the current chunk, so however large a chunk the source enqueues, wasm memory holds no more of
/// it at a time than the pump's buffer, as with the BYOB reader.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
struct ChunkReader {
	chunks: IntoStream<'static>,
	/// The chunk being read and how many of its bytes have been, until it is used up.
	current: Option<(Uint8Array, u32)>,
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl AsyncRead for ChunkReader {
	fn poll_read(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &mut [u8],
	) -> Poll<io::Result<usize>> {
		let this = self.get_mut();
		if buf.is_empty() {
			return Poll::Ready(Ok(0));
		}
		loop {
			if let Some((chunk, read)) = &mut this.current {
				let rest = chunk.length() - *read;
				if rest > 0 {
					let len = usize::try_from(rest).map_or(buf.len(), |rest| rest.min(buf.len()));
					let end = *read
						+ u32::try_from(len).expect(
							"len is at most the u32 rest of the chunk (should be impossible)",
						);
					chunk.subarray(*read, end).copy_to(&mut buf[..len]);
					*read = end;
					return Poll::Ready(Ok(len));
				}
				this.current = None;
			}
			// an empty chunk is skipped: only the end of the stream reads as 0
			match ready!(this.chunks.poll_next_unpin(cx)) {
				Some(Ok(chunk)) => this.current = Some((uint8_chunk(chunk)?, 0)),
				Some(Err(e)) => return Poll::Ready(Err(io::Error::other(js_error_message(&e)))),
				None => return Poll::Ready(Ok(0)),
			}
		}
	}
}

/// A default reader's chunk as the `Uint8Array` the parameter's TypeScript type promises; any
/// other chunk fails the read instead of being converted.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn uint8_chunk(chunk: JsValue) -> io::Result<Uint8Array> {
	chunk.dyn_into::<Uint8Array>().map_err(|chunk| {
		// the chunk's type only: its contents are the caller's data
		let found = chunk.js_typeof().as_string().unwrap_or_default();
		io::Error::new(
			io::ErrorKind::InvalidData,
			format!("a stream chunk is not a Uint8Array (typeof {found})"),
		)
	})
}

/// Where [`pump_stream`] hands what it reads.
pub(crate) trait PumpSink: Sized {
	/// Hands over the next read, once there is room for it. `false` once the consumer is gone,
	/// which ends the pump.
	async fn hand_over(&mut self, chunk: &[u8]) -> bool;

	/// The stream ended: nothing more comes.
	fn end(self);

	/// A read failed, as `message` says: the last thing handed over, once there is room for it.
	async fn fail(self, message: String);
}

/// The body of [`spawn_stream_pump`]'s task.
pub(crate) async fn pump_stream(
	mut reader: impl AsyncRead + Unpin,
	size_hint: Option<u64>,
	progress: Option<impl Fn(u64)>,
	mut sink: impl PumpSink,
) {
	// A zero hint (an empty file) must not size the buffer: a read into an empty buffer returns 0,
	// which reads as the end of the stream, and a BYOB reader refuses an empty view outright.
	let read_len = size_hint
		.filter(|&size| size > 0)
		.and_then(|size| usize::try_from(size).ok())
		.map_or(MAX_READ_LEN, |size| size.min(MAX_READ_LEN));
	let mut buffer = vec![0u8; read_len];
	let mut written = 0u64;
	loop {
		match reader.read(&mut buffer).await {
			// EOF
			Ok(0) => {
				sink.end();
				return;
			}
			Ok(n) => {
				// the consumer has ended, with a result of its own
				if !sink.hand_over(&buffer[..n]).await {
					return;
				}
				if let Some(progress) = &progress {
					written += n as u64;
					progress(written);
				}
			}
			Err(e) => {
				sink.fail(format!("error reading from stream: {e:?}")).await;
				return;
			}
		}
	}
}

/// The stream's thread's half of a pump and the commander's, which talk over a `MessageChannel`:
/// the stream's thread posts each read and waits for the commander's answer before it reads the
/// next, and posts how the stream ended. The commander answers a read, or a failed one, once it is
/// in the channel or the consumer is gone.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod cross_thread {
	use serde::{Deserialize, Serialize};
	use tokio::sync::mpsc;
	use wasm_bindgen::{JsCast, JsValue};
	use web_sys::{
		MessagePort,
		js_sys::{Array, Uint8Array},
	};

	use super::PumpSink;
	use crate::{
		Error, ErrorKind,
		js::port_channel::{self, PortMessages, broken},
		runtime::PortRequest,
	};

	/// The commander's answer to a read once it is in the channel; anything else means that the
	/// consumer is gone.
	const TAKEN: &str = "taken";
	/// The commander's answer to a read once the consumer is gone.
	const GONE: &str = "gone";

	/// What the stream's thread posts besides a read, which it posts as its `Uint8Array`.
	#[derive(Serialize, Deserialize)]
	enum Ending {
		/// The stream ended. The commander does not answer this.
		End,
		/// A read failed, with the message the consumer gets.
		Failed(String),
	}

	/// The stream's thread's end of the channel to the commander.
	pub(super) struct ToCommander {
		answers: PortMessages,
	}

	impl ToCommander {
		pub(super) fn new(port: MessagePort) -> Self {
			Self {
				answers: PortMessages::new(port),
			}
		}

		fn post_ending(&self, ending: &Ending) -> Result<(), JsValue> {
			let ending = serde_wasm_bindgen::to_value(ending)?;
			self.answers.port().post_message(&ending)
		}

		/// Whether the commander answered that what was posted last is in the channel.
		async fn taken(&mut self) -> bool {
			self.answers.next().await.as_string().as_deref() == Some(TAKEN)
		}
	}

	impl PumpSink for ToCommander {
		async fn hand_over(&mut self, chunk: &[u8]) -> bool {
			// copied out of wasm memory, into a buffer of the message's own
			let chunk = Uint8Array::from(chunk);
			let posted = self
				.answers
				.port()
				.post_message_with_transferable(&chunk, &Array::of1(&chunk.buffer()));
			if let Err(e) = posted {
				// the commander then waits for good, and the call with it
				tracing::error!("failed to hand the commander a read of a stream: {e:?}");
				return false;
			}
			self.taken().await
		}

		fn end(self) {
			if let Err(e) = self.post_ending(&Ending::End) {
				// as for a read
				tracing::error!("failed to tell the commander that a stream ended: {e:?}");
			}
		}

		async fn fail(mut self, message: String) {
			if let Err(e) = self.post_ending(&Ending::Failed(message)) {
				// as for a read
				tracing::error!("failed to tell the commander that a stream failed: {e:?}");
				return;
			}
			// The stream is let go only once the failure is in the channel, or the consumer gone.
			self.taken().await;
		}
	}

	/// What the commander got from the stream's thread.
	enum Handed {
		Read(Uint8Array),
		Ending(Ending),
		/// Anything else, which breaks the exchange.
		Unknown,
	}

	impl Handed {
		fn from_js(value: JsValue) -> Self {
			match value.dyn_into::<Uint8Array>() {
				Ok(read) => Self::Read(read),
				Err(value) => {
					serde_wasm_bindgen::from_value(value).map_or(Self::Unknown, Self::Ending)
				}
			}
		}
	}

	/// The commander's half of a pump: opens the channel to the thread that made `request`, and
	/// puts what that thread reads into `sender`.
	pub(super) async fn feed_channel(
		request: PortRequest,
		sender: mpsc::Sender<Result<Vec<u8>, Error>>,
	) {
		let mut handed = match port_channel::open(request) {
			Ok(handed) => handed,
			Err(e) => {
				// in-band, so that the consumer does not take the closed channel for the end of
				// the stream
				let _ = sender.send(Err(e)).await;
				return;
			}
		};
		loop {
			let (item, last) = match Handed::from_js(handed.next().await) {
				// copied into wasm memory, out of the buffer the message transferred
				Handed::Read(read) => (Ok(read.to_vec()), false),
				Handed::Ending(Ending::Failed(message)) => {
					// the error was always only this message, with nothing underneath to keep
					(Err(Error::custom(ErrorKind::IO, message)), true)
				}
				Handed::Ending(Ending::End) => break,
				Handed::Unknown => {
					// The stream's thread, should it wait for an answer, lets the stream go.
					let _ = handed.port().post_message(&JsValue::from_str(GONE));
					let _ = sender
						.send(Err(broken(
							"the stream's thread sent what is no read".to_owned(),
						)))
						.await;
					break;
				}
			};
			let taken = sender.send(item).await.is_ok();
			let answer = JsValue::from_str(if taken { TAKEN } else { GONE });
			if let Err(e) = handed.port().post_message(&answer) {
				// The stream's thread then waits for good, and its stream stays open.
				if taken && !last {
					let _ = sender
						.send(Err(broken(format!(
							"failed to answer the stream's thread: {e:?}"
						))))
						.await;
				}
				break;
			}
			if last || !taken {
				break;
			}
		}
		handed.port().close();
	}
}

#[cfg(test)]
mod test_support {
	use tokio::sync::mpsc;

	use super::PumpSink;
	use crate::{Error, ErrorKind};

	/// Pumps into a channel, as the commander fills one with what the stream's thread reads.
	impl PumpSink for mpsc::Sender<Result<Vec<u8>, Error>> {
		async fn hand_over(&mut self, chunk: &[u8]) -> bool {
			self.send(Ok(chunk.to_vec())).await.is_ok()
		}

		fn end(self) {}

		async fn fail(self, message: String) {
			let _ = self.send(Err(Error::custom(ErrorKind::IO, message))).await;
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{cell::RefCell, io};

	use futures::{TryStreamExt, io::Cursor, stream};
	use tokio::sync::mpsc;

	use super::*;
	use crate::{Error, ErrorKind};

	/// Every item `pump_stream` sends over `reader`, once it has ended.
	async fn pump(
		reader: impl AsyncRead + Unpin,
		size_hint: Option<u64>,
		progress: Option<impl Fn(u64)>,
	) -> Vec<Result<Vec<u8>, Error>> {
		let (sender, mut receiver) = mpsc::channel(16);
		pump_stream(reader, size_hint, progress, sender).await;
		let mut items = Vec::new();
		while let Some(item) = receiver.recv().await {
			items.push(item);
		}
		items
	}

	fn chunks(items: Vec<Result<Vec<u8>, Error>>) -> Vec<Vec<u8>> {
		items.into_iter().map(Result::unwrap).collect()
	}

	#[tokio::test]
	async fn a_pump_reads_at_most_the_size_hint_at_a_time() {
		let progress = RefCell::new(Vec::new());
		let items = pump(
			Cursor::new(b"abcdefghij".to_vec()),
			Some(4),
			Some(|written| progress.borrow_mut().push(written)),
		)
		.await;
		assert_eq!(
			chunks(items),
			[b"abcd".to_vec(), b"efgh".to_vec(), b"ij".to_vec()]
		);
		assert_eq!(progress.into_inner(), [4, 8, 10]);
	}

	#[tokio::test]
	async fn a_pump_with_a_zero_size_hint_still_reads_the_stream() {
		let items = pump(Cursor::new(b"abc".to_vec()), Some(0), None::<fn(u64)>).await;
		assert_eq!(chunks(items), [b"abc".to_vec()]);
	}

	#[tokio::test]
	async fn a_pump_without_a_size_hint_reads_up_to_64_kib_at_a_time() {
		let items = pump(
			Cursor::new(vec![7u8; MAX_READ_LEN + 1]),
			None,
			None::<fn(u64)>,
		)
		.await;
		let lens = chunks(items).iter().map(Vec::len).collect::<Vec<_>>();
		assert_eq!(lens, [MAX_READ_LEN, 1]);
	}

	#[tokio::test]
	async fn a_pump_sends_a_read_error_as_its_last_item() {
		let reader = stream::iter([
			Ok(b"partial".to_vec()),
			Err(io::Error::other("broken")),
			Ok(b"never read".to_vec()),
		])
		.into_async_read();
		let mut items = pump(reader, None, None::<fn(u64)>).await;

		let error = items.pop().unwrap().unwrap_err();
		assert_eq!(error.kind(), ErrorKind::IO);
		assert_eq!(
			error.to_string(),
			"Error of kind IO: context: error reading from stream: Custom { kind: Other, error: \"broken\" }"
		);
		assert_eq!(chunks(items), [b"partial".to_vec()]);
	}

	#[tokio::test]
	async fn a_pump_stops_once_its_consumer_is_gone() {
		let (sender, receiver) = mpsc::channel(16);
		drop(receiver);
		let progress = RefCell::new(Vec::new());
		let mut reader = Cursor::new(b"abcdefgh".to_vec());

		pump_stream(
			&mut reader,
			Some(4),
			Some(|written| progress.borrow_mut().push(written)),
			sender,
		)
		.await;

		assert_eq!(reader.position(), 4);
		assert!(progress.into_inner().is_empty());
	}
}
