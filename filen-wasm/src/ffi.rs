use std::{borrow::Cow, cell::RefCell, collections::HashMap};

use chrono::{DateTime, Utc};
use filen_sdk_rs::{
	crypto::file::FileKey,
	fs::{
		HasUUID,
		dir::{
			DecryptedDirectoryMeta as SDKDecryptedDirMeta, RemoteDirectory, RootDirectory,
			UnsharedDirectoryType, meta::DirectoryMeta,
		},
		file::{
			RemoteFile,
			meta::{DecryptedFileMeta as SDKDecryptedFileMeta, FileMeta as SDKFileMeta},
		},
	},
};
use filen_types::{
	auth::FileEncryptionVersion,
	crypto::{EncryptedString, Sha512Hash, rsa::RSAEncryptedString},
	fs::{ParentUuid, UuidStr},
};
use serde::{Deserialize, Deserializer, Serialize, ser::SerializeStruct};
use tsify::Tsify;
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
use web_sys::js_sys::{self};

thread_local! {
	static KEY_TO_JS_VALUE: RefCell<HashMap<&'static str, JsValue>> = RefCell::new(HashMap::new());
}

const HIDDEN_META_KEY: &str = "__hiddenMeta";

fn diff_apply_value<T>(
	key: &'static str,
	old_value: &T,
	new_value: T,
	apply_to: &JsValue,
) -> Result<(), JsValue>
where
	JsValue: From<T>,
	JsValue: From<&'static str>,
	T: PartialEq,
{
	if *old_value != new_value {
		KEY_TO_JS_VALUE.with_borrow_mut(|map| {
			let value = JsValue::from(new_value);
			let key = map.entry(key).or_insert_with(|| JsValue::from(key));
			js_sys::Reflect::set(apply_to, key, &value)?;
			Ok::<(), JsValue>(())
		})?;
	}
	Ok(())
}

#[derive(Tsify, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[tsify(large_number_types_as_bigints)]
#[cfg_attr(test, derive(Debug))]
pub struct DecryptedFileMeta {
	pub name: String,
	pub mime: String,
	#[tsify(type = "bigint")]
	#[serde(
		with = "filen_types::serde::time::optional",
		skip_serializing_if = "Option::is_none",
		default
	)]
	pub created: Option<DateTime<Utc>>,
	#[tsify(type = "bigint")]
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub modified: DateTime<Utc>,
	#[tsify(type = "Uint8Array")]
	#[serde(skip_serializing_if = "Option::is_none", default)]
	pub hash: Option<Sha512Hash>,

	#[tsify(type = "bigint")]
	pub size: u64,
	pub key: String,
	pub version: FileEncryptionVersion,
}

impl From<SDKDecryptedFileMeta<'_>> for DecryptedFileMeta {
	fn from(meta: SDKDecryptedFileMeta) -> Self {
		DecryptedFileMeta {
			name: meta.name.into_owned(),
			mime: meta.mime.into_owned(),
			created: meta.created,
			modified: meta.last_modified,
			hash: meta.hash,
			size: meta.size,
			version: meta.key.version(),
			key: meta.key.to_str().into_owned(),
		}
	}
}

impl TryFrom<DecryptedFileMeta> for SDKDecryptedFileMeta<'static> {
	type Error = JsValue;
	fn try_from(meta: DecryptedFileMeta) -> Result<Self, Self::Error> {
		Ok(SDKDecryptedFileMeta {
			name: Cow::Owned(meta.name),
			mime: Cow::Owned(meta.mime),
			created: meta.created,
			last_modified: meta.modified,
			hash: meta.hash,
			size: meta.size,
			key: Cow::Owned(
				FileKey::from_str_with_version(&meta.key, meta.version)
					.map_err(|e| JsValue::from_str(&e.to_string()))?,
			),
		})
	}
}

#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(Debug))]
pub enum FileMeta {
	Decoded(DecryptedFileMeta),
	DecryptedUTF8(String),
	DecryptedRaw(Vec<u8>),
	Encrypted(EncryptedString),
	RSAEncrypted(RSAEncryptedString),
}

