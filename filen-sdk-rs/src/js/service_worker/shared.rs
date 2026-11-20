use std::mem;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use crate::js::{AnyFile, ManagedFuture};
#[cfg(feature = "wasm-full")]
use crate::runtime::{port_from_worker, spawn_on_commander};
use crate::{
	Error, ErrorKind,
	fs::file::service_worker::{MAX_BUFFER_SIZE_BEFORE_FLUSH, StreamWriter, WriteFrame},
	runtime::spawn_local,
};

use filen_macros::js_type;
use futures::SinkExt;
use tokio::sync::{mpsc, oneshot};
use wasm_bindgen::{JsCast, JsValue};
use wasm_streams::writable::IntoSink;
use web_sys::js_sys::{self, Uint8Array};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[js_type(import, wasm_all, no_ser, no_default)]
pub struct DownloadFileStreamParams {
	pub file: AnyFile,
	#[tsify(type = "WritableStream<Uint8Array>")]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub writer: web_sys::WritableStream,
	#[tsify(type = "(bytes: bigint) => void", optional)]
	#[serde(default, deserialize_with = "optional_function")]
	pub progress: Option<js_sys::Function>,
	#[serde(default)]
	#[tsify(type = "bigint")]
	pub start: Option<u64>,
	#[serde(default)]
	#[tsify(type = "bigint")]
	pub end: Option<u64>,
	// swap to flatten when https://github.com/madonoharu/tsify/issues/68 is resolved
	// #[serde(flatten)]
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[serde(default)]
	pub managed_future: ManagedFuture,
}

/// Deserializes an optional JS callback, for `#[serde(default, deserialize_with = ...)]` fields of
/// `Option<js_sys::Function>`. `undefined` and `null` mean no callback. A plain `js_sys::Function`
/// field with `#[serde(default)]` cannot be used: `Function::default()` is `new Function("")`, which
/// is eval and throws under a Content-Security-Policy without `unsafe-eval`, so every call that
/// omitted the callback would fail before it started.
pub(crate) fn optional_function<'de, D: serde::Deserializer<'de>>(
	deserializer: D,
) -> Result<Option<js_sys::Function>, D::Error> {
	let value: JsValue = serde_wasm_bindgen::preserve::deserialize(deserializer)?;
	if value.is_undefined() || value.is_null() {
		return Ok(None);
	}
	value
		.dyn_into::<js_sys::Function>()
		.map(Some)
		.map_err(|_| serde::de::Error::custom("expected a function"))
}

// #[wasm_bindgen::prelude::wasm_bindgen]
// unsafe extern "C" {
// 	#[wasm_bindgen::prelude::wasm_bindgen(extends = Function, is_type_of = JsValue::is_function, typescript_type = "(bytesWritten: bigint, totalBytes: bigint, itemsProcessed: bigint, totalItems: bigint) => void")]
// 	pub type ZipProgressCallbackJS;
// 	#[wasm_bindgen::prelude::wasm_bindgen(method, catch, js_name = call)]
// 	pub unsafe fn call4(
// 		this: &ZipProgressCallbackJS,
// 		context: &JsValue,
// 		arg1: &JsValue,
// 		arg2: &JsValue,
// 		arg3: &JsValue,
// 		arg4: &JsValue,
// 	) -> Result<JsValue, JsValue>;
// }

// #[cfg(all(target_family = "wasm", target_os = "unknown"))]
// impl Default for ZipProgressCallbackJS {
// 	fn default() -> Self {
// 		wasm_bindgen::JsCast::unchecked_into(JsValue::undefined())
// 	}
// }

// #[cfg(all(target_family = "wasm", target_os = "unknown"))]
// impl ZipProgressCallbackJS {
// 	pub(crate) fn into_rust_callback(self) -> Option<impl ZipProgressCallback> {
// 		if self.is_undefined() {
// 			None
// 		} else {
// 			let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

