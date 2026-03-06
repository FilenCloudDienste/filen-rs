use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::fs::UuidStr;

use crate::{crypto::error::ConversionError, js::FileMeta};

#[js_type(import, export)]
pub struct FileVersion {
	pub bucket: String,
	pub region: String,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub chunks: u64,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub size: u64,
	pub metadata: FileMeta,
	#[cfg_attr(
		feature = "wasm-full",
		tsify(type = "bigint"),
		serde(with = "chrono::serde::ts_milliseconds")
	)]
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