#[derive(Serialize, Deserialize)]
enum FileMetaEncoded<'a> {
	DecryptedRaw(Cow<'a, [u8]>),
	DecryptedUTF8(Cow<'a, str>),
	Encrypted(Cow<'a, EncryptedString>),
	RSAEncrypted(Cow<'a, RSAEncryptedString>),
}

impl From<SDKFileMeta<'_>> for FileMeta {
	fn from(meta: SDKFileMeta) -> Self {
		match meta {
			SDKFileMeta::Decoded(meta) => FileMeta::Decoded(meta.into()),
			SDKFileMeta::DecryptedUTF8(meta) => FileMeta::DecryptedUTF8(meta.into_owned()),
			SDKFileMeta::DecryptedRaw(meta) => FileMeta::DecryptedRaw(meta.into_owned()),
			SDKFileMeta::Encrypted(meta) => FileMeta::Encrypted(meta.into_owned()),
			SDKFileMeta::RSAEncrypted(meta) => FileMeta::RSAEncrypted(meta.into_owned()),
		}
	}
}

impl TryFrom<FileMeta> for SDKFileMeta<'static> {
	type Error = JsValue;
	fn try_from(meta: FileMeta) -> Result<Self, Self::Error> {
		Ok(match meta {
			FileMeta::Decoded(meta) => SDKFileMeta::Decoded(meta.try_into()?),
			FileMeta::DecryptedUTF8(meta) => SDKFileMeta::DecryptedUTF8(Cow::Owned(meta)),
			FileMeta::DecryptedRaw(meta) => SDKFileMeta::DecryptedRaw(Cow::Owned(meta)),
			FileMeta::Encrypted(meta) => SDKFileMeta::Encrypted(Cow::Owned(meta)),
			FileMeta::RSAEncrypted(meta) => SDKFileMeta::RSAEncrypted(Cow::Owned(meta)),
		})
	}
}

impl<'a> AsEncodedOrDecoded<'a, FileMetaEncoded<'a>, &'a DecryptedFileMeta> for FileMeta {
	fn as_encoded_or_decoded(
		&'a self,
	) -> EncodedOrDecoded<FileMetaEncoded<'a>, &'a DecryptedFileMeta> {
		match self {
			FileMeta::Decoded(meta) => EncodedOrDecoded::Decoded(meta),
			FileMeta::DecryptedRaw(data) => {
				EncodedOrDecoded::Encoded(FileMetaEncoded::DecryptedRaw(Cow::Borrowed(data)))
			}
			FileMeta::DecryptedUTF8(data) => {
				EncodedOrDecoded::Encoded(FileMetaEncoded::DecryptedUTF8(Cow::Borrowed(data)))
			}
			FileMeta::Encrypted(data) => {
				EncodedOrDecoded::Encoded(FileMetaEncoded::Encrypted(Cow::Borrowed(data)))
			}
			FileMeta::RSAEncrypted(data) => {
				EncodedOrDecoded::Encoded(FileMetaEncoded::RSAEncrypted(Cow::Borrowed(data)))
			}
		}
	}
}

#[derive(Tsify, Clone)]
#[tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub struct File {
	pub uuid: UuidStr,
	#[tsify(optional, type = "DecryptedFileMeta")]
	pub meta: FileMeta,

	pub parent: ParentUuid,
	#[tsify(type = "bigint")]
	pub size: u64,
	pub favorited: bool,

	pub region: String,
	pub bucket: String,
	#[tsify(type = "bigint")]
	pub chunks: u64,
}

impl Serialize for File {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let mut state = serializer.serialize_struct("File", 5)?;
		state.serialize_field("uuid", &self.uuid)?;
		state.serialize_field("parent", &self.parent)?;

		state.serialize_field("size", &self.size)?;
		state.serialize_field("favorited", &self.favorited)?;

		state.serialize_field("region", &self.region)?;
		state.serialize_field("bucket", &self.bucket)?;
		state.serialize_field("chunks", &self.chunks)?;