// 			wasm_bindgen_futures::spawn_local(async move {
// 				while let Some((bytes_written, files_dirs_written, bytes_total, files_dirs_total)) =
// 					receiver.recv().await
// 				{
// 					let _ = unsafe {
// 						self.call4(
// 							&JsValue::NULL,
// 							&BigInt::from(bytes_written).into(),
// 							&BigInt::from(files_dirs_written).into(),
// 							&BigInt::from(bytes_total).into(),
// 							&BigInt::from(files_dirs_total).into(),
// 						)
// 					};
// 				}
// 			});
// 			Some(
// 				move |bytes_written: u64,
// 				      files_dirs_written: u64,
// 				      bytes_total: u64,
// 				      files_dirs_total: u64| {
// 					let _ = sender.send((
// 						bytes_written,
// 						files_dirs_written,
// 						bytes_total,
// 						files_dirs_total,
// 					));
// 				},
// 			)
// 		}
// 	}
// }

// #[cfg(all(target_family = "wasm", target_os = "unknown"))]
// #[derive(Deserialize, tsify::Tsify)]
// #[tsify(from_wasm_abi)]
// #[serde(rename_all = "camelCase")]
// pub struct DownloadFileToZipParams {
// 	pub items: Vec<Item>,
// 	#[tsify(type = "WritableStream<Uint8Array>")]
// 	#[serde(with = "serde_wasm_bindgen::preserve")]
// 	pub writer: web_sys::WritableStream,
// 	#[serde(default, with = "serde_wasm_bindgen::preserve")]
// 	#[tsify(
// 		type = "(bytesWritten: bigint, totalBytes: bigint, itemsProcessed: bigint, totalItems: bigint) => void"
// 	)]
// 	pub progress: ZipProgressCallbackJS,
// 	// swap to flatten when https://github.com/madonoharu/tsify/issues/68 is resolved
// 	// #[serde(flatten)]
// 	#[serde(default)]
// 	pub managed_future: ManagedFuture,
// }

/// Bridges `stream` to a `Send` writer: the stream stays on this thread, which writes it what the
/// [`StreamWriter`] flushes, small frames gathered first, closes it once the writer is closed and
/// aborts it when the writer is dropped first. `progress` gets, on this thread, the bytes handed to
/// the stream so far after each write. The receiver answers once the stream is closed, failed or
/// aborted. A stream that cannot be written fails with `Conversion`, its message starting with
/// `conversion_failure`.
///
/// With threads (`wasm-full`), the frames are gathered on the commander worker, which hands each
/// chunk to this thread over a `MessageChannel` and waits until it was written before it gathers
/// the next, so no more is buffered than when this thread gathered them. This thread, the page's
/// main thread as a rule, then only takes messages, never a lock: receiving from the writer's
/// channel locks a mutex the writing worker locks too, and the page may not wait for one a worker
/// holds (Chromium throws "Atomics.wait cannot be called in this context", WebKit traps). The
/// commander posts the channel's port to its parent, so this is called on the thread that spawned
/// the commander: the one JS calls the SDK on. The service worker build has no threads and gathers
/// the frames here.
pub(crate) fn stream_writer<P: Fn(u64) + 'static>(
	stream: web_sys::WritableStream,
	progress: Option<P>,
	conversion_failure: &'static str,
) -> Result<(StreamWriter, oneshot::Receiver<Result<(), Error>>), Error> {
	let stream = JsStream::new(stream, progress, conversion_failure)?;
	let (data_sender, data_receiver) = mpsc::channel(10);
	let (result_sender, result_receiver) = oneshot::channel();
	let frames = Frames::new(data_receiver);
	#[cfg(feature = "wasm-full")]
	{
		let request = port_from_worker(move |port| match port {
			Some(port) => spawn_local(stream.write_orders(port)),
			// The commander was given up on before it started the hand-over: nothing takes the
			// writer's frames any more, and the stream ends as for a writer dropped unclosed.
			None => spawn_local(stream.abort()),
		});
		spawn_on_commander(move || async move {
			let _ = result_sender.send(frames.hand_over(request).await);
		});
	}
	#[cfg(not(feature = "wasm-full"))]
	spawn_local(async move {
		let _ = result_sender.send(frames.write_to(stream).await);
	});
	Ok((StreamWriter::new(data_sender), result_receiver))
}

