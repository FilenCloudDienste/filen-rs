use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	api::v3::dir::color::DirColor, auth::FileEncryptionVersion, crypto::rsa::RSAEncryptedString,
	fs::Uuid,
};

/// Looks up one item shared with the caller by its uuid. The server answers [`ITEM_NOT_SHARED`]
/// when the item is not shared with the caller (trashed, deleted, revoked, left, moved out of
/// the shared tree, in the caller's own drive or in someone else's, never existed, or its
/// sharer's account no longer exists), and only after every read of the lookup succeeded; any
/// failure is `internal_error`.
pub const ENDPOINT: &str = "v3/shared/in/item";

/// The error code [`ENDPOINT`] answers when the item is not shared with the caller.
pub const ITEM_NOT_SHARED: &str = "item_not_shared";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
}

/// One item shared with the caller, as `v3/shared/in` lists it, tagged by `type` with the values
/// `item/share` uses.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum Response<'a> {
	#[serde(rename = "file")]
	File(SharedItemFileIn<'a>),
	#[serde(rename = "folder")]
	Dir(SharedItemDirIn<'a>),
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SharedItemFileIn<'a> {
	/// The current version's uuid, which differs from the one requested when that was an older
	/// version of the file.
	pub uuid: Uuid,
	/// The direct parent as recorded for the caller; `None` exactly when the file is a share root.
	/// The field must be present: a missing one is an error, not a root.
	#[serde(deserialize_with = "Option::deserialize")]
	pub parent: Option<Uuid>,
	pub metadata: RSAEncryptedString<'a>,
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub chunks: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub size: u64,
	pub version: FileEncryptionVersion,
	/// The sharer's email, read when the lookup runs (`v3/shared/in` sends the one stored when
	/// the item was shared).
	pub sharer_email: Cow<'a, str>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub sharer_id: u64,
	/// The share's stored write access. The backend never sets it and nothing enforces it.
	#[serde(with = "crate::serde::boolean::number")]
	pub write_access: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SharedItemDirIn<'a> {
	pub uuid: Uuid,
	/// The direct parent as recorded for the caller; `None` exactly when the folder is a share
	/// root. The field must be present: a missing one is an error, not a root.
	#[serde(deserialize_with = "Option::deserialize")]
	pub parent: Option<Uuid>,
	pub metadata: RSAEncryptedString<'a>,
	/// The sharer's email, read when the lookup runs (`v3/shared/in` sends the one stored when
	/// the item was shared).
	pub sharer_email: Cow<'a, str>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub sharer_id: u64,
	/// The share's stored write access. The backend never sets it and nothing enforces it.
	#[serde(with = "crate::serde::boolean::number")]
	pub write_access: bool,
	pub color: DirColor<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::boolean::number", rename = "is_sync")]
	pub is_sync: bool,
	#[serde(with = "crate::serde::boolean::number", rename = "is_default")]
	pub is_default: bool,
}

#[cfg(test)]
mod tests {
	use super::*;

	const ITEM: Uuid = Uuid::from_u128(0x4f1c0d1e_5a3b_4c7a_9d8e_2b1f0a9c6e33);
	const PARENT: Uuid = Uuid::from_u128(0xa7e20000_0000_4000_8000_000000000001);

	fn dir(json: &str) -> SharedItemDirIn<'static> {
		match serde_json::from_str::<Response<'static>>(json).unwrap() {
			Response::Dir(dir) => dir,
			Response::File(file) => panic!("expected a folder, got {file:?}"),
		}
	}

	fn file(json: &str) -> SharedItemFileIn<'static> {
		match serde_json::from_str::<Response<'static>>(json).unwrap() {
			Response::File(file) => file,
			Response::Dir(dir) => panic!("expected a file, got {dir:?}"),
		}
	}

	#[test]
	fn a_nested_folder_row_deserializes_with_its_parent_and_sharer() {
		let dir = dir(r#"{
			"type": "folder",
			"uuid": "4f1c0d1e-5a3b-4c7a-9d8e-2b1f0a9c6e33",
			"parent": "a7e20000-0000-4000-8000-000000000001",
			"metadata": "cipher",
			"writeAccess": 1,
			"color": "blue",
			"timestamp": 1764230400,
			"is_sync": 0,
			"is_default": 0,
			"sharerId": 123456,
			"sharerEmail": "owner@example.com"
		}"#);
		assert_eq!(dir.uuid, ITEM);
		assert_eq!(dir.parent, Some(PARENT));
		assert_eq!(dir.metadata.0, "cipher");
		assert!(dir.write_access);
		assert_eq!(dir.color, DirColor::Blue);
		assert_eq!(dir.timestamp.timestamp(), 1_764_230_400);
		assert!(!dir.is_sync);
		assert!(!dir.is_default);
		assert_eq!(dir.sharer_id, 123_456);
		assert_eq!(dir.sharer_email, "owner@example.com");
	}

	#[test]
	fn a_root_folder_row_has_a_null_parent() {
		let dir = dir(r#"{
			"type": "folder",
			"uuid": "4f1c0d1e-5a3b-4c7a-9d8e-2b1f0a9c6e33",
			"parent": null,
			"metadata": "cipher",
			"writeAccess": 0,
			"color": "default",
			"timestamp": 1764230400000,
			"is_sync": 0,
			"is_default": 0,
			"sharerId": 7,
			"sharerEmail": "owner@example.com"
		}"#);
		assert_eq!(dir.parent, None);
		assert!(!dir.write_access);
		assert_eq!(dir.color, DirColor::Default);
		assert_eq!(dir.timestamp.timestamp(), 1_764_230_400);
	}

	#[test]
	fn a_nested_file_row_deserializes_with_its_storage_fields() {
		let file = file(
			r#"{
			"type": "file",
			"uuid": "4f1c0d1e-5a3b-4c7a-9d8e-2b1f0a9c6e33",
			"parent": "a7e20000-0000-4000-8000-000000000001",
			"metadata": "cipher",
			"bucket": "filen-1",
			"region": "de-1",
			"chunks": 3,
			"size": 2500000,
			"version": 2,
			"writeAccess": 1,
			"timestamp": 1764230400,
			"sharerId": 123456,
			"sharerEmail": "owner@example.com"
		}"#,
		);
		assert_eq!(file.uuid, ITEM);
		assert_eq!(file.parent, Some(PARENT));
		assert_eq!(file.bucket, "filen-1");
		assert_eq!(file.region, "de-1");
		assert_eq!(file.chunks, 3);
		assert_eq!(file.size, 2_500_000);
		assert_eq!(file.version, FileEncryptionVersion::V2);
		assert!(file.write_access);
		assert_eq!(file.sharer_id, 123_456);
		assert_eq!(file.sharer_email, "owner@example.com");
	}

	#[test]
	fn a_root_file_row_has_a_null_parent() {
		let file = file(
			r#"{
			"type": "file",
			"uuid": "4f1c0d1e-5a3b-4c7a-9d8e-2b1f0a9c6e33",
			"parent": null,
			"metadata": "cipher",
			"bucket": "filen-1",
			"region": "de-1",
			"chunks": 1,
			"size": 10,
			"version": 2,
			"writeAccess": 0,
			"timestamp": 1764230400,
			"sharerId": 7,
			"sharerEmail": "owner@example.com"
		}"#,
		);
		assert_eq!(file.parent, None);
		assert!(!file.write_access);
		assert_eq!(file.size, 10);
	}

	#[test]
	fn numbers_sent_as_strings_still_parse() {
		let file = file(
			r#"{
			"type": "file",
			"uuid": "4f1c0d1e-5a3b-4c7a-9d8e-2b1f0a9c6e33",
			"parent": null,
			"metadata": "cipher",
			"bucket": "filen-1",
			"region": "de-1",
			"chunks": "4",
			"size": "4000000",
			"version": 2,
			"writeAccess": 1,
			"timestamp": 1764230400,
			"sharerId": "123456",
			"sharerEmail": "owner@example.com"
		}"#,
		);
		assert_eq!(file.chunks, 4);
		assert_eq!(file.size, 4_000_000);
		assert_eq!(file.sharer_id, 123_456);
	}

	const FOLDER_FIELDS: &str = r#"
		"uuid": "4f1c0d1e-5a3b-4c7a-9d8e-2b1f0a9c6e33",
		"parent": null,
		"metadata": "cipher",
		"writeAccess": 0,
		"color": "default",
		"timestamp": 1764230400,
		"is_sync": 0,
		"is_default": 0,
		"sharerId": 7,
		"sharerEmail": "owner@example.com"
	"#;

	#[test]
	fn a_row_without_a_type_is_an_error() {
		let json = format!("{{{FOLDER_FIELDS}}}");
		assert!(serde_json::from_str::<Response<'static>>(&json).is_err());
	}

	#[test]
	fn a_row_with_an_unknown_type_is_an_error() {
		// "dir" is the bulk `FileOrDir` tag, not the one this endpoint sends
		let json = format!(r#"{{"type": "dir", {FOLDER_FIELDS}}}"#);
		assert!(serde_json::from_str::<Response<'static>>(&json).is_err());
	}

	#[test]
	fn a_row_without_a_parent_field_is_an_error_not_a_root() {
		let json = r#"{
			"type": "folder",
			"uuid": "4f1c0d1e-5a3b-4c7a-9d8e-2b1f0a9c6e33",
			"metadata": "cipher",
			"writeAccess": 0,
			"color": "default",
			"timestamp": 1764230400,
			"is_sync": 0,
			"is_default": 0,
			"sharerId": 7,
			"sharerEmail": "owner@example.com"
		}"#;
		assert!(serde_json::from_str::<Response<'static>>(json).is_err());
	}
}
