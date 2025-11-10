#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use crate::js::ManagedFuture;
use crate::{
	Error, ErrorKind,
	fs::{file::service_worker::MAX_BUFFER_SIZE_BEFORE_FLUSH, zip::ZipProgressCallback},
	js::{FileEnum, Item},
};

use futures::AsyncWriteExt;
use serde::Deserialize;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_bindgen::JsValue;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use web_sys::js_sys::{BigInt, Function};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[derive(Deserialize, tsify::Tsify)]
#[tsify(from_wasm_abi, large_number_types_as_bigints)]
#[serde(rename_all = "camelCase")]
pub struct DownloadFileStreamParams {
	pub file: FileEnum,
	#[tsify(type = "WritableStream<Uint8Array>")]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub writer: web_sys::WritableStream,
	#[tsify(type = "(bytes: bigint) => void")]
	#[serde(default, with = "serde_wasm_bindgen::preserve")]
	pub progress: web_sys::js_sys::Function,
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

#[wasm_bindgen::prelude::wasm_bindgen]
unsafe extern "C" {
	#[wasm_bindgen::prelude::wasm_bindgen(extends = Function, is_type_of = JsValue::is_function, typescript_type = "(bytesWritten: bigint, totalBytes: bigint, itemsProcessed: bigint, totalItems: bigint) => void")]
	pub type ZipProgressCallbackJS;
	#[wasm_bindgen::prelude::wasm_bindgen(method, catch, js_name = call)]
	pub unsafe fn call4(
		this: &ZipProgressCallbackJS,
		context: &JsValue,
		arg1: &JsValue,
		arg2: &JsValue,
		arg3: &JsValue,
		arg4: &JsValue,
	) -> Result<JsValue, JsValue>;
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl Default for ZipProgressCallbackJS {
	fn default() -> Self {
		wasm_bindgen::JsCast::unchecked_into(JsValue::undefined())
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl ZipProgressCallbackJS {
	pub(crate) fn into_rust_callback(self) -> Option<impl ZipProgressCallback> {
		if self.is_undefined() {
			None
		} else {
			let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

			wasm_bindgen_futures::spawn_local(async move {
				while let Some((bytes_written, files_dirs_written, bytes_total, files_dirs_total)) =
					receiver.recv().await
				{
					let _ = unsafe {
						self.call4(
							&JsValue::NULL,
							&BigInt::from(bytes_written).into(),
							&BigInt::from(files_dirs_written).into(),
							&BigInt::from(bytes_total).into(),
							&BigInt::from(files_dirs_total).into(),
						)
					};
				}
			});
			Some(
				move |bytes_written: u64,
				      files_dirs_written: u64,
				      bytes_total: u64,
				      files_dirs_total: u64| {
					let _ = sender.send((
						bytes_written,
						files_dirs_written,
						bytes_total,
						files_dirs_total,
					));
				},
			)
		}
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[derive(Deserialize, tsify::Tsify)]
#[tsify(from_wasm_abi)]
#[serde(rename_all = "camelCase")]
pub struct DownloadFileToZipParams {
	pub items: Vec<Item>,
	#[tsify(type = "WritableStream<Uint8Array>")]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub writer: web_sys::WritableStream,
	#[serde(default, with = "serde_wasm_bindgen::preserve")]
	#[tsify(
		type = "(bytesWritten: bigint, totalBytes: bigint, itemsProcessed: bigint, totalItems: bigint) => void"
	)]
	pub progress: ZipProgressCallbackJS,
	// swap to flatten when https://github.com/madonoharu/tsify/issues/68 is resolved
	// #[serde(flatten)]
	#[serde(default)]
	pub managed_future: ManagedFuture,
}

pub(crate) fn spawn_buffered_write_future(
	mut data_receiver: tokio::sync::mpsc::Receiver<Vec<u8>>,
	mut writer: wasm_streams::writable::IntoAsyncWrite<'static>,
	progress_callback: Option<impl Fn(u64) + 'static>,
	result_sender: tokio::sync::oneshot::Sender<Result<(), Error>>,
) {
	wasm_bindgen_futures::spawn_local(async move {
		let mut local_cache = Vec::with_capacity(1024);
		let mut read = 0u64;

		while let Some(data) = data_receiver.recv().await {
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