/// A JS value as text, as wasm-streams words a stream's errors, so the SDK's own readers and
/// writers word them the same. Kept as text because a `JsValue` is not `Send`, so it cannot be an
/// `io::Error`'s source.
pub(crate) fn js_error_message(value: &JsValue) -> String {
	value
		.as_string()
		.or_else(|| {
			js_sys::Object::try_from(value).and_then(|object| object.to_string().as_string())
		})
		.unwrap_or_else(|| "Unknown error".to_owned())
}

/// How a write to the stream failed, worded as the receiver's error is. Text, so that it can cross
/// to the commander, where the receiver is answered.
struct StreamFailure(String);

impl StreamFailure {
	/// Words a failure as wasm-streams' `IntoAsyncWrite` does: an `io::Error` holding the JS
	/// error's text.
	fn new(doing: &str, error: &JsValue) -> Self {
		Self(format!(
			"{doing}: {:?}",
			std::io::Error::other(js_error_message(error))
		))
	}
}

impl From<StreamFailure> for Error {
	fn from(failure: StreamFailure) -> Self {
		// the error was always only this message, with nothing underneath to keep
		Error::custom(ErrorKind::IO, failure.0)
	}
}

/// The error the receiver answers with when the writer was dropped before it closed.
fn producer_dropped() -> Error {
	Error::custom(
		ErrorKind::Cancelled,
		"stream producer dropped before completing",
	)
}

/// The caller's `WritableStream`, written on the thread it lives on.
struct JsStream<P> {
	sink: IntoSink<'static>,
	progress: Option<P>,
	/// The bytes handed to the stream so far.
	written: u64,
}

impl<P: Fn(u64)> JsStream<P> {
	fn new(
		stream: web_sys::WritableStream,
		progress: Option<P>,
		conversion_failure: &'static str,
	) -> Result<Self, Error> {
		let sink = wasm_streams::WritableStream::from_raw(stream)
			.try_into_sink()
			.map_err(|(e, _)| {
				Error::custom(
					ErrorKind::Conversion,
					format!("{conversion_failure}: {e:?}"),
				)
			})?;
		Ok(Self {
			sink,
			progress,
			written: 0,
		})
	}

	/// Hands `chunk` to the stream once it is ready for more, without waiting for the write.
	async fn write(&mut self, chunk: Uint8Array) -> Result<(), StreamFailure> {
		let len = chunk.length();
		self.sink
			.feed(chunk.into())
			.await
			.map_err(|e| StreamFailure::new("error writing to stream", &e))?;
		if let Some(progress) = &self.progress {
			self.written += u64::from(len);
			progress(self.written);
		}
		Ok(())
	}

	async fn close(mut self) -> Result<(), StreamFailure> {
		self.sink
			.close()
			.await
			.map_err(|e| StreamFailure::new("error closing stream", &e))
	}

	/// Aborts the stream, so that the JS side discards what it got instead of keeping a truncated
	/// file as complete.
	async fn abort(self) {
		let _ = self
			.sink
			.abort_with_reason(&JsValue::from_str("download did not complete"))
			.await;
	}
}

/// What a [`Frames::next`] call returns.
enum Gathered {
	/// A chunk to write.
	Chunk(Vec<u8>),
	/// The writer was closed and every chunk returned: the stream closes.
	Done,
	/// The writer was dropped before it closed, by a download that errored or was aborted: the
	/// stream is aborted, and what was gathered of the frames since the last chunk is dropped.
	Dropped,
}

/// The frames a [`StreamWriter`] sends, gathered into the chunks the stream is written in: small
/// frames (a flush) wait for more, while a frame that is big enough on its own is a chunk as it
/// came, without copying it.
struct Frames {
	/// `None` once the writer's [`WriteFrame::Done`] came.
	receiver: Option<mpsc::Receiver<WriteFrame>>,
	gathered: Vec<u8>,
}

impl Frames {
	fn new(receiver: mpsc::Receiver<WriteFrame>) -> Self {
		Self {
			receiver: Some(receiver),
			gathered: Vec::new(),
		}
	}

