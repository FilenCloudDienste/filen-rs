//! A JS `ReadableStream<Uint8Array>` the SDK reads from its own runtimes, for an upload from a
//! stream.
//!
//! The SDK pulls. Each read it makes asks the JS thread, through a weak threadsafe function, for at
//! most as many bytes as its buffer holds, and nothing more is asked until that read is answered.
//! The JS thread reads the stream's next chunk only once the last one is used up, keeps that chunk
//! (a reference to it, not a copy) and copies out of it what a read asks for. The copy is made on
//! the JS thread, as `bytes::read` makes one: napi's `Uint8Array` borrows memory JS may change. So
//! however fast the stream's source produces, what this holds of it is one chunk on the JS side and
//! one read's copy on the SDK's, besides what the stream queues itself (its high-water mark). The
//! bound is the stream's chunk, however large its source makes it, not the read's size: one 500 MB
//! chunk is held whole until the upload has read all of it.
//!
//! The stream stays locked to its reader whatever the outcome, as on wasm: the reader's lock is
//! never released, and a stream not read to its end is cancelled.
//!
//! Dropping the reader before the stream ended (an aborted upload, a failed one) cancels the
//! stream; a rejection of that `cancel` is swallowed. A read the stream rejects, or a chunk that is
//! not a `Uint8Array`, fails the read with an `IO` error, which fails the upload.

use std::{
	cell::{Cell, RefCell},
	future::Future,
	io,
	num::NonZeroUsize,
	ops::Range,
	panic::{AssertUnwindSafe, catch_unwind},
	pin::Pin,
	rc::Rc,
	task::{Context, Poll},
};

use futures::AsyncRead;
use napi::{
	Env, Status,
	bindgen_prelude::{
		BigInt, CallbackContext, Function, FunctionRef, JsObjectValue, Object, PromiseRaw,
		Uint8Array, Unknown,
	},
	threadsafe_function::{ThreadsafeCallContext, ThreadsafeFunctionCallMode},
};
use tokio::sync::oneshot;

use super::{IntoWire, bound_method};
use crate::{Error, ErrorKind};

/// What the JS thread answers a read with.
enum Answer {
	/// Bytes of the stream, never more than the read asked for.
	Bytes(Vec<u8>),
	/// The stream ended.
	End,
}

/// What a [`StreamReader`] asks the JS thread.
enum Request {
	/// The stream's next bytes, at most `max` of them, through `answer`.
	Read {
		max: NonZeroUsize,
		answer: oneshot::Sender<Result<Answer, Error>>,
	},
	/// Cancel the stream: the reader is gone before it ended.
	Cancel,
}

type SendFn = Box<dyn Fn(Request) -> Status + Send + Sync>;

/// Where a [`StreamReader`] stands.
enum ReadState {
	/// Nothing asked, nothing held.
	Idle,
	/// A read was asked for and not answered yet.
	Asked(oneshot::Receiver<Result<Answer, Error>>),
	/// Bytes answered that the last read had no room for, from `read` on.
	Holding { bytes: Vec<u8>, read: usize },
	/// The stream ended.
	Ended,
	/// A read failed (also what a poll leaves while it has the state out).
	Failed,
}

/// A JS `ReadableStream`, read as an `AsyncRead` from any thread. Built on the JS thread.
pub(crate) struct StreamReader {
	send: SendFn,
	state: ReadState,
}

