use chrono::{DateTime, Utc};
use filen_sdk_rs::fs::{
	HasParent, HasRemoteInfo, HasUUID,
	dir::{
		DecryptedDirectoryMeta, HasUUIDContents, RemoteDirectory, RootDirectory,
		UnsharedDirectoryType, meta::DirectoryMeta, traits::HasRemoteDirInfo,
	},
	file::{RemoteFile, meta::DecryptedFileMeta},
};
use filen_types::crypto::Sha512Hash;
use serde::{Deserialize, Serialize};
use tsify::Tsify;
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
use web_sys::js_sys::{self, Date};

trait WasmConvert {
	fn into_js_value(self) -> JsValue;
	fn from_js_value(value: JsValue) -> Self;
}

impl<T: WasmConvert> WasmConvert for Option<T> {
	fn into_js_value(self) -> JsValue {
		self.map_or(JsValue::UNDEFINED, |f| f.into_js_value())
	}

	fn from_js_value(value: JsValue) -> Self {
		if value.is_undefined() {
			None
		} else {
			Some(T::from_js_value(value))
		}
	}
}

impl WasmConvert for DateTime<Utc> {
	fn into_js_value(self) -> JsValue {
		Date::from(self).into()
	}

	fn from_js_value(value: JsValue) -> Self {
		let date = Date::from(value);
		DateTime::<Utc>::from(date)
	}
}

#[derive(Tsify, Serialize)]
#[tsify(into_wasm_abi)]
pub struct FileMeta {
	pub name: String,
	pub mime: String,
	// will need to consider if these are worth it or if we need to revert to timestamps
	// depending on how long serialization takes
	#[tsify(type = "Date | undefined")]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub created: JsValue,
	#[tsify(type = "Date")]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub modified: JsValue,
	#[tsify(type = "Uint8Array | undefined")]
	pub hash: Option<Sha512Hash>,
}

impl FileMeta {
	fn from_ref(meta: &DecryptedFileMeta) -> Self {
		FileMeta {
			name: meta.name.to_string(),
			mime: meta.mime.to_string(),
			created: meta.created.into_js_value(),
			modified: meta.last_modified.into_js_value(),
			hash: meta.hash,
		}
	}
}

#[wasm_bindgen]
pub struct File(pub(crate) RemoteFile);

#[wasm_bindgen]
impl File {
	#[wasm_bindgen(getter)]
	pub fn uuid(&self) -> String {
		self.0.uuid.to_string()
	}

	#[wasm_bindgen(getter)]
	pub fn parent(&self) -> String {
		self.0.parent.to_string()
	}

	#[wasm_bindgen(getter)]
	pub fn size(&self) -> u64 {
		self.0.size
	}

	#[wasm_bindgen(getter)]
	pub fn favorited(&self) -> bool {
		self.0.favorited
	}

	#[wasm_bindgen(getter)]
	pub fn meta(&self) -> Option<FileMeta> {
		match &self.0.meta {
			filen_sdk_rs::fs::file::meta::FileMeta::Decoded(meta) => Some(FileMeta::from_ref(meta)),
			_ => None,
		}
	}
}

impl From<RemoteFile> for File {
	fn from(file: RemoteFile) -> Self {
		File(file)
	}
}

impl From<File> for RemoteFile {
	fn from(file: File) -> Self {
		file.0
	}
}

#[derive(Tsify, Serialize)]
#[tsify(into_wasm_abi)]
pub struct DirMeta {
	pub name: String,
	#[tsify(type = "Date | undefined")]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub created: JsValue,
}

impl DirMeta {
	fn from_ref(meta: &DecryptedDirectoryMeta) -> Self {
		DirMeta {
			name: meta.name.to_string(),
			created: meta.created().into_js_value(),
		}
	}
}