	async fn next(&mut self) -> Gathered {
		let Some(receiver) = &mut self.receiver else {
			return Gathered::Done;
		};
		loop {
			match receiver.recv().await {
				Some(WriteFrame::Data(data)) => {
					let data = if self.gathered.is_empty() {
						data
					} else {
						self.gathered.extend_from_slice(&data);
						mem::take(&mut self.gathered)
					};
					if data.len() >= MAX_BUFFER_SIZE_BEFORE_FLUSH {
						return Gathered::Chunk(data);
					}
					self.gathered = data;
				}
				Some(WriteFrame::Done) => {
					self.receiver = None;
					if self.gathered.is_empty() {
						return Gathered::Done;
					}
					return Gathered::Chunk(mem::take(&mut self.gathered));
				}
				None => return Gathered::Dropped,
			}
		}
	}

	/// Writes the chunks to `stream` on this thread, and answers how the stream ended.
	#[cfg(not(feature = "wasm-full"))]
	async fn write_to<P: Fn(u64)>(mut self, mut stream: JsStream<P>) -> Result<(), Error> {
		loop {
			match self.next().await {
				Gathered::Chunk(data) => stream.write(Uint8Array::from(data.as_slice())).await?,
				Gathered::Done => return Ok(stream.close().await?),
				Gathered::Dropped => {
					stream.abort().await;
					return Err(producer_dropped());
				}
			}
		}
	}
}

/// The commander's half of a [`stream_writer`] and the stream's thread's half, which talk over a
/// `MessageChannel`: the commander orders a write, the close or the abort, one order at a time, and
/// the stream's thread answers each once it did it.
#[cfg(feature = "wasm-full")]
mod cross_thread {
	use futures::{
		StreamExt,
		channel::mpsc::{UnboundedReceiver, unbounded},
	};
	use serde::{Deserialize, Serialize};
	use wasm_bindgen::{JsCast, JsValue, closure::Closure};
	use web_sys::{MessageChannel, MessageEvent, MessagePort, js_sys::Uint8Array};

	use super::{Frames, Gathered, JsStream, StreamFailure, producer_dropped};
	use crate::{Error, ErrorKind, runtime::PortRequest};

	/// What the commander orders the stream's thread to do.
	enum Order {
		/// Hand the chunk to the stream.
		Write(Uint8Array),
		Close,
		Abort,
	}

	impl Order {
		/// Posts this to `port`, transferring a chunk's buffer rather than copying it.
		fn post(self, port: &MessagePort) -> Result<(), JsValue> {
			match self {
				Self::Write(chunk) => port.post_message_with_transferable(
					&chunk,
					&web_sys::js_sys::Array::of1(&chunk.buffer()),
				),
				Self::Close => port.post_message(&JsValue::from_str("close")),
				Self::Abort => port.post_message(&JsValue::from_str("abort")),
			}
		}

		/// Reads what [`post`](Self::post) sent. Anything but a chunk or the close is the abort,
		/// which never leaves a truncated file kept as complete.
		fn from_js(value: JsValue) -> Self {
			match value.dyn_into::<Uint8Array>() {
				Ok(chunk) => Self::Write(chunk),
				Err(value) if value.as_string().as_deref() == Some("close") => Self::Close,
				Err(_) => Self::Abort,
			}
		}
	}

	/// What the stream's thread answers an order with, once it did it.
	#[derive(Serialize, Deserialize)]
	enum Answer {
		/// The chunk was handed to the stream.
		Written,
		Closed,
		Aborted,
		/// The stream failed, with the message of a [`StreamFailure`].
		Failed(String),
	}

	/// The messages a port gets, in the order they came, on this thread.
	struct PortMessages {
		port: MessagePort,
		messages: UnboundedReceiver<JsValue>,
		/// Called by the port with each message for as long as this holds it.
		_on_message: Closure<dyn FnMut(MessageEvent)>,
	}

