use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::{
	auth::Client,
	fs::{
		NonRootFSObject,
		dir::UnsharedDirectoryType,
		file::{FileBuilder, RemoteFile},
	},
	js::{Dir, DirEnum, File, ManagedFuture},
};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use tsify::Tsify;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_bindgen::prelude::*;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use web_sys::js_sys::{self};

#[derive(Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(untagged)]
pub enum NonRootItem {
	File(File),
	Dir(Dir),
}

impl TryFrom<NonRootItem> for NonRootFSObject<'static> {
	type Error = <RemoteFile as TryFrom<File>>::Error;
	fn try_from(value: NonRootItem) -> Result<Self, Self::Error> {
		Ok(match value {
			NonRootItem::Dir(dir) => Self::Dir(Cow::Owned(dir.into())),
			NonRootItem::File(file) => Self::File(Cow::Owned(file.try_into()?)),
		})
	}
}

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify, Deserialize)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct FileBuilderParams {
	pub parent: DirEnum,
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

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify, Deserialize),
	tsify(from_wasm_abi),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[derive(Deserialize, Tsify)]
#[tsify(from_wasm_abi, large_number_types_as_bigints)]
#[serde(rename_all = "camelCase")]
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

#[derive(Deserialize, Debug)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(from_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct LoginParams {
	pub email: String,
	pub password: String,
	#[serde(default)]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "string")
	)]
	#[cfg_attr(
		feature = "uniffi",
		uniffi(default = None)
	)]
	pub two_factor_code: Option<String>,
}
