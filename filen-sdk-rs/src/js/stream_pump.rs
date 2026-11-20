#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use std::{
	io,
	pin::Pin,
	task::{Context, Poll, ready},
};

use futures::{AsyncRead, AsyncReadExt};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use futures::{StreamExt, future::Either};
use tokio::sync::mpsc;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_bindgen::{JsCast, JsValue};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_streams::readable::IntoStream;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use web_sys::js_sys::Uint8Array;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use crate::js::js_error_message;
use crate::{Error, ErrorKind};

/// The most a pump reads from its stream at a time.
const MAX_READ_LEN: usize = 64 * 1024;

/// Pumps a JS `ReadableStream` into a channel from a task on the CALLING thread, which is where
/// the stream's JS object lives: a `ReadableStream` cannot be moved to the commander.
///
/// Reads at most 64 KiB at a time, and no more than `size_hint` when the caller knows how much
/// there is. `progress` gets the bytes handed over so far after each read.
///
/// A read error arrives in-band as the last item, so the consumer sees it where it sees the bytes;
/// a closed channel only ever means the end of the stream. A consumer that stops reading drops the
/// receiver; the pump notices when its next read is done, and ends, cancelling the stream.
///
/// Fails with `Conversion` only when the stream cannot be read at all, because it is already
/// locked to a reader.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) fn spawn_stream_pump(
	stream: web_sys::ReadableStream,
	size_hint: Option<u64>,
	progress: Option<impl Fn(u64) + 'static>,
) -> Result<mpsc::Receiver<Result<Vec<u8>, Error>>, Error> {
	let reader = stream_reader(stream)?;
	let (sender, receiver) = mpsc::channel(10);
	crate::runtime::spawn_local(pump_stream(reader, size_hint, progress, sender));
	Ok(receiver)
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

/// The body of [`spawn_stream_pump`]'s task.
pub(crate) async fn pump_stream(
	mut reader: impl AsyncRead + Unpin,
	size_hint: Option<u64>,
	progress: Option<impl Fn(u64)>,
	sender: mpsc::Sender<Result<Vec<u8>, Error>>,
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
			Ok(0) => return, // EOF
			Ok(n) => {
				// a closed channel: the consumer has ended, with a result of its own
				if sender.send(Ok(buffer[..n].to_vec())).await.is_err() {
					return;
				}
				if let Some(progress) = &progress {
					written += n as u64;
					progress(written);
				}
			}
			Err(e) => {
				let _ = sender
					.send(Err(Error::custom(
						ErrorKind::IO,
						format!("error reading from stream: {e:?}"),
					)))
					.await;
				return;
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{cell::RefCell, io};

	use futures::{TryStreamExt, io::Cursor, stream};

	use super::*;

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