	impl PortMessages {
		fn new(port: MessagePort) -> Self {
			let (sender, messages) = unbounded();
			let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
				// fails only once this was dropped, which takes the handler off the port first
				let _ = sender.unbounded_send(event.data());
			});
			// which also starts the port
			port.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
			Self {
				port,
				messages,
				_on_message: on_message,
			}
		}

		async fn next(&mut self) -> JsValue {
			self.messages.next().await.expect(
				"the port's handler, which this holds, holds the sender (should be impossible)",
			)
		}
	}

	impl Drop for PortMessages {
		fn drop(&mut self) {
			self.port.set_onmessage(None);
		}
	}

	impl Frames {
		/// Hands the chunks to the thread that made `request`, over a `MessageChannel` whose port
		/// goes there, gathering the next once that thread answered it wrote the one before.
		/// Answers how the stream ended.
		pub(super) async fn hand_over(mut self, request: PortRequest) -> Result<(), Error> {
			// Neither fails short of the browser refusing a new channel, or the transfer of a port
			// it just made, and the stream's thread is then never told: its stream stays open.
			let channel = MessageChannel::new()
				.map_err(|e| broken(format!("failed to open the stream's channel: {e:?}")))?;
			request
				.send(channel.port2())
				.map_err(|e| broken(format!("failed to send the stream's port: {e:?}")))?;
			let mut answers = PortMessages::new(channel.port1());
			let result = loop {
				let (order, expected) = match self.next().await {
					// copied out of wasm memory, into a buffer of the message's own
					Gathered::Chunk(data) => (
						Order::Write(Uint8Array::from(data.as_slice())),
						Expected::Written,
					),
					Gathered::Done => (Order::Close, Expected::Closed),
					Gathered::Dropped => (Order::Abort, Expected::Aborted),
				};
				if let Err(e) = order.post(&answers.port) {
					break Err(broken(format!(
						"failed to send the stream's thread an order: {e:?}"
					)));
				}
				let answer = serde_wasm_bindgen::from_value(answers.next().await).ok();
				match (expected, answer) {
					(Expected::Written, Some(Answer::Written)) => {}
					(Expected::Closed, Some(Answer::Closed)) => break Ok(()),
					(Expected::Aborted, Some(Answer::Aborted)) => break Err(producer_dropped()),
					(_, Some(Answer::Failed(message))) => break Err(StreamFailure(message).into()),
					// Any other pairing breaks the exchange. The abort keeps the stream from being
					// left open, should the stream's thread still take orders.
					(_, _) => {
						let _ = Order::Abort.post(&answers.port);
						break Err(broken(
							"the stream's thread answered out of turn".to_owned(),
						));
					}
				}
			};
			answers.port.close();
			result
		}
	}

	/// The answer an order expects.
	enum Expected {
		Written,
		Closed,
		Aborted,
	}

	/// The error for an exchange with the stream's thread that broke down, which only a browser
	/// refusing what a `MessageChannel` takes can do.
	fn broken(message: String) -> Error {
		Error::custom(ErrorKind::Internal, message)
	}

	impl<P: Fn(u64)> JsStream<P> {
		/// Does what the commander orders over `port`, answering each order, until the close or
		/// the abort.
		pub(super) async fn write_orders(mut self, port: MessagePort) {
			let mut orders = PortMessages::new(port);
			let answer = loop {
				match Order::from_js(orders.next().await) {
					Order::Write(chunk) => match self.write(chunk).await {
						Ok(()) => post_answer(&orders.port, &Answer::Written),
						Err(failure) => break Answer::Failed(failure.0),
					},
					Order::Close => {
						break match self.close().await {
							Ok(()) => Answer::Closed,
							Err(failure) => Answer::Failed(failure.0),
						};
					}
					Order::Abort => {
						self.abort().await;
						break Answer::Aborted;
					}
				}
			};
			post_answer(&orders.port, &answer);
		}
	}

	fn post_answer(port: &MessagePort, answer: &Answer) {
		let posted = serde_wasm_bindgen::to_value(answer)
			.map_err(JsValue::from)
			.and_then(|answer| port.post_message(&answer));
		if let Err(e) = posted {
			// the commander then waits for good, and the call with it
			tracing::error!("failed to answer the commander for a stream: {e:?}");
		}
	}
}