impl StreamReader {
	/// Locks `stream` to a reader of its own, on the JS thread. A `stream` without a `getReader`
	/// method throws napi's own error; one that cannot be read (it is locked already) is a
	/// `Conversion` error for the call to reject with (`Ok(Err)`), as on wasm. `progress` gets the
	/// bytes read from the stream so far, a running total, after each read.
	pub(crate) fn new(
		env: &Env,
		stream: &Object<'_>,
		progress: Option<Function<'_, BigInt, ()>>,
	) -> napi::Result<Result<Self, Error>> {
		let get_reader: Function<'_, (), Object<'_>> = stream.get_named_property("getReader")?;
		// `call`, not `apply`: a throw comes back as the error instead of staying pending.
		let reader = match get_reader.bind(stream)?.call(()) {
			Ok(reader) => reader,
			Err(e) => {
				return Ok(Err(Error::custom(
					ErrorKind::Conversion,
					format!("got error when converting to ReadableStream: {e}"),
				)));
			}
		};
		let js = Rc::new(JsSide {
			read: bound_method(&reader, "read")?,
			cancel: bound_method(&reader, "cancel")?,
			progress: progress.map(|progress| progress.create_ref()).transpose()?,
			chunk: RefCell::new(None),
			handed_over: Cell::new(0),
		});
		// The callback does the work itself, so the function the threadsafe function calls after
		// it does nothing. Weak: a pending read never keeps Node's event loop alive (the upload's
		// promise does).
		let tsfn = env
			.create_function_from_closure::<(), (), _>("streamReader", |_| Ok(()))?
			.build_threadsafe_function::<Request>()
			.weak::<true>()
			.build_callback(move |ctx: ThreadsafeCallContext<Request>| {
				js.handle(&ctx.env, ctx.value);
				Ok(())
			})?;
		Ok(Ok(Self::with_send(move |request| {
			tsfn.call(request, ThreadsafeFunctionCallMode::NonBlocking)
		})))
	}

	fn with_send(send: impl Fn(Request) -> Status + Send + Sync + 'static) -> Self {
		Self {
			send: Box::new(send),
			state: ReadState::Idle,
		}
	}
}

/// The read error for a JS environment that went away with the stream.
fn env_gone() -> io::Error {
	io::Error::other(Error::custom(
		ErrorKind::IO,
		"error reading from stream: its JS environment is gone",
	))
}

impl AsyncRead for StreamReader {
	fn poll_read(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &mut [u8],
	) -> Poll<io::Result<usize>> {
		let this = self.get_mut();
		let Some(max) = NonZeroUsize::new(buf.len()) else {
			return Poll::Ready(Ok(0));
		};
		loop {
			match std::mem::replace(&mut this.state, ReadState::Failed) {
				ReadState::Idle => {
					let (answer, answered) = oneshot::channel();
					match (this.send)(Request::Read { max, answer }) {
						Status::Ok => this.state = ReadState::Asked(answered),
						Status::Closing => return Poll::Ready(Err(env_gone())),
						status => {
							return Poll::Ready(Err(io::Error::other(Error::custom(
								ErrorKind::IO,
								format!(
									"error reading from stream: the read was not handed to JS: {status}"
								),
							))));
						}
					}
				}
				ReadState::Asked(mut answered) => match Pin::new(&mut answered).poll(cx) {
					Poll::Pending => {
						this.state = ReadState::Asked(answered);
						return Poll::Pending;
					}
					Poll::Ready(Ok(Ok(Answer::Bytes(bytes)))) => {
						this.state = ReadState::Holding { bytes, read: 0 };
					}
					Poll::Ready(Ok(Ok(Answer::End))) => {
						this.state = ReadState::Ended;
						return Poll::Ready(Ok(0));
					}
					Poll::Ready(Ok(Err(e))) => return Poll::Ready(Err(io::Error::other(e))),
					// The request was dropped unanswered, as its environment went away.
					Poll::Ready(Err(_)) => return Poll::Ready(Err(env_gone())),
				},
				ReadState::Holding { bytes, read } => {
					let Some(rest) = bytes.get(read..).filter(|rest| !rest.is_empty()) else {
						// an empty answer is no end: ask again
						this.state = ReadState::Idle;
						continue;
					};
					let len = rest.len().min(buf.len());
					buf[..len].copy_from_slice(&rest[..len]);
					let read = read + len;
					this.state = if read < bytes.len() {
						ReadState::Holding { bytes, read }
					} else {
						ReadState::Idle
					};
					return Poll::Ready(Ok(len));
				}
				ReadState::Ended => {
					this.state = ReadState::Ended;
					return Poll::Ready(Ok(0));
				}
				ReadState::Failed => {
					return Poll::Ready(Err(io::Error::other(Error::custom(
						ErrorKind::IO,
						"error reading from stream: read again after a failed read",
					))));
				}
			}
		}
	}
}

