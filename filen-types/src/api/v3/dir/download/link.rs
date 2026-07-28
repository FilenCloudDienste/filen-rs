use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	auth::FileEncryptionVersion,
	crypto::{EncryptedString, LinkHashedPassword},
	fs::{ParentUuid, Uuid},
};

pub const ENDPOINT: &str = "v3/dir/download/link";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub password: LinkHashedPassword<'a>,
	pub parent: Uuid,
	pub skip_cache: bool,
}

pub use super::Directory;

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub files: Vec<File<'a>>,
	#[serde(rename = "folders")]
	pub dirs: Vec<Directory<'a>>,
}

/// The owner surface's file row minus `stableUUID`: link surfaces never carry
/// a stable id on the wire.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct File<'a> {
	pub uuid: Uuid,
	pub metadata: EncryptedString<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub chunks: u64,
	pub size: EncryptedString<'a>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub chunks_size: u64,
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
	pub parent: ParentUuid,
	pub version: FileEncryptionVersion,
	#[serde(with = "crate::serde::boolean::number")]
	pub favorited: bool,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn file_rows_deserialize_without_stable_uuid() {
		// link surfaces never carry stableUUID; the row must not require it
		let json = r#"{"uuid":"11111111-1111-1111-1111-111111111111","metadata":"m","timestamp":1700000,"chunks":1,"size":"s","chunksSize":100,"bucket":"b","region":"r","parent":"22222222-2222-2222-2222-222222222222","version":2,"favorited":0}"#;
		serde_json::from_str::<File>(json).unwrap();
	}
}