#[wasm_bindgen]
pub struct Dir(pub(crate) UnsharedDirectoryType<'static>);

#[wasm_bindgen]
impl Dir {
	#[wasm_bindgen(getter)]
	pub fn uuid(&self) -> String {
		self.0.uuid().to_string()
	}

	#[wasm_bindgen(getter)]
	pub fn parent(&self) -> Option<String> {
		match self.0 {
			UnsharedDirectoryType::Dir(ref dir) => Some(dir.parent().to_string()),
			UnsharedDirectoryType::Root(_) => None,
		}
	}

	#[wasm_bindgen(getter)]
	pub fn color(&self) -> Option<String> {
		match self.0 {
			UnsharedDirectoryType::Dir(ref dir) => dir.color().map(|c| c.to_string()),
			UnsharedDirectoryType::Root(_) => None,
		}
	}

	#[wasm_bindgen(getter)]
	pub fn favorited(&self) -> bool {
		match self.0 {
			UnsharedDirectoryType::Dir(ref dir) => dir.favorited(),
			UnsharedDirectoryType::Root(_) => false,
		}
	}

	#[wasm_bindgen(getter)]
	pub fn meta(&self) -> Option<DirMeta> {
		match self.0 {
			UnsharedDirectoryType::Dir(ref dir) => match &dir.meta {
				DirectoryMeta::Decoded(meta) => Some(DirMeta::from_ref(meta)),
				_ => None,
			},
			UnsharedDirectoryType::Root(_) => None,
		}
	}
}

impl From<RemoteDirectory> for Dir {
	fn from(dir: RemoteDirectory) -> Self {
		Dir(dir.into())
	}
}

impl From<RootDirectory> for Dir {
	fn from(root: RootDirectory) -> Self {
		Dir(root.into())
	}
}

impl TryFrom<Dir> for RemoteDirectory {
	type Error = JsValue;

	fn try_from(dir: Dir) -> Result<Self, Self::Error> {
		match dir.0 {
			UnsharedDirectoryType::Dir(dir) => Ok(dir.into_owned()),
			UnsharedDirectoryType::Root(_) => Err(JsValue::from_str(
				"Cannot convert root directory to RemoteDirectory",
			)),
		}
	}
}

impl Dir {
	// pub(crate) fn try_ref(&self) -> Result<&RemoteDirectory, JsValue> {
	// 	match &self.0 {
	// 		UnsharedDirectoryType::Dir(dir) => Ok(dir),
	// 		UnsharedDirectoryType::Root(_) => Err(JsValue::from_str(
	// 			"Cannot convert root directory to RemoteDirectory",
	// 		)),
	// 	}
	// }

	pub(crate) fn try_mut_ref(&mut self) -> Result<&mut RemoteDirectory, JsValue> {
		match &mut self.0 {
			UnsharedDirectoryType::Dir(dir) => Ok(dir.to_mut()),
			UnsharedDirectoryType::Root(_) => Err(JsValue::from_str(
				"Cannot convert root directory to RemoteDirectory",
			)),
		}
	}
}

#[derive(Tsify, Deserialize)]
#[tsify(from_wasm_abi)]
pub struct UploadFileParams {
	pub name: String,
	#[tsify(type = "Date", optional)]
	#[serde(with = "serde_wasm_bindgen::preserve", default)]
	pub created: JsValue,
	#[tsify(type = "Date", optional)]
	#[serde(with = "serde_wasm_bindgen::preserve", default)]
	pub modified: JsValue,
	#[tsify(optional)]
	pub mime: Option<String>,
}

impl UploadFileParams {
	pub(crate) fn into_file_builder(
		self,
		client: &filen_sdk_rs::auth::Client,
		parent: &impl HasUUIDContents,
	) -> filen_sdk_rs::fs::file::FileBuilder {
		let mut file_builder = client.make_file_builder(self.name, parent);
		if let Some(mime) = self.mime {
			file_builder = file_builder.mime(mime);
		}
		let created = Option::<DateTime<Utc>>::from_js_value(self.created);
		let modified = Option::<DateTime<Utc>>::from_js_value(self.modified);
		match (created, modified) {
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

#[derive(Tsify, Deserialize)]
#[tsify(from_wasm_abi, large_number_types_as_bigints)]
pub struct UploadFileStreamParams {
	#[serde(flatten)]
	pub file_params: UploadFileParams,
	#[tsify(type = "ReadableStream<Uint8Array>")]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub reader: web_sys::ReadableStream,
	#[tsify(optional)]
	pub known_size: Option<u64>,
	#[tsify(type = "(bytes: bigint) => void", optional)]
	#[serde(with = "serde_wasm_bindgen::preserve", default)]
	pub progress: js_sys::Function,
}

#[derive(Tsify, Deserialize)]
#[tsify(from_wasm_abi, large_number_types_as_bigints)]
pub struct DownloadFileStreamParams {
	#[tsify(type = "WritableStream<Uint8Array>")]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub writer: web_sys::WritableStream,
	#[tsify(type = "(bytes: bigint) => void", optional)]
	#[serde(with = "serde_wasm_bindgen::preserve", default)]
	pub progress: js_sys::Function,
}