impl Drop for StreamReader {
	fn drop(&mut self) {
		if !matches!(self.state, ReadState::Ended) {
			// Lost only once the environment is gone, and the stream with it.
			let _ = (self.send)(Request::Cancel);
		}
	}
}

/// What of a chunk of `len` bytes, read up to `read`, the next read of at most `max` takes.
fn next_range(len: usize, read: usize, max: NonZeroUsize) -> Range<usize> {
	let start = read.min(len);
	start..start + (len - start).min(max.get())
}

/// The JS thread's side of a [`StreamReader`]: the stream's reader, bound, and the chunk being
/// read. Only ever touched on the JS thread.
struct JsSide {
	read: FunctionRef<(), PromiseRaw<'static, Object<'static>>>,
	cancel: FunctionRef<(), PromiseRaw<'static, Unknown<'static>>>,
	progress: Option<FunctionRef<BigInt, ()>>,
	/// The chunk being read and how much of it has been, until it is used up.
	chunk: RefCell<Option<(Uint8Array, usize)>>,
	/// The bytes handed to the SDK so far, for `progress`.
	handed_over: Cell<u64>,
}

/// A read's answer, shared by the handlers of the JS promise it waits on; whichever runs first
/// takes it.
type SharedAnswer = Rc<Cell<Option<oneshot::Sender<Result<Answer, Error>>>>>;

/// A read the stream failed with `reason`.
fn stream_error(reason: &str) -> Error {
	Error::custom(
		ErrorKind::IO,
		format!("error reading from stream: {reason}"),
	)
}

impl JsSide {
	/// Handle `request`. Never unwinds: it runs inside napi's `extern "C"` callback, where an
	/// unwind would abort the process.
	fn handle(self: &Rc<Self>, env: &Env, request: Request) {
		let handled = catch_unwind(AssertUnwindSafe(|| match request {
			Request::Read { max, answer } => self.pull(env, max, answer),
			Request::Cancel => self.cancel(env),
		}));
		if handled.is_err() {
			tracing::error!("a stream read's dispatch panicked; the read was lost");
		}
	}