		let encoded_meta = match &self.meta {
			FileMeta::Decoded(meta) => {
				state.serialize_field("meta", &meta)?;
				None
			}
			FileMeta::DecryptedRaw(meta) => {
				Some(FileMetaEncoded::DecryptedRaw(Cow::Borrowed(meta)))
			}
			FileMeta::DecryptedUTF8(meta) => {
				Some(FileMetaEncoded::DecryptedUTF8(Cow::Borrowed(meta)))
			}
			FileMeta::Encrypted(meta) => Some(FileMetaEncoded::Encrypted(Cow::Borrowed(meta))),
			FileMeta::RSAEncrypted(meta) => {
				Some(FileMetaEncoded::RSAEncrypted(Cow::Borrowed(meta)))
			}
		};
		if let Some(encoded_meta) = encoded_meta {
			state.serialize_field(HIDDEN_META_KEY, &encoded_meta)?;
		}
		state.end()
	}
}

#[derive(Deserialize)]
struct FileIntermediate {
	uuid: UuidStr,
	parent: ParentUuid,

	size: u64,
	favorited: bool,

	region: String,
	bucket: String,
	chunks: u64,

	meta: Option<DecryptedFileMeta>,
	// HIDDEN_META_KEY
	#[serde(rename = "__hiddenMeta")]
	hidden_meta: Option<FileMetaEncoded<'static>>,
}

impl<'de> Deserialize<'de> for File {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let intermediate = FileIntermediate::deserialize(deserializer)?;

		// Handle meta field priority: decoded meta takes precedence over hidden meta
		let final_meta = if let Some(decoded_meta) = intermediate.meta {
			FileMeta::Decoded(decoded_meta)
		} else if let Some(encoded_meta) = intermediate.hidden_meta {
			match encoded_meta {
				FileMetaEncoded::DecryptedRaw(data) => FileMeta::DecryptedRaw(data.into_owned()),
				FileMetaEncoded::DecryptedUTF8(data) => FileMeta::DecryptedUTF8(data.into_owned()),
				FileMetaEncoded::Encrypted(data) => FileMeta::Encrypted(data.into_owned()),
				FileMetaEncoded::RSAEncrypted(data) => FileMeta::RSAEncrypted(data.into_owned()),
			}
		} else {
			// this doesn't need to be an allocation
			return Err(serde::de::Error::custom(format!(
				"either 'meta' or '{HIDDEN_META_KEY}' field is required"
			)));
		};

		Ok(File {
			uuid: intermediate.uuid,
			meta: final_meta,
			parent: intermediate.parent,
			size: intermediate.size,
			favorited: intermediate.favorited,
			region: intermediate.region,
			bucket: intermediate.bucket,
			chunks: intermediate.chunks,
		})
	}
}

impl wasm_bindgen::__rt::VectorIntoJsValue for File {
	fn vector_into_jsvalue(
		vector: wasm_bindgen::__rt::std::boxed::Box<[Self]>,
	) -> wasm_bindgen::JsValue {
		wasm_bindgen::__rt::js_value_vector_into_jsvalue(vector)
	}
}

impl From<RemoteFile> for File {
	fn from(file: RemoteFile) -> Self {
		File {
			uuid: file.uuid,
			meta: file.meta.into(),
			parent: file.parent,
			size: file.size,
			favorited: file.favorited,
			region: file.region,
			bucket: file.bucket,
			chunks: file.chunks,
		}
	}
}

impl TryFrom<File> for RemoteFile {
	type Error = JsValue;
	fn try_from(file: File) -> Result<Self, Self::Error> {
		Ok(RemoteFile {
			uuid: file.uuid,
			meta: file.meta.try_into()?,
			parent: file.parent,
			size: file.size,
			favorited: file.favorited,
			region: file.region,
			bucket: file.bucket,
			chunks: file.chunks,
		})
	}
}

