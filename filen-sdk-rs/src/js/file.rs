use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::{
	auth::FileEncryptionVersion,
	crypto::{Blake3Hash, EncryptedString, rsa::RSAEncryptedString},
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
	thumbnail::is_supported_thumbnail_mime,
};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use tsify::Tsify;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DecryptedFileMeta {
	pub name: String,
	pub mime: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	#[serde(
		with = "filen_types::serde::time::optional",
		skip_serializing_if = "Option::is_none",
		default
	)]
	pub created: Option<DateTime<Utc>>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub modified: DateTime<Utc>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "Uint8Array")
	)]
	#[serde(skip_serializing_if = "Option::is_none", default)]
	pub hash: Option<Blake3Hash>,

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
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
	type Error = ConversionError;
	fn try_from(meta: DecryptedFileMeta) -> Result<Self, Self::Error> {
		Ok(SDKDecryptedFileMeta {
			name: Cow::Owned(meta.name),
			mime: Cow::Owned(meta.mime),
			created: meta.created,
			last_modified: meta.modified,
			hash: meta.hash,
			size: meta.size,
			key: Cow::Owned(FileKey::from_string_with_version(
				Cow::Owned(meta.key),
				meta.version,
			)?),
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi, into_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type")]
pub enum FileMeta {
	Decoded(DecryptedFileMeta),
	DecryptedUTF8(String),
	DecryptedRaw(Vec<u8>),
	Encrypted(String),
	RSAEncrypted(String),
}

#[derive(Serialize, Deserialize, Clone)]
enum FileMetaEncoded<'a> {
	DecryptedRaw(Cow<'a, [u8]>),
	DecryptedUTF8(Cow<'a, str>),
	Encrypted(EncryptedString<'a>),
	RSAEncrypted(RSAEncryptedString<'a>),
}

impl From<SDKFileMeta<'_>> for FileMeta {
	fn from(meta: SDKFileMeta) -> Self {
		match meta {
			SDKFileMeta::Decoded(meta) => FileMeta::Decoded(meta.into()),
			SDKFileMeta::DecryptedUTF8(meta) => FileMeta::DecryptedUTF8(meta.into_owned()),
			SDKFileMeta::DecryptedRaw(meta) => FileMeta::DecryptedRaw(meta.into_owned()),
			SDKFileMeta::Encrypted(meta) => FileMeta::Encrypted(meta.0.into_owned()),
			SDKFileMeta::RSAEncrypted(meta) => FileMeta::RSAEncrypted(meta.0.into_owned()),
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
			FileMeta::Encrypted(meta) => SDKFileMeta::Encrypted(EncryptedString(Cow::Owned(meta))),
			FileMeta::RSAEncrypted(meta) => {
				SDKFileMeta::RSAEncrypted(RSAEncryptedString(Cow::Owned(meta)))
			}
		})
	}
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi, into_wasm_abi, large_number_types_as_bigints),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct File {
	pub uuid: UuidStr,
	pub meta: FileMeta,

	pub parent: ParentUuid,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub size: u64,
	pub favorited: bool,

	pub region: String,
	pub bucket: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint"),
		serde(with = "chrono::serde::ts_milliseconds")
	)]
	pub timestamp: DateTime<Utc>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub chunks: u64,
	// JS only field, indicates if the file can have a thumbnail generated
	// this is here to avoid having to call into WASM to check mime types
	pub can_make_thumbnail: bool,
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
			timestamp: file.timestamp,
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
			timestamp: file.timestamp,
			chunks: file.chunks,
		})
	}
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi, into_wasm_abi, large_number_types_as_bigints),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct RootFile {
	pub uuid: UuidStr,
	pub size: u64,
	pub chunks: u64,
	pub region: String,
	pub bucket: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint"),
		serde(with = "chrono::serde::ts_milliseconds")
	)]
	pub timestamp: DateTime<Utc>,
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
			timestamp: file.timestamp,
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
			timestamp: file.timestamp,
			meta,
		}
	}
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi, into_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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

#[derive(Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi, into_wasm_abi, large_number_types_as_bigints)
)]
#[serde(untagged)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi, into_wasm_abi, large_number_types_as_bigints),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct FileVersion {
	pub bucket: String,
	pub region: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub chunks: u64,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub size: u64,
	pub metadata: FileMeta,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	pub uuid: UuidStr,
}

impl From<crate::fs::file::FileVersion> for FileVersion {
	fn from(version: crate::fs::file::FileVersion) -> Self {
		FileVersion {
			bucket: version.bucket,
			region: version.region,
			chunks: version.chunks,
			size: version.size,
			metadata: version.metadata.into(),
			timestamp: version.timestamp,
			uuid: version.uuid,
		}
	}
}

impl TryFrom<FileVersion> for crate::fs::file::FileVersion {
	type Error = ConversionError;
	fn try_from(version: FileVersion) -> Result<Self, Self::Error> {
		Ok(crate::fs::file::FileVersion {
			bucket: version.bucket,
			region: version.region,
			chunks: version.chunks,
			size: version.size,
			metadata: version.metadata.try_into()?,
			timestamp: version.timestamp,
			uuid: version.uuid,
		})
	}
}
