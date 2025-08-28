use std::{borrow::Cow, pin::Pin};

use chrono::{DateTime, Utc};
use futures::future;
use serde::Deserialize;
use wasm_bindgen::{JsCast, JsValue, prelude::Closure};
use web_sys::{AbortSignal as WasmAbortSignal, js_sys::JsString};

use crate::{
	Error, ErrorKind,
	auth::Client,
	error::AbortedError,
	fs::{dir::UnsharedDirectoryType, file::FileBuilder},
	js::DirEnum,
};

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use crate::js::{File, Item};
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use tsify::Tsify;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use web_sys::js_sys::{self};

#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	derive(Tsify, Debug)
)]
#[derive(Deserialize, Default)]
#[serde(transparent)]
pub struct AbortSignal(
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "AbortSignal")
	)]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub JsValue,
);

impl AbortSignal {
	pub(crate) fn into_future(self) -> Result<Pin<Box<dyn Future<Output = AbortedError>>>, Error> {
		let signal: Option<WasmAbortSignal> = if self.0.is_undefined() {
			None
		} else {
			Some(self.0.dyn_into().map_err(|e| {
				let ty = JsValue::dyn_ref::<JsString>(&e.js_typeof())
					.map(|s| Cow::Owned(String::from(s)))
					.unwrap_or(Cow::Borrowed("unknown"));
				Error::custom(
					ErrorKind::Conversion,
					format!("expected AbortSignal, got {}", ty),
				)
			})?)
		};
		Ok(match signal {
			None => Box::pin(future::pending()),
			Some(abort_signal) => {
				let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
				let closure = Closure::once(move || {
					let _ = sender.send(());
				});
				abort_signal.set_onabort(Some(closure.as_ref().unchecked_ref()));
				Box::pin(async move {
					if abort_signal.aborted() {
						log::debug!("AbortSignal already aborted, returning AbortedError");
						return AbortedError;
					}
					let _closure = closure; // keep the closure alive
					let _ = receiver.await;
					log::debug!("AbortSignal aborted, returning AbortedError");
					AbortedError
				})
			}
		})
	}
}

#[derive(Deserialize)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), derive(Tsify))]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	tsify(from_wasm_abi)
)]
#[serde(rename_all = "camelCase")]
pub struct UploadFileParams {
	pub parent: DirEnum,
	pub name: String,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	#[serde(with = "filen_types::serde::time::optional", default)]
	pub created: Option<DateTime<Utc>>,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	#[serde(with = "filen_types::serde::time::optional", default)]
	pub modified: Option<DateTime<Utc>>,
	#[serde(default)]
	pub mime: Option<String>,
	#[serde(default)]
	pub abort_signal: AbortSignal,
}

#[cfg(feature = "node")]
super::napi_from_json_impl!(UploadFileParams);

impl UploadFileParams {
	pub(crate) fn into_file_builder(self, client: &Client) -> (FileBuilder, AbortSignal) {
		let mut file_builder =
			client.make_file_builder(self.name, &UnsharedDirectoryType::from(self.parent));
		if let Some(mime) = self.mime {
			file_builder = file_builder.mime(mime);
		}
		match (self.created, self.modified) {
			(Some(created), Some(modified)) => {
				file_builder = file_builder.created(created).modified(modified)
			}
			(Some(created), None) => file_builder = file_builder.created(created),
			(None, Some(modified)) => {
				file_builder = file_builder.modified(modified).created(modified)
			}
			(None, None) => {}
		};
		(file_builder, self.abort_signal)
	}
}

// not sure how the streams are handled in napi, so just excluding these from napi for now
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[derive(Deserialize, Tsify)]
#[tsify(from_wasm_abi, large_number_types_as_bigints)]
pub struct UploadFileStreamParams {
	#[serde(flatten)]
	pub file_params: UploadFileParams,
	#[tsify(type = "ReadableStream<Uint8Array>")]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub reader: web_sys::ReadableStream,
	pub known_size: Option<u64>,
	#[tsify(type = "(bytes: bigint) => void", optional)]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub progress: js_sys::Function,
}

// #[derive(Deserialize)]
// pub struct UploadFileStreamParams<'a> {
// 	#[serde(flatten)]
// 	pub file_params: UploadFileParams,
// 	pub reader: web_sys::ReadableStream,
// 	pub known_size: Option<u64>,
// 	pub progress: Arc<ThreadsafeFunction<u64, ()>>,
// }

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[derive(Deserialize)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), derive(Tsify))]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	tsify(from_wasm_abi, large_number_types_as_bigints)
)]
pub struct DownloadFileStreamParams {
	pub file: File,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "WritableStream<Uint8Array>")
	)]
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		serde(with = "serde_wasm_bindgen::preserve")
	)]
	pub writer: web_sys::WritableStream,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "(bytes: bigint) => void")
	)]
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		serde(with = "serde_wasm_bindgen::preserve")
	)]
	#[serde(default)]
	pub progress: js_sys::Function,
	#[serde(default)]
	pub abort_signal: AbortSignal,
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[derive(Deserialize)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), derive(Tsify))]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	tsify(from_wasm_abi)
)]
pub struct DownloadFileToZipParams {
	pub items: Vec<Item>,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "WritableStream<Uint8Array>")
	)]
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		serde(with = "serde_wasm_bindgen::preserve")
	)]
	pub writer: web_sys::WritableStream,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "(bytes: bigint) => void")
	)]
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		serde(with = "serde_wasm_bindgen::preserve")
	)]
	#[serde(default)]
	// ignored for now, as the zip writer doesn't currently support progress updates
	pub progress: js_sys::Function,
	#[serde(default)]
	pub abort_signal: AbortSignal,
}