fn diff_apply_meta<'a, 'b, T, U, V>(
	old: &'a T,
	new: &'a T,
	apply_to: &js_sys::Object,
) -> Result<(), JsValue>
where
	T: PartialEq<T> + AsEncodedOrDecoded<'a, U, V>,
	U: Serialize,
	V: Serialize,
	'a: 'b,
{
	if old != new {
		KEY_TO_JS_VALUE.with_borrow_mut(move |map| {
			// The reason we do this rather than using map.entry is because
			// there's no way to get 2 disjoint mutable references to entries in a HashMap
			// so we must insert first and then get the references
			if !map.contains_key("meta") {
				map.insert("meta", JsValue::from("meta"));
			}
			if !map.contains_key(HIDDEN_META_KEY) {
				map.insert(HIDDEN_META_KEY, JsValue::from(HIDDEN_META_KEY));
			}

			let old = old.as_encoded_or_decoded();
			let new = new.as_encoded_or_decoded();
			match (old, new) {
				(EncodedOrDecoded::Decoded(_), EncodedOrDecoded::Decoded(new)) => {
					// SAFETY: we know that the key exists because we inserted it above
					let key = unsafe { map.get("meta").unwrap_unchecked() };
					let value = serde_wasm_bindgen::to_value(&new)?;
					js_sys::Reflect::set(apply_to, key, &value)?;
				}
				(EncodedOrDecoded::Encoded(_), EncodedOrDecoded::Encoded(new)) => {
					// SAFETY: we know that the key exists because we inserted it above
					let key = unsafe { map.get(HIDDEN_META_KEY).unwrap_unchecked() };
					let value = serde_wasm_bindgen::to_value(&new)?;
					js_sys::Reflect::set(apply_to, key, &value)?;
				}
				(EncodedOrDecoded::Decoded(_), EncodedOrDecoded::Encoded(new)) => {
					// SAFETY: we know that the key exists because we inserted it above
					let old_key = unsafe { map.get("meta").unwrap_unchecked() };
					let key = unsafe { map.get(HIDDEN_META_KEY).unwrap_unchecked() };
					let value = serde_wasm_bindgen::to_value(&new)?;
					js_sys::Reflect::set(apply_to, key, &value)?;
					js_sys::Reflect::delete_property(apply_to, old_key)?;
				}
				(EncodedOrDecoded::Encoded(_), EncodedOrDecoded::Decoded(new)) => {
					// SAFETY: we know that the key exists because we inserted it above
					let old_key = unsafe { map.get(HIDDEN_META_KEY).unwrap_unchecked() };
					let key = unsafe { map.get("meta").unwrap_unchecked() };
					let value = serde_wasm_bindgen::to_value(&new)?;
					js_sys::Reflect::set(apply_to, key, &value)?;
					js_sys::Reflect::delete_property(apply_to, old_key)?;
				}
			}
			Ok::<(), JsValue>(())
		})?;
	}
	Ok(())
}

impl File {
	pub(crate) fn apply_diff(&self, other: Self, apply_to: &js_sys::Object) -> Result<(), JsValue> {
		diff_apply_value("uuid", &self.uuid, other.uuid, apply_to)?;
		diff_apply_value("parent", &self.parent, other.parent, apply_to)?;
		diff_apply_value("size", &self.size, other.size, apply_to)?;
		diff_apply_value("favorited", &self.favorited, other.favorited, apply_to)?;
		diff_apply_value("region", &self.region, other.region, apply_to)?;
		diff_apply_value("bucket", &self.bucket, other.bucket, apply_to)?;
		diff_apply_value("chunks", &self.chunks, other.chunks, apply_to)?;
		diff_apply_meta(&self.meta, &other.meta, apply_to)?;
		Ok(())
	}
}

#[derive(Tsify, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(Debug))]
#[tsify(large_number_types_as_bigints)]
pub struct DecryptedDirMeta {
	pub name: String,
	#[tsify(type = "bigint")]
	#[serde(
		with = "filen_types::serde::time::optional",
		skip_serializing_if = "Option::is_none",
		default
	)]
	pub created: Option<DateTime<Utc>>,
}

impl From<SDKDecryptedDirMeta<'_>> for DecryptedDirMeta {
	fn from(meta: SDKDecryptedDirMeta) -> Self {
		DecryptedDirMeta {
			name: meta.name.into_owned(),
			created: meta.created,
		}
	}
}

impl From<DecryptedDirMeta> for SDKDecryptedDirMeta<'static> {
	fn from(meta: DecryptedDirMeta) -> Self {
		SDKDecryptedDirMeta {
			name: Cow::Owned(meta.name),
			created: meta.created,
		}
	}
}

