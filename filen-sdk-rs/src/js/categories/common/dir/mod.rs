use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::fs::UuidStr;

use crate::fs::dir::RootDirectoryWithMeta;

pub(crate) mod color;
pub(crate) mod meta;

#[js_type(wasm_all)]
pub struct RootDirWithMeta {
	pub uuid: UuidStr,
	pub color: color::DirColor,
	#[cfg_attr(
		feature = "wasm-full",
		tsify(type = "bigint"),
		serde(with = "chrono::serde::ts_milliseconds")
	)]
	pub timestamp: DateTime<Utc>,
	pub meta: meta::DirMeta,
}

impl From<RootDirectoryWithMeta> for RootDirWithMeta {
	fn from(value: RootDirectoryWithMeta) -> Self {
		Self {
			uuid: value.uuid,
			color: value.color.into(),
			timestamp: value.timestamp,
			meta: value.meta.into(),
		}
	}
}

impl From<RootDirWithMeta> for RootDirectoryWithMeta {
	fn from(value: RootDirWithMeta) -> Self {
		Self {
			uuid: value.uuid,
			color: value.color.into(),
			timestamp: value.timestamp,
			meta: value.meta.into(),
		}
	}
}
