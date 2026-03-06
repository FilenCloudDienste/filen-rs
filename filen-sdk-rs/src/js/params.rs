use chrono::{DateTime, Utc};
use filen_macros::js_type;

use crate::{
	auth::Client,
	fs::{
		HasUUID,
		categories::{DirType, Normal},
		file::FileBuilder,
	},
	js::{AnyNormalDir, ManagedFuture},
};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use {
	wasm_bindgen::prelude::*,
	web_sys::js_sys::{self},
};

#[js_type(import, no_ser)]
pub struct FileBuilderParams {
	pub parent: AnyNormalDir,
	pub name: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint"),
		serde(with = "filen_types::serde::time::optional", default)
	)]
	pub created: Option<DateTime<Utc>>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint"),
		serde(with = "filen_types::serde::time::optional", default)
	)]
	pub modified: Option<DateTime<Utc>>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "string"),
		serde(default)
	)]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub mime: Option<String>,
}

#[js_type(import, no_ser, no_default)]
pub struct UploadFileParams {
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(flatten))]
	pub file_builder_params: FileBuilderParams,
	// swap to flatten when https://github.com/madonoharu/tsify/issues/68 is resolved
	// #[serde(flatten)]
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(default))]
	pub managed_future: ManagedFuture,
}

impl FileBuilderParams {
	pub(crate) fn into_file_builder(self, client: &Client) -> FileBuilder {
		let mut file_builder = client.make_file_builder(
			self.name,
			*DirType::<'static, Normal>::from(self.parent).uuid(),
		);
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

#[cfg(feature = "wasm-full")]
#[js_type(import, no_ser, no_default)]
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

#[js_type(import)]
pub struct LoginParams {
	pub email: String,
	pub password: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		serde(default),
		tsify(type = "string")
	)]
	#[cfg_attr(
		feature = "uniffi",
		uniffi(default = None)
	)]
	pub two_factor_code: Option<String>,
}