#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(Debug))]
pub enum DirMeta {
	Decoded(DecryptedDirMeta),
	DecryptedRaw(Vec<u8>),
	DecryptedUTF8(String),
	Encrypted(EncryptedString),
	RSAEncrypted(RSAEncryptedString),
}

#[derive(Serialize, Deserialize)]
enum DirMetaEncoded<'a> {
	DecryptedRaw(Cow<'a, [u8]>),
	DecryptedUTF8(Cow<'a, str>),
	Encrypted(Cow<'a, EncryptedString>),
	RSAEncrypted(Cow<'a, RSAEncryptedString>),
}

enum EncodedOrDecoded<T, U> {
	Encoded(T),
	Decoded(U),
}

trait AsEncodedOrDecoded<'a, T, U> {
	fn as_encoded_or_decoded(&'a self) -> EncodedOrDecoded<T, U>;
}

impl<'a> AsEncodedOrDecoded<'a, DirMetaEncoded<'a>, &'a DecryptedDirMeta> for DirMeta {
	fn as_encoded_or_decoded(
		&'a self,
	) -> EncodedOrDecoded<DirMetaEncoded<'a>, &'a DecryptedDirMeta> {
		match self {
			DirMeta::Decoded(meta) => EncodedOrDecoded::Decoded(meta),
			DirMeta::DecryptedRaw(data) => {
				EncodedOrDecoded::Encoded(DirMetaEncoded::DecryptedRaw(Cow::Borrowed(data)))
			}
			DirMeta::DecryptedUTF8(data) => {
				EncodedOrDecoded::Encoded(DirMetaEncoded::DecryptedUTF8(Cow::Borrowed(data)))
			}
			DirMeta::Encrypted(data) => {
				EncodedOrDecoded::Encoded(DirMetaEncoded::Encrypted(Cow::Borrowed(data)))
			}
			DirMeta::RSAEncrypted(data) => {
				EncodedOrDecoded::Encoded(DirMetaEncoded::RSAEncrypted(Cow::Borrowed(data)))
			}
		}
	}
}

impl From<DirectoryMeta<'_>> for DirMeta {
	fn from(meta: DirectoryMeta) -> Self {
		match meta {
			DirectoryMeta::Decoded(meta) => DirMeta::Decoded(meta.into()),
			DirectoryMeta::DecryptedRaw(meta) => DirMeta::DecryptedRaw(meta.into_owned()),
			DirectoryMeta::DecryptedUTF8(meta) => DirMeta::DecryptedUTF8(meta.into_owned()),
			DirectoryMeta::Encrypted(meta) => DirMeta::Encrypted(meta.into_owned()),
			DirectoryMeta::RSAEncrypted(meta) => DirMeta::RSAEncrypted(meta.into_owned()),
		}
	}
}

impl From<DirMeta> for DirectoryMeta<'static> {
	fn from(meta: DirMeta) -> Self {
		match meta {
			DirMeta::Decoded(meta) => DirectoryMeta::Decoded(meta.into()),
			DirMeta::DecryptedRaw(meta) => DirectoryMeta::DecryptedRaw(Cow::Owned(meta)),
			DirMeta::DecryptedUTF8(meta) => DirectoryMeta::DecryptedUTF8(Cow::Owned(meta)),
			DirMeta::Encrypted(meta) => DirectoryMeta::Encrypted(Cow::Owned(meta)),
			DirMeta::RSAEncrypted(meta) => DirectoryMeta::RSAEncrypted(Cow::Owned(meta)),
		}
	}
}

#[derive(Tsify, Serialize, Deserialize)]
#[tsify(into_wasm_abi, from_wasm_abi)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq, Clone))]
pub struct Root {
	pub uuid: UuidStr,
}

impl From<RootDirectory> for Root {
	fn from(dir: RootDirectory) -> Self {
		Root { uuid: dir.uuid() }
	}
}

impl From<Root> for RootDirectory {
	fn from(root: Root) -> Self {
		RootDirectory::new(root.uuid)
	}
}

#[derive(Tsify, Clone)]
#[tsify(into_wasm_abi, from_wasm_abi)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub struct Dir {
	pub uuid: UuidStr,
	pub parent: ParentUuid,
	#[serde(skip_serializing_if = "Option::is_none", default)]
	pub color: Option<String>,
	pub favorited: bool,
	#[tsify(optional, type = "DecryptedDirMeta")]
	pub meta: DirMeta,
}

