use chrono::{DateTime, Utc};
use filen_macros::js_type;

use crate::{
	Error,
	auth::Client,
	fs::{
		HasUUID,
		categories::{DirType, Normal},
		file::{FileBuilder, FileBuilderOptionalName},
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
	/// If true, skip EXIF / track-metadata parsing entirely during upload.
	/// Defaults to false (EXIF parsing runs when MIME matches image/video/audio).
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(default))]
	#[cfg_attr(feature = "uniffi", uniffi(default = false))]
	pub no_exif: bool,
	/// If true, EXIF-parsed times will NOT override caller-supplied
	/// `created` / `modified`. Defaults to false (EXIF overrides by default).
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(default))]
	#[cfg_attr(feature = "uniffi", uniffi(default = false))]
	pub no_exif_override: bool,
}

#[js_type(import, no_ser)]
pub struct FileBuilderParamsOptionalName {
	pub parent: AnyNormalDir,
	pub name: Option<String>,
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
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(default))]
	#[cfg_attr(feature = "uniffi", uniffi(default = false))]
	pub no_exif: bool,
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(default))]
	#[cfg_attr(feature = "uniffi", uniffi(default = false))]
	pub no_exif_override: bool,
}

impl TryFrom<FileBuilderParamsOptionalName> for FileBuilderOptionalName {
	type Error = Error;
	fn try_from(params: FileBuilderParamsOptionalName) -> Result<Self, Self::Error> {
		let dir = DirType::<Normal>::from(params.parent);

		let mut builder = FileBuilderOptionalName::new(dir.uuid());
		if let Some(name) = params.name {
			builder.name(&name)?;
		}
		if let Some(mime) = params.mime {
			builder.mime(mime);
		}
		if let Some(created) = params.created {
			builder.created(created);
		}
		if let Some(modified) = params.modified {
			builder.modified(modified);
		}
		if params.no_exif {
			builder.no_exif();
		}
		if params.no_exif_override {
			builder.no_exif_override();
		}
		Ok(builder)
	}
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
	pub(crate) fn into_file_builder(self, client: &Client) -> Result<FileBuilder, Error> {
		let mut file_builder = client.make_file_builder(
			&self.name,
			DirType::<'static, Normal>::from(self.parent).uuid(),
		)?;
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
		if self.no_exif {
			file_builder = file_builder.no_exif();
		}
		if self.no_exif_override {
			file_builder = file_builder.no_exif_override();
		}
		Ok(file_builder)
	}
}

#[cfg(feature = "wasm-full")]
#[js_type(import, no_ser, no_default)]
pub struct UploadFileStreamParams {
	#[serde(flatten)]
	pub file_builder_params: FileBuilderParams,
	#[tsify(type = "ReadableStream<Uint8Array>")]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub reader: web_sys::ReadableStream,
	pub known_size: Option<u64>,
	#[tsify(type = "(bytes: bigint) => void", optional)]
	#[serde(default, with = "serde_wasm_bindgen::preserve")]
	pub progress: js_sys::Function,
	// Direct (non-flattened) field so serde_wasm_bindgen::preserve keeps the abort/pause
	// signals as live JS references. Flattening buffers the params into a serde map, which
	// strips the preserved values — the download stream params keep it direct for the same reason.
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(default))]
	pub managed_future: ManagedFuture,
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
