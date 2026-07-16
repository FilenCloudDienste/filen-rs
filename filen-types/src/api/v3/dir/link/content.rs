use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	api::v3::dir::color::DirColor,
	auth::FileEncryptionVersion,
	crypto::{EncryptedString, LinkHashedPassword},
	fs::Uuid,
};

pub const ENDPOINT: &str = "v3/dir/link/content";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub password: LinkHashedPassword<'a>,
	pub parent: Uuid,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	#[serde(rename = "folders")]
	pub dirs: Vec<Directory<'a>>,
	pub files: Vec<File<'a>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Directory<'a> {
	pub uuid: Uuid,
	pub parent: Uuid,
	pub metadata: EncryptedString<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub color: DirColor<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct File<'a> {
	pub uuid: Uuid,
	pub parent: Uuid,
	pub metadata: EncryptedString<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub size: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub chunks: u64,
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
	pub version: FileEncryptionVersion,
}

#[cfg(test)]
mod tests {
	use chrono::Datelike;

	use super::*;

	// 1_700_000_000_000 ms is 2023-11-14. Parsed as *seconds* (the old
	// `ts_seconds` behaviour) the same integer lands in the far future
	// (year ~55893), which is exactly the silent corruption this endpoint hit.
	const MILLIS: i64 = 1_700_000_000_000;

	#[test]
	fn directory_reads_millisecond_timestamp_as_recent_date() {
		let json = r#"{
			"uuid":"00000000-0000-0000-0000-000000000000",
			"parent":"11111111-1111-1111-1111-111111111111",
			"metadata":"encrypted-meta",
			"timestamp":1700000000000,
			"color":"default"
		}"#;
		let dir: Directory = serde_json::from_str(json).unwrap();
		assert_eq!(dir.timestamp.timestamp_millis(), MILLIS);
		assert_eq!(dir.timestamp.year(), 2023);
	}

	#[test]
	fn file_reads_millisecond_timestamp_as_recent_date() {
		let json = r#"{
			"uuid":"00000000-0000-0000-0000-000000000000",
			"parent":"11111111-1111-1111-1111-111111111111",
			"metadata":"encrypted-meta",
			"timestamp":1700000000000,
			"size":1024,
			"chunks":2,
			"bucket":"bucket-1",
			"region":"us-east-1",
			"version":1
		}"#;
		let file: File = serde_json::from_str(json).unwrap();
		assert_eq!(file.timestamp.timestamp_millis(), MILLIS);
		assert_eq!(file.timestamp.year(), 2023);
	}
}