impl Serialize for Dir {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let mut state = serializer.serialize_struct("Dir", 5)?;
		state.serialize_field("uuid", &self.uuid)?;
		state.serialize_field("parent", &self.parent)?;
		if let Some(color) = &self.color {
			state.serialize_field("color", color)?;
		}
		state.serialize_field("favorited", &self.favorited)?;
		let encoded_meta = match &self.meta {
			DirMeta::Decoded(meta) => {
				state.serialize_field("meta", &meta)?;
				None
			}
			DirMeta::DecryptedRaw(meta) => Some(DirMetaEncoded::DecryptedRaw(Cow::Borrowed(meta))),
			DirMeta::DecryptedUTF8(meta) => {
				Some(DirMetaEncoded::DecryptedUTF8(Cow::Borrowed(meta)))
			}
			DirMeta::Encrypted(meta) => Some(DirMetaEncoded::Encrypted(Cow::Borrowed(meta))),
			DirMeta::RSAEncrypted(meta) => Some(DirMetaEncoded::RSAEncrypted(Cow::Borrowed(meta))),
		};
		if let Some(encoded_meta) = encoded_meta {
			state.serialize_field(HIDDEN_META_KEY, &encoded_meta)?;
		}
		state.end()
	}
}

#[derive(Deserialize)]
struct DirIntermediate {
	uuid: UuidStr,
	parent: ParentUuid,
	color: Option<String>,
	favorited: bool,
	meta: Option<DecryptedDirMeta>,
	// HIDDEN_META_KEY
	#[serde(rename = "__hiddenMeta")]
	hidden_meta: Option<DirMetaEncoded<'static>>,
}

impl<'de> Deserialize<'de> for Dir {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let intermediate = DirIntermediate::deserialize(deserializer)?;

		// Handle meta field priority: decoded meta takes precedence over hidden meta
		let final_meta = if let Some(decoded_meta) = intermediate.meta {
			DirMeta::Decoded(decoded_meta)
		} else if let Some(encoded_meta) = intermediate.hidden_meta {
			match encoded_meta {
				DirMetaEncoded::DecryptedRaw(data) => DirMeta::DecryptedRaw(data.into_owned()),
				DirMetaEncoded::DecryptedUTF8(data) => DirMeta::DecryptedUTF8(data.into_owned()),
				DirMetaEncoded::Encrypted(data) => DirMeta::Encrypted(data.into_owned()),
				DirMetaEncoded::RSAEncrypted(data) => DirMeta::RSAEncrypted(data.into_owned()),
			}
		} else {
			// this doesn't need to be an allocation
			return Err(serde::de::Error::custom(format!(
				"either 'meta' or '{HIDDEN_META_KEY}' field is required"
			)));
		};

		Ok(Dir {
			uuid: intermediate.uuid,
			parent: intermediate.parent,
			color: intermediate.color,
			favorited: intermediate.favorited,
			meta: final_meta,
		})
	}
}

impl From<RemoteDirectory> for Dir {
	fn from(dir: RemoteDirectory) -> Self {
		Dir {
			uuid: dir.uuid,
			parent: dir.parent,
			color: dir.color,
			favorited: dir.favorited,
			meta: dir.meta.into(),
		}
	}
}

impl From<Dir> for RemoteDirectory {
	fn from(dir: Dir) -> Self {
		RemoteDirectory::from_meta(
			dir.uuid,
			dir.parent,
			dir.color,
			dir.favorited,
			dir.meta.into(),
		)
	}
}