	/// Answer a read of at most `max` bytes out of the current chunk, or out of the next one the
	/// stream gives once it does.
	fn pull(
		self: &Rc<Self>,
		env: &Env,
		max: NonZeroUsize,
		answer: oneshot::Sender<Result<Answer, Error>>,
	) {
		if answer.is_closed() {
			// the reader is gone: nothing to read for
			return;
		}
		if let Some(bytes) = self.take_from_chunk(max) {
			self.hand_over(env, bytes, answer);
			return;
		}
		let promise = match self.read.borrow_back(env).and_then(|read| read.call(())) {
			Ok(promise) => promise,
			Err(e) => {
				let _ = answer.send(Err(stream_error(&e.reason)));
				return;
			}
		};
		let answer: SharedAnswer = Rc::new(Cell::new(Some(answer)));
		let (js, on_read) = (Rc::clone(self), Rc::clone(&answer));
		let on_rejected = Rc::clone(&answer);
		// Neither handler fails, and the `catch` takes a rejection of `read()`, so nothing is left
		// for JS to report as an unhandled rejection.
		let attached = promise
			.then(move |ctx: CallbackContext<Object<'static>>| {
				if let Some(answer) = on_read.take() {
					js.on_read(&ctx.env, &ctx.value, max, answer);
				}
				Ok(())
			})
			.and_then(|then| {
				then.catch(move |ctx: CallbackContext<Unknown<'static>>| {
					if let Some(answer) = on_rejected.take() {
						// read without running JS (no getter, no `toString`) and clearing what it leaves
						let reason = napi::Error::from_unknown_without_coercion(ctx.value).reason;
						let _ = answer.send(Err(stream_error(&reason)));
					}
					Ok(())
				})
			});
		if let Err(e) = attached
			&& let Some(answer) = answer.take()
		{
			let _ = answer.send(Err(stream_error(&e.reason)));
		}
	}

	/// The stream's `read()` resolved with `result`: keep its chunk and answer out of it, or tell
	/// the reader the stream ended.
	fn on_read(
		self: &Rc<Self>,
		env: &Env,
		result: &Object<'_>,
		max: NonZeroUsize,
		answer: oneshot::Sender<Result<Answer, Error>>,
	) {
		if answer.is_closed() {
			// The reader went away while the read was pending, and its cancel may have run already:
			// the chunk is not kept, or it would be held until the last handler is dropped.
			return;
		}
		match result.get_named_property::<bool>("done") {
			Ok(true) => {
				let _ = answer.send(Ok(Answer::End));
			}
			Ok(false) => match result.get_named_property::<Uint8Array>("value") {
				Ok(chunk) => {
					*self.chunk.borrow_mut() = Some((chunk, 0));
					// an empty chunk is skipped: only the end of the stream reads as 0
					self.pull(env, max, answer);
				}
				Err(_) => {
					// the chunk's type only: its contents are the caller's data
					let found = result
						.get_named_property::<Unknown>("value")
						.and_then(|value| value.get_type())
						.map_or_else(|_| "unknown".to_owned(), |found| format!("{found:?}"));
					let _ = answer.send(Err(Error::custom(
						ErrorKind::IO,
						format!(
							"error reading from stream: a stream chunk is not a Uint8Array ({found})"
						),
					)));
				}
			},
			Err(e) => {
				let _ = answer.send(Err(stream_error(&e.reason)));
			}
		}
	}

	/// Copies up to `max` bytes out of the current chunk, dropping it once it is used up; `None`
	/// when there is none left to read.
	fn take_from_chunk(&self, max: NonZeroUsize) -> Option<Vec<u8>> {
		let mut current = self.chunk.borrow_mut();
		let (chunk, read) = current.as_mut()?;
		let range = next_range(chunk.len(), *read, max);
		if range.is_empty() {
			*current = None;
			return None;
		}
		let bytes = chunk.get(range.clone())?.to_vec();
		*read = range.end;
		if range.end == chunk.len() {
			*current = None;
		}
		Some(bytes)
	}

	/// Hands `bytes` to the reader, then tells `progress` the running total.
	fn hand_over(&self, env: &Env, bytes: Vec<u8>, answer: oneshot::Sender<Result<Answer, Error>>) {
		let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
		if answer.send(Ok(Answer::Bytes(bytes))).is_err() {
			// the reader is gone
			return;
		}
		let handed_over = self.handed_over.get().saturating_add(len);
		self.handed_over.set(handed_over);
		if let Some(progress) = &self.progress {
			// `call`, not `apply`: a throw comes back as the error instead of staying pending.
			let called = progress
				.borrow_back(env)
				.and_then(|progress| progress.call(handed_over.into_wire()));
			if let Err(e) = called {
				tracing::warn!("a stream upload's progress callback threw: {e}");
			}
		}
	}

	/// Cancels the stream, dropping the chunk being read.
	fn cancel(&self, env: &Env) {
		self.chunk.borrow_mut().take();
		let cancelled = self
			.cancel
			.borrow_back(env)
			.and_then(|cancel| cancel.call(()))
			// a stream that failed rejects its cancel too
			.and_then(|promise| promise.catch(|_: CallbackContext<Unknown<'static>>| Ok(())));
		if let Err(e) = cancelled {
			tracing::debug!("cancelling a stream failed: {e}");
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::{Arc, Mutex};

	use futures::AsyncReadExt;

	use super::*;
	use filen_types::fs::Uuid;

	use crate::auth::test_support::offline_client;

	/// What a fake JS thread was asked, in order, the reads by how many bytes they asked for.
	#[derive(Debug, PartialEq)]
	enum Asked {
		Read(usize),
		Cancel,
	}

	/// The fake JS thread's record of what it was asked, and the reads it has not answered yet.
	#[derive(Default)]
	struct FakeJs {
		asked: Vec<Asked>,
		unanswered: Vec<oneshot::Sender<Result<Answer, Error>>>,
	}

	type Shared = Arc<Mutex<FakeJs>>;

	/// A reader whose reads a fake JS thread answers with `answers` in turn, as they come, and
	/// leaves unanswered once they run out; and that thread's record.
	fn reader(answers: Vec<Result<Answer, Error>>) -> (StreamReader, Shared) {
		let js = Shared::default();
		let record = Arc::clone(&js);
		let answers = Mutex::new(answers.into_iter());
		let reader = StreamReader::with_send(move |request| {
			let mut js = record.lock().unwrap();
			match request {
				Request::Read { max, answer } => {
					js.asked.push(Asked::Read(max.get()));
					match answers.lock().unwrap().next() {
						Some(answered) => {
							let _ = answer.send(answered);
						}
						None => js.unanswered.push(answer),
					}
				}
				Request::Cancel => js.asked.push(Asked::Cancel),
			}
			Status::Ok
		});
		(reader, js)
	}

	fn asked(js: &Shared) -> Vec<Asked> {
		std::mem::take(&mut js.lock().unwrap().asked)
	}

	fn bytes(bytes: &[u8]) -> Result<Answer, Error> {
		Ok(Answer::Bytes(bytes.to_vec()))
	}

	fn read_error() -> Error {
		stream_error("the source failed")
	}

	#[tokio::test]
	async fn a_stream_is_read_in_order_one_read_asked_at_a_time_then_ends() {
		let (mut reader, js) = reader(vec![bytes(b"hello "), bytes(b"world"), Ok(Answer::End)]);
		let mut buf = [0u8; 8];

		assert_eq!(reader.read(&mut buf).await.unwrap(), 6);
		assert_eq!(&buf[..6], b"hello ");
		assert_eq!(reader.read(&mut buf).await.unwrap(), 5);
		assert_eq!(&buf[..5], b"world");
		assert_eq!(reader.read(&mut buf).await.unwrap(), 0);
		// read again after the end: no request
		assert_eq!(reader.read(&mut buf).await.unwrap(), 0);

		assert_eq!(asked(&js), [Asked::Read(8), Asked::Read(8), Asked::Read(8)]);
		drop(reader);
		assert_eq!(asked(&js), [], "a stream read to its end is not cancelled");
	}

	#[tokio::test]
	async fn a_read_waits_for_its_answer_without_asking_again() {
		let (mut reader, js) = reader(Vec::new());
		let mut buf = [0u8; 4];
		{
			let mut read = std::pin::pin!(reader.read(&mut buf));
			assert!(futures::poll!(read.as_mut()).is_pending());
			assert!(futures::poll!(read.as_mut()).is_pending());
			assert_eq!(asked(&js), [Asked::Read(4)]);

			let answer = js.lock().unwrap().unanswered.pop().unwrap();
			answer.send(bytes(b"abc")).ok().unwrap();
			assert_eq!(read.await.unwrap(), 3);
		}
		assert_eq!(&buf[..3], b"abc");
	}

	#[tokio::test]
	async fn an_answer_larger_than_the_buffer_is_handed_out_in_parts_before_asking_again() {
		let (mut reader, js) = reader(vec![bytes(b"abcdef"), Ok(Answer::End)]);
		let mut small = [0u8; 4];

		assert_eq!(reader.read(&mut small).await.unwrap(), 4);
		assert_eq!(&small, b"abcd");
		assert_eq!(reader.read(&mut small).await.unwrap(), 2);
		assert_eq!(&small[..2], b"ef");
		assert_eq!(reader.read(&mut small).await.unwrap(), 0);

		assert_eq!(asked(&js), [Asked::Read(4), Asked::Read(4)]);
	}

	#[tokio::test]
	async fn an_empty_answer_is_asked_again_not_read_as_the_end() {
		let (mut reader, _js) = reader(vec![bytes(b""), bytes(b"x"), Ok(Answer::End)]);
		let mut buf = [0u8; 4];

		assert_eq!(reader.read(&mut buf).await.unwrap(), 1);
		assert_eq!(reader.read(&mut buf).await.unwrap(), 0);
	}

	#[tokio::test]
	async fn an_empty_buffer_reads_nothing_and_asks_nothing() {
		let (mut reader, js) = reader(vec![bytes(b"x")]);

		assert_eq!(reader.read(&mut []).await.unwrap(), 0);
		assert_eq!(asked(&js), []);
	}

	#[tokio::test]
	async fn a_read_the_stream_fails_fails_with_its_io_error_and_cancels_on_drop() {
		let (mut reader, js) = reader(vec![bytes(b"partial"), Err(read_error())]);
		let mut buf = [0u8; 16];

		assert_eq!(reader.read(&mut buf).await.unwrap(), 7);
		let error = Error::from(reader.read(&mut buf).await.unwrap_err());
		assert_eq!(error.kind(), ErrorKind::IO);
		assert_eq!(
			error.to_string(),
			"Error of kind IO: context: error reading from stream: the source failed"
		);

		drop(reader);
		assert_eq!(
			asked(&js),
			[Asked::Read(16), Asked::Read(16), Asked::Cancel]
		);
	}

	#[tokio::test]
	async fn a_reader_dropped_before_the_end_cancels_the_stream() {
		let (mut reader, js) = reader(vec![bytes(b"abc")]);
		assert_eq!(reader.read(&mut [0u8; 4]).await.unwrap(), 3);

		drop(reader);
		assert_eq!(asked(&js), [Asked::Read(4), Asked::Cancel]);
	}

	#[tokio::test]
	async fn a_read_dropped_unanswered_fails_as_the_environment_gone() {
		let (mut reader, js) = reader(Vec::new());
		let mut buf = [0u8; 4];
		let mut read = std::pin::pin!(reader.read(&mut buf));
		assert!(futures::poll!(read.as_mut()).is_pending());

		js.lock().unwrap().unanswered.clear();

		let error = Error::from(read.await.unwrap_err());
		assert_eq!(error.kind(), ErrorKind::IO);
		assert!(
			error.to_string().contains("its JS environment is gone"),
			"{error}"
		);
	}

	#[tokio::test]
	async fn a_read_the_environment_refuses_fails_without_waiting() {
		let mut reader = StreamReader::with_send(|_| Status::Closing);

		let error = Error::from(reader.read(&mut [0u8; 4]).await.unwrap_err());
		assert_eq!(error.kind(), ErrorKind::IO);
		assert!(
			error.to_string().contains("its JS environment is gone"),
			"{error}"
		);
	}

	#[tokio::test]
	async fn an_upload_from_a_stream_that_fails_midway_ends_with_the_read_error() {
		let client = offline_client();
		let builder = client
			.make_file_builder("payload.bin", Uuid::from_u128(1))
			.unwrap();
		let (mut reader, js) = reader(vec![bytes(&[7u8; 1024]), Err(read_error())]);

		let error = client
			.upload_file_from_reader(builder, &mut reader, None, None)
			.await
			.unwrap_err();

		assert_eq!(error.kind(), ErrorKind::IO);
		assert_eq!(
			error.to_string(),
			"Error of kind IO: context: error reading from stream: the source failed"
		);
		drop(reader);
		assert_eq!(asked(&js).last(), Some(&Asked::Cancel));
	}

	#[test]
	fn a_read_takes_at_most_its_max_from_the_rest_of_the_chunk() {
		let max = |n| NonZeroUsize::new(n).unwrap();
		assert_eq!(next_range(10, 0, max(4)), 0..4);
		assert_eq!(next_range(10, 8, max(4)), 8..10);
		assert_eq!(next_range(10, 10, max(4)), 10..10);
		assert_eq!(next_range(0, 0, max(4)), 0..0);
		// a read position past the end, which nothing makes, takes nothing
		assert_eq!(next_range(3, 5, max(4)), 3..3);
		assert_eq!(next_range(usize::MAX, 0, max(usize::MAX)), 0..usize::MAX);
	}
}
