use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::{
	auth::Client,
	fs::{dir::UnsharedDirectoryType, file::FileBuilder},
	js::DirEnum,
};

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use crate::js::{File, Item};
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use tsify::Tsify;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use web_sys::js_sys::{self};

#[derive(Deserialize)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), derive(Tsify))]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	tsify(from_wasm_abi)
)]
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
	#[serde(skip_serializing_if = "Option::is_none", default)]
	pub mime: Option<String>,
}

#[cfg(feature = "node")]
super::napi_from_json_impl!(UploadFileParams);

impl UploadFileParams {
	pub(crate) fn into_file_builder(self, client: &Client) -> FileBuilder {
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
		file_builder
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
}