impl Dir {
	pub(crate) fn apply_diff(&self, other: Self, apply_to: &js_sys::Object) -> Result<(), JsValue> {
		diff_apply_value("uuid", &self.uuid, other.uuid, apply_to)?;
		diff_apply_value("parent", &self.parent, other.parent, apply_to)?;
		diff_apply_value("color", &self.color, other.color, apply_to)?;
		diff_apply_value("favorited", &self.favorited, other.favorited, apply_to)?;
		diff_apply_meta(&self.meta, &other.meta, apply_to)?;
		Ok(())
	}
}

#[derive(Tsify, Serialize, Deserialize)]
#[tsify(into_wasm_abi, from_wasm_abi)]
#[serde(untagged)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub enum DirEnum {
	Dir(Dir),
	Root(Root),
}

impl wasm_bindgen::__rt::VectorIntoJsValue for DirEnum {
	fn vector_into_jsvalue(
		vector: wasm_bindgen::__rt::std::boxed::Box<[Self]>,
	) -> wasm_bindgen::JsValue {
		wasm_bindgen::__rt::js_value_vector_into_jsvalue(vector)
	}
}

impl From<RemoteDirectory> for DirEnum {
	fn from(dir: RemoteDirectory) -> Self {
		DirEnum::Dir(Dir::from(dir))
	}
}

impl From<UnsharedDirectoryType<'_>> for DirEnum {
	fn from(dir: UnsharedDirectoryType<'_>) -> Self {
		match dir {
			UnsharedDirectoryType::Root(root) => DirEnum::Root(Root::from(root.into_owned())),
			UnsharedDirectoryType::Dir(dir) => {
				let dir = dir.into_owned();
				DirEnum::Dir(Dir::from(dir))
			}
		}
	}
}

impl From<DirEnum> for UnsharedDirectoryType<'static> {
	fn from(dir: DirEnum) -> Self {
		match dir {
			DirEnum::Root(root) => UnsharedDirectoryType::Root(Cow::Owned(root.into())),
			DirEnum::Dir(dir) => UnsharedDirectoryType::Dir(Cow::Owned(dir.into())),
		}
	}
}

impl TryFrom<DirEnum> for RemoteDirectory {
	type Error = JsValue;

	fn try_from(dir: DirEnum) -> Result<Self, Self::Error> {
		match dir {
			DirEnum::Dir(dir) => Ok(dir.into()),
			DirEnum::Root(_) => Err(JsValue::from_str(
				"Cannot convert root directory to RemoteDirectory",
			)),
		}
	}
}

#[derive(Tsify, Deserialize)]
#[tsify(from_wasm_abi)]
pub struct UploadFileParams {
	pub parent: DirEnum,
	pub name: String,
	#[tsify(type = "bigint")]
	#[serde(with = "filen_types::serde::time::optional", default)]
	pub created: Option<DateTime<Utc>>,
	#[tsify(type = "bigint")]
	#[serde(with = "filen_types::serde::time::optional", default)]
	pub modified: Option<DateTime<Utc>>,
	#[serde(skip_serializing_if = "Option::is_none", default)]
	pub mime: Option<String>,
}

