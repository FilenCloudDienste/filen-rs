use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::{
	auth::FileEncryptionVersion,
	crypto::{EncryptedString, Sha512Hash, rsa::RSAEncryptedString},
	fs::{ParentUuid, UuidStr},
};
use serde::{Deserialize, Serialize};

use crate::{
	connect::fs::SharingRole,
	crypto::{error::ConversionError, file::FileKey},
	fs::file::{
		RemoteFile, RemoteRootFile,
		enums::RemoteFileType,
		meta::{DecryptedFileMeta as SDKDecryptedFileMeta, FileMeta as SDKFileMeta},
	},
	js::{AsEncodedOrDecoded, EncodedOrDecoded},
	thumbnail::is_supported_thumbnail_mime,
};

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use tsify::Tsify;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), derive(Tsify))]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	tsify(large_number_types_as_bigints)
)]
#[cfg_attr(test, derive(Debug))]
pub struct DecryptedFileMeta {
	pub name: String,
	pub mime: String,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	#[serde(
		with = "filen_types::serde::time::optional",
		skip_serializing_if = "Option::is_none",
		default
	)]
	pub created: Option<DateTime<Utc>>,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub modified: DateTime<Utc>,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "Uint8Array")
	)]
	#[serde(skip_serializing_if = "Option::is_none", default)]
	pub hash: Option<Sha512Hash>,

	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub size: u64,
	pub key: String,
	pub version: FileEncryptionVersion,
}
#[cfg(feature = "node")]
super::napi_to_json_impl!(&DecryptedFileMeta);

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
	type Error = ConversionError;
	fn try_from(meta: DecryptedFileMeta) -> Result<Self, Self::Error> {
		Ok(SDKDecryptedFileMeta {
			name: Cow::Owned(meta.name),
			mime: Cow::Owned(meta.mime),
			created: meta.created,
			last_modified: meta.modified,
			hash: meta.hash,
			size: meta.size,
			key: Cow::Owned(FileKey::from_str_with_version(&meta.key, meta.version)?),
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
	Encrypted(EncryptedString<'a>),
	RSAEncrypted(RSAEncryptedString<'a>),
}

#[cfg(feature = "node")]
super::napi_to_json_impl!(FileMetaEncoded<'_>);

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
	type Error = ConversionError;
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

impl<'a>
	AsEncodedOrDecoded<
		'a,
		FileMetaEncoded<'a>,
		&'a DecryptedFileMeta,
		FileMetaEncoded<'static>,
		DecryptedFileMeta,
	> for FileMeta
{
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

	fn from_decoded(decoded: DecryptedFileMeta) -> Self {
		FileMeta::Decoded(decoded)
	}

	fn from_encoded(encoded: FileMetaEncoded<'static>) -> Self {
		match encoded {
			FileMetaEncoded::DecryptedRaw(data) => FileMeta::DecryptedRaw(data.into_owned()),
			FileMetaEncoded::DecryptedUTF8(data) => FileMeta::DecryptedUTF8(data.into_owned()),
			FileMetaEncoded::Encrypted(data) => FileMeta::Encrypted(data.into_owned()),
			FileMetaEncoded::RSAEncrypted(data) => FileMeta::RSAEncrypted(data.into_owned()),
		}
	}
}

#[derive(Clone)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), derive(Tsify))]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
#[serde(rename_all = "camelCase")]
pub struct File {
	pub uuid: UuidStr,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(optional, type = "DecryptedFileMeta")
	)]
	pub meta: FileMeta,

	pub parent: ParentUuid,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub size: u64,
	pub favorited: bool,

	pub region: String,
	pub bucket: String,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub chunks: u64,
	// JS only field, indicates if the file can have a thumbnail generated
	// this is here to avoid having to call into WASM to check mime types
	pub can_make_thumbnail: bool,
}

#[cfg(feature = "node")]
super::napi_to_json_impl!(File);
#[cfg(feature = "node")]
super::napi_from_json_impl!(File);

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
impl wasm_bindgen::__rt::VectorIntoJsValue for File {
	fn vector_into_jsvalue(
		vector: wasm_bindgen::__rt::std::boxed::Box<[Self]>,
	) -> wasm_bindgen::JsValue {
		wasm_bindgen::__rt::js_value_vector_into_jsvalue(vector)
	}
}

impl From<RemoteFile> for File {
	fn from(file: RemoteFile) -> Self {
		let meta = file.meta.into();
		File {
			can_make_thumbnail: if let FileMeta::Decoded(meta) = &meta {
				is_supported_thumbnail_mime(&meta.mime)
			} else {
				false
			},
			uuid: file.uuid,
			meta,
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
	type Error = ConversionError;
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

#[derive(Tsify)]
#[tsify(from_wasm_abi, into_wasm_abi)]
#[serde(rename_all = "camelCase")]
pub struct RootFile {
	pub uuid: UuidStr,
	pub size: u64,
	pub chunks: u64,
	pub region: String,
	pub bucket: String,
	#[tsify(optional, type = "DecryptedDirMeta")]
	pub meta: FileMeta,
	// JS only field, indicates if the file can have a thumbnail generated
	// this is here to avoid having to call into WASM to check mime types
	pub can_make_thumbnail: bool,
}

impl TryFrom<RootFile> for RemoteRootFile {
	type Error = ConversionError;
	fn try_from(file: RootFile) -> Result<Self, Self::Error> {
		Ok(RemoteRootFile {
			uuid: file.uuid,
			size: file.size,
			chunks: file.chunks,
			region: file.region,
			bucket: file.bucket,
			meta: file.meta.try_into()?,
		})
	}
}

impl From<RemoteRootFile> for RootFile {
	fn from(file: RemoteRootFile) -> Self {
		let meta = file.meta.into();
		RootFile {
			can_make_thumbnail: if let FileMeta::Decoded(meta) = &meta {
				is_supported_thumbnail_mime(&meta.mime)
			} else {
				false
			},
			uuid: file.uuid,
			size: file.size,
			chunks: file.chunks,
			region: file.region,
			bucket: file.bucket,
			meta,
		}
	}
}

#[derive(Tsify, Serialize, Deserialize)]
#[tsify(from_wasm_abi, into_wasm_abi)]
#[serde(rename_all = "camelCase")]
pub struct SharedFile {
	pub file: RootFile,
	pub sharing_role: SharingRole,
}

impl TryFrom<SharedFile> for crate::connect::fs::SharedFile {
	type Error = <RemoteRootFile as TryFrom<RootFile>>::Error;
	fn try_from(shared: SharedFile) -> Result<Self, Self::Error> {
		Ok(Self {
			file: shared.file.try_into()?,
			sharing_role: shared.sharing_role,
		})
	}
}

impl From<crate::connect::fs::SharedFile> for SharedFile {
	fn from(shared: crate::connect::fs::SharedFile) -> Self {
		Self {
			file: shared.file.into(),
			sharing_role: shared.sharing_role,
		}
	}
}

impl wasm_bindgen::__rt::VectorIntoJsValue for SharedFile {
	fn vector_into_jsvalue(
		vector: wasm_bindgen::__rt::std::boxed::Box<[Self]>,
	) -> wasm_bindgen::JsValue {
		wasm_bindgen::__rt::js_value_vector_into_jsvalue(vector)
	}
}

#[derive(Tsify, Serialize, Deserialize)]
#[tsify(from_wasm_abi, into_wasm_abi)]
#[serde(untagged)]
pub enum FileEnum {
	File(File),
	RootFile(RootFile),
}

impl TryFrom<FileEnum> for RemoteFileType<'static> {
	type Error = ConversionError;
	fn try_from(file: FileEnum) -> Result<Self, Self::Error> {
		Ok(match file {
			FileEnum::File(file) => RemoteFileType::File(Cow::Owned(file.try_into()?)),
			FileEnum::RootFile(file) => RemoteFileType::SharedFile(Cow::Owned(file.try_into()?)),
		})
	}
}

#[derive(Debug, PartialEq, Eq, Clone, Default, Tsify, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMetaChanges {
	name: Option<String>,
	mime: Option<String>,
	last_modified: Option<DateTime<Utc>>,
	created: Option<Option<DateTime<Utc>>>,
}

mod serde_impls {
	use serde::ser::SerializeStruct;

	use crate::js::HIDDEN_META_KEY;

	use super::*;
	impl Serialize for File {
		fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
		where
			S: serde::Serializer,
		{
			let mut state = serializer.serialize_struct("File", 8)?;
			state.serialize_field("uuid", &self.uuid)?;
			state.serialize_field("parent", &self.parent)?;

			state.serialize_field("size", &self.size)?;
			state.serialize_field("favorited", &self.favorited)?;

			state.serialize_field("region", &self.region)?;
			state.serialize_field("bucket", &self.bucket)?;
			state.serialize_field("chunks", &self.chunks)?;
			state.serialize_field("canMakeThumbnail", &self.can_make_thumbnail)?;

			match self.meta.as_encoded_or_decoded() {
				EncodedOrDecoded::Decoded(meta) => state.serialize_field("meta", &meta)?,
				EncodedOrDecoded::Encoded(encoded) => {
					state.serialize_field(HIDDEN_META_KEY, &encoded)?
				}
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
			D: serde::Deserializer<'de>,
		{
			let intermediate = FileIntermediate::deserialize(deserializer)?;

			Ok(File {
				uuid: intermediate.uuid,
				meta: FileMeta::from_encoded_or_decoded(
					intermediate.hidden_meta,
					intermediate.meta,
				)
				.ok_or_else(|| {
					serde::de::Error::custom(format!(
						"either 'meta' or '{HIDDEN_META_KEY}' field is required"
					))
				})?,
				parent: intermediate.parent,
				size: intermediate.size,
				favorited: intermediate.favorited,
				region: intermediate.region,
				bucket: intermediate.bucket,
				chunks: intermediate.chunks,
				can_make_thumbnail: false,
			})
		}
	}

	impl Serialize for RootFile {
		fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
		where
			S: serde::Serializer,
		{
			let mut state = serializer.serialize_struct("RootFile", 6)?;
			state.serialize_field("uuid", &self.uuid)?;
			state.serialize_field("size", &self.size)?;
			state.serialize_field("chunks", &self.chunks)?;
			state.serialize_field("region", &self.region)?;
			state.serialize_field("bucket", &self.bucket)?;
			state.serialize_field("canMakeThumbnail", &self.can_make_thumbnail)?;
			match self.meta.as_encoded_or_decoded() {
				EncodedOrDecoded::Decoded(meta) => state.serialize_field("meta", &meta)?,
				EncodedOrDecoded::Encoded(encoded) => {
					state.serialize_field(HIDDEN_META_KEY, &encoded)?
				}
			}
			state.end()
		}
	}

	#[derive(Deserialize)]
	struct RootFileIntermediate<'a> {
		uuid: UuidStr,
		size: u64,
		chunks: u64,
		region: String,
		bucket: String,
		meta: Option<DecryptedFileMeta>,
		// HIDDEN_META_KEY
		#[serde(rename = "__hiddenMeta")]
		hidden_meta: Option<FileMetaEncoded<'a>>,
	}

	impl<'de> Deserialize<'de> for RootFile {
		fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
		where
			D: serde::Deserializer<'de>,
		{
			let intermediate = RootFileIntermediate::deserialize(deserializer)?;

			Ok(RootFile {
				uuid: intermediate.uuid,
				size: intermediate.size,
				chunks: intermediate.chunks,
				region: intermediate.region,
				bucket: intermediate.bucket,
				meta: FileMeta::from_encoded_or_decoded(
					intermediate.hidden_meta,
					intermediate.meta,
				)
				.ok_or_else(|| {
					serde::de::Error::custom(format!(
						"either 'meta' or '{HIDDEN_META_KEY}' field is required"
					))
				})?,
				can_make_thumbnail: false,
			})
		}
	}
}
