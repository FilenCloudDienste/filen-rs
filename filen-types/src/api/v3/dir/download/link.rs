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

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub files: Vec<File<'a>>,
	#[serde(rename = "folders")]
	pub dirs: Vec<Directory<'a>>,
}

/// A folder row. Unlike the owner surface's, it carries no `color` or
/// `favorited`. The folder the request is for is one of the rows, with
/// `parent: "base"`.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Directory<'a> {
	pub uuid: Uuid,
	#[serde(rename = "name")]
	pub meta: EncryptedString<'a>,
	#[serde(with = "crate::serde::parent_uuid::base")]
	pub parent: Option<ParentUuid>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
}

/// A file row. Unlike the owner surface's, it carries no `stableUUID`,
/// `size` or `favorited`.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct File<'a> {
	pub uuid: Uuid,
	pub metadata: EncryptedString<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub chunks: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub chunks_size: u64,
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
	pub parent: ParentUuid,
	pub version: FileEncryptionVersion,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rows_deserialize_in_the_shape_the_server_sends() {
		// The rows carry no stableUUID, size, color or favorited. The folder the
		// request is for comes back too, with parent "base".
		let json = r#"{
			"folders": [
				{"uuid":"11111111-1111-1111-1111-111111111111","name":"n","parent":"base","timestamp":1700000},
				{"uuid":"22222222-2222-2222-2222-222222222222","name":"n","parent":"11111111-1111-1111-1111-111111111111","timestamp":1700000}
			],
			"files": [
				{"uuid":"33333333-3333-3333-3333-333333333333","metadata":"m","timestamp":1700000,"chunks":1,"chunksSize":100,"bucket":"b","region":"r","parent":"22222222-2222-2222-2222-222222222222","version":2}
			]
		}"#;
		let response = serde_json::from_str::<Response>(json).unwrap();
		assert_eq!(response.dirs[0].parent, None);
		assert!(response.dirs[1].parent.is_some());
		assert_eq!(response.files[0].chunks_size, 100);
	}
}