impl UploadFileParams {
	pub(crate) fn into_file_builder(
		self,
		client: &filen_sdk_rs::auth::Client,
	) -> filen_sdk_rs::fs::file::FileBuilder {
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

#[derive(Tsify, Serialize)]
#[tsify(into_wasm_abi)]
#[serde(tag = "type")]
#[cfg_attr(test, derive(Clone, Debug, PartialEq, Eq))]
pub enum NonRootObject {
	#[serde(rename = "dir")]
	Dir(Dir),
	#[serde(rename = "file")]
	File(File),
}

#[derive(Tsify, Deserialize)]
#[tsify(from_wasm_abi, large_number_types_as_bigints)]
pub struct UploadFileStreamParams {
	#[serde(flatten)]
	pub file_params: UploadFileParams,
	#[tsify(type = "ReadableStream<Uint8Array>")]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub reader: web_sys::ReadableStream,
	#[serde(default)]
	pub known_size: Option<u64>,
	#[tsify(type = "(bytes: bigint) => void")]
	#[serde(with = "serde_wasm_bindgen::preserve", default)]
	pub progress: js_sys::Function,
}

#[derive(Tsify, Deserialize)]
#[tsify(from_wasm_abi, large_number_types_as_bigints)]
pub struct DownloadFileStreamParams {
	pub file: File,
	#[tsify(type = "WritableStream<Uint8Array>")]
	#[serde(with = "serde_wasm_bindgen::preserve")]
	pub writer: web_sys::WritableStream,
	#[tsify(type = "(bytes: bigint) => void")]
	#[serde(with = "serde_wasm_bindgen::preserve", default)]
	pub progress: js_sys::Function,
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use wasm_bindgen_test::wasm_bindgen_test;

	use super::*;

	#[wasm_bindgen_test]
	fn root_serde() {
		let root = Root {
			uuid: UuidStr::default(),
		};
		let js_value = JsValue::from(root.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let dir_enum: DirEnum = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(dir_enum, DirEnum::Root(root));
	}

	#[wasm_bindgen_test]
	fn dir_serde() {
		let dir = Dir {
			uuid: UuidStr::default(),
			parent: ParentUuid::default(),
			color: Some("blue".to_string()),
			favorited: true,
			meta: DirMeta::Decoded(DecryptedDirMeta {
				name: "Test Directory".to_string(),
				created: Some(Utc::now()),
			}),
		};
		let js_value = JsValue::from(dir.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let dir_enum: DirEnum = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(dir_enum, DirEnum::Dir(dir));
	}

	#[wasm_bindgen_test]
	fn non_root_object_serde() {
		let file = File {
			uuid: UuidStr::default(),
			meta: FileMeta::Decoded(DecryptedFileMeta {
				name: "Test File".to_string(),
				mime: "text/plain".to_string(),
				created: Some(Utc::now()),
				modified: Utc::now(),
				hash: None,
				size: 1024,
				key: "file_key".to_string(),
				version: FileEncryptionVersion::V1,
			}),
			parent: ParentUuid::default(),
			size: 1024,
			favorited: false,
			region: "us-west-1".to_string(),
			bucket: "test-bucket".to_string(),
			chunks: 1,
		};

		let dir = Dir {
			uuid: UuidStr::from_str("413c5087-cef2-468a-a7b0-3e4f597fffd3").unwrap(),
			parent: ParentUuid::from_str("32514e81-2753-4741-aac9-7da2400900c3").unwrap(),
			color: None,
			favorited: false,
			meta: DirMeta::Decoded(DecryptedDirMeta {
				name: "wasm-test-dir".to_string(),
				created: Some(DateTime::from_timestamp_millis(1755781567998).unwrap()),
			}),
		};

		let non_root_object = NonRootObject::File(file.clone());
		let js_value = JsValue::from(non_root_object.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let deserialized_object: File = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_object, file);

		let non_root_object = NonRootObject::Dir(dir.clone());
		let js_value = JsValue::from(non_root_object.clone());

		let js_value2 = js_value.clone();
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let deserialized_object: Dir = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_object, dir);

		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value2);
		let deserialized_object: DirEnum = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_object, DirEnum::Dir(dir));
	}

	#[wasm_bindgen_test]
	fn dir_meta_serde() {
		let mut dir = Dir {
			uuid: UuidStr::default(),
			parent: ParentUuid::default(),
			color: Some("blue".to_string()),
			favorited: true,
			meta: DirMeta::Decoded(DecryptedDirMeta {
				name: "Test Directory".to_string(),
				created: Some(Utc::now()),
			}),
		};
		let js_value = JsValue::from(dir.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let deserialized_dir: Dir = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_dir, dir);

		dir.meta = DirMeta::DecryptedRaw(vec![1, 2, 3, 4]);
		let js_value = JsValue::from(dir.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let deserialized_dir: Dir = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_dir, dir);

		dir.meta = DirMeta::DecryptedUTF8("Test Directory".to_string());
		let js_value = JsValue::from(dir.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let deserialized_dir: Dir = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_dir, dir);

		dir.meta = DirMeta::Encrypted(EncryptedString("encrypted_data".to_string()));
		let js_value = JsValue::from(dir.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let deserialized_dir: Dir = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_dir, dir);
	}
}
