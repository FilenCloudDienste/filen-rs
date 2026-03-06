use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, ser::SerializeStruct};

use crate::{
	api::v3::dir::color::DirColor, auth::FileEncryptionVersion, crypto::EncryptedString,
	fs::UuidStr,
};

pub const ENDPOINT: &str = "v3/shared/out";

#[derive(Debug, Clone)]
pub struct Request {
	pub receiver_id: Option<u64>,
}

impl Serialize for Request {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let mut state = serializer.serialize_struct("Request", 2)?;
		state.serialize_field("receiverId", &self.receiver_id.unwrap_or_default())?;
		state.serialize_field("uuid", "shared-out")?;
		state.end()
	}
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	#[serde(rename = "uploads")]
	pub files: Vec<SharedRootFileOut<'a>>,
	#[serde(rename = "folders")]
	pub dirs: Vec<SharedRootDirOut<'a>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SharedRootFileOut<'a> {
	pub uuid: UuidStr,
	pub metadata: EncryptedString<'a>,
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
	pub chunks: u64,
	pub size: u64,
	pub version: FileEncryptionVersion,
	pub receiver_email: Cow<'a, str>,
	pub receiver_id: u64,
	#[serde(with = "crate::serde::boolean::number")]
	pub write_access: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SharedRootDirOut<'a> {
	pub uuid: UuidStr,
	pub metadata: EncryptedString<'a>,
	pub receiver_email: Cow<'a, str>,
	pub receiver_id: u64,
	#[serde(with = "crate::serde::boolean::number")]
	pub write_access: bool,
	pub color: DirColor<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::boolean::number", rename = "is_sync")]
	pub is_sync: bool,
	#[serde(with = "crate::serde::boolean::number", rename = "is_default")]
	pub is_default: bool, // might be redundant with parent option
}
