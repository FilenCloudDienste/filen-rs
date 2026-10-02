#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use crate::js::{AnyFile, ManagedFuture};
use crate::{
	Error, ErrorKind,
	fs::file::service_worker::{MAX_BUFFER_SIZE_BEFORE_FLUSH, StreamWriter, WriteFrame},
};

use filen_macros::js_type;
use futures::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::js_sys;

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

/// Bridges `stream` to a `Send` writer: a task on this thread owns the stream, writes what the
/// [`StreamWriter`] flushes, closes it once the writer is closed and aborts it when the writer
/// is dropped first. The receiver answers once the stream is closed, failed or aborted. A stream
/// that cannot be written fails with `Conversion`, its message starting with `conversion_failure`.
pub(crate) fn stream_writer(
	stream: web_sys::WritableStream,
	progress: Option<impl Fn(u64) + 'static>,
	conversion_failure: &'static str,
) -> Result<(StreamWriter, oneshot::Receiver<Result<(), Error>>), Error> {
	let writer = wasm_streams::WritableStream::from_raw(stream)
		.try_into_async_write()
		.map_err(|(e, _)| {
			Error::custom(
				ErrorKind::Conversion,
				format!("{conversion_failure}: {e:?}"),
			)
		})?;
	let (data_sender, data_receiver) = mpsc::channel(10);
	let (result_sender, result_receiver) = oneshot::channel();
	spawn_buffered_write_future(data_receiver, writer, progress, result_sender);
	Ok((StreamWriter::new(data_sender), result_receiver))
}

fn spawn_buffered_write_future(
	mut data_receiver: tokio::sync::mpsc::Receiver<WriteFrame>,
	mut writer: wasm_streams::writable::IntoAsyncWrite<'static>,
	progress_callback: Option<impl Fn(u64) + 'static>,
	result_sender: tokio::sync::oneshot::Sender<Result<(), Error>>,
) {
	wasm_bindgen_futures::spawn_local(async move {
		let mut local_cache = Vec::with_capacity(1024);
		let mut read = 0u64;
		let mut completed = false;

		while let Some(frame) = data_receiver.recv().await {
			let data = match frame {
				WriteFrame::Data(data) => data,
				WriteFrame::Done => {
					completed = true;
					break;
				}
			};
			local_cache.extend_from_slice(&data);

			if local_cache.len() < MAX_BUFFER_SIZE_BEFORE_FLUSH {
				continue;
			}
			if let Err(e) = writer.write(&local_cache).await {
				let _ = result_sender.send(Err(Error::custom(
					ErrorKind::IO,
					format!("error writing to stream: {:?}", e),
				)));
				return;
			}
			if let Some(callback) = &progress_callback {
				read += local_cache.len() as u64;
				callback(read);
			}
			local_cache.clear();
		}

		if !completed {
			// The sender was dropped without a Done frame: the producing download
			// errored or was aborted mid-stream. Abort the stream so the JS side
			// discards the partial data instead of saving a truncated file as
			// complete.
			let _ = writer
				.abort_with_reason(&JsValue::from_str("download did not complete"))
				.await;
			let _ = result_sender.send(Err(Error::custom(
				ErrorKind::Cancelled,
				"stream producer dropped before completing",
			)));
			return;
		}

		if !local_cache.is_empty() {
			if let Err(e) = writer.write(&local_cache).await {
				let _ = result_sender.send(Err(Error::custom(
					ErrorKind::IO,
					format!("error writing to stream: {:?}", e),
				)));
				return;
			}
			if let Some(callback) = &progress_callback {
				read += local_cache.len() as u64;
				callback(read);
			}
		}

		if let Err(e) = writer.close().await {
			let _ = result_sender.send(Err(Error::custom(
				ErrorKind::IO,
				format!("error closing stream: {:?}", e),
			)));
			return;
		}
		let _ = result_sender.send(Ok(()));
	});
}
