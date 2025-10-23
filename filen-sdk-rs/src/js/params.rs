use std::borrow::Cow;

use serde::Deserialize;

use crate::{
	auth::Client,
	fs::{
		NonRootFSObject,
		dir::UnsharedDirectoryType,
		file::{FileBuilder, RemoteFile},
	},
	js::{DateTime, Dir, DirEnum, File},
};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use crate::{fs::zip::ZipProgressCallback, js::ManagedFuture, runtime};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use crate::js::{FileEnum, Item};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use tsify::Tsify;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_bindgen::prelude::*;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use web_sys::js_sys::{self, BigInt, Function};

#[derive(Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi)
)]
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

#[derive(Deserialize)]
#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), derive(Tsify))]
pub struct FileBuilderParams {
	pub parent: DirEnum,
	pub name: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	#[serde(with = "filen_types::serde::time::optional", default)]
	pub created: Option<DateTime>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	#[serde(with = "filen_types::serde::time::optional", default)]
	pub modified: Option<DateTime>,
	#[serde(default)]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "string")
	)]
	pub mime: Option<String>,
}

#[derive(Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi)
)]
#[serde(rename_all = "camelCase")]
pub struct UploadFileParams {
	#[serde(flatten)]
	pub file_builder_params: FileBuilderParams,
	// swap to flatten when https://github.com/madonoharu/tsify/issues/68 is resolved
	// #[serde(flatten)]
	#[serde(default)]
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
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

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[derive(Deserialize, Tsify)]
#[tsify(from_wasm_abi, large_number_types_as_bigints)]
#[serde(rename_all = "camelCase")]
pub struct DownloadFileStreamParams {
	pub file: FileEnum,
	#[tsify(type = "WritableStream<Uint8Array>")]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub writer: web_sys::WritableStream,
	#[tsify(type = "(bytes: bigint) => void")]
	#[serde(default, with = "serde_wasm_bindgen::preserve")]
	pub progress: js_sys::Function,
	#[serde(default)]
	#[tsify(type = "bigint")]
	pub start: Option<u64>,
	#[serde(default)]
	#[tsify(type = "bigint")]
	pub end: Option<u64>,
	// swap to flatten when https://github.com/madonoharu/tsify/issues/68 is resolved
	// #[serde(flatten)]
	#[serde(default)]
	pub managed_future: ManagedFuture,
}

#[wasm_bindgen]
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
unsafe extern "C" {
	#[wasm_bindgen(extends = Function, is_type_of = JsValue::is_function, typescript_type = "(bytesWritten: bigint, totalBytes: bigint, itemsProcessed: bigint, totalItems: bigint) => void")]
	pub type ZipProgressCallbackJS;
	#[wasm_bindgen(method, catch, js_name = call)]
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
		JsValue::undefined().unchecked_into()
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl ZipProgressCallbackJS {
	pub(crate) fn into_rust_callback(self) -> Option<impl ZipProgressCallback> {
		if self.is_undefined() {
			None
		} else {
			let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

			runtime::spawn_local(async move {
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
#[derive(Deserialize, Tsify)]
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
