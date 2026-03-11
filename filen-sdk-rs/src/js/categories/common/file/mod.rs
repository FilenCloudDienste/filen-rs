use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::fs::{ParentUuid, UuidStr};

pub(crate) mod meta;
pub(crate) mod version;

use meta::FileMeta;

use crate::{
	crypto::error::ConversionError, io::RemoteFile, thumbnail::is_supported_thumbnail_mime,
};

#[js_type(import, export, wasm_all)]
pub struct File {
	pub(crate) uuid: UuidStr,
	pub(crate) meta: FileMeta,

	parent: ParentUuid,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	size: u64,
	favorited: bool,

	region: String,
	bucket: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint"),
		serde(with = "chrono::serde::ts_milliseconds")
	)]
	timestamp: DateTime<Utc>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	chunks: u64,
	// JS only field, indicates if the file can have a thumbnail generated
	// this is here to avoid having to call into WASM to check mime types
	can_make_thumbnail: bool,
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
