use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	api::v3::dir::color::DirColor, auth::FileEncryptionVersion, crypto::EncryptedString,
	fs::UuidStr,
};

pub const ENDPOINT: &str = "v3/shared/out";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	#[serde(with = "crate::serde::uuid::shared_out")]
	pub uuid: Option<UuidStr>,
	#[serde(with = "crate::serde::option::default")]
	pub receiver_id: Option<u64>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	#[serde(rename = "uploads")]
	pub files: Vec<SharedFileOut<'a>>,
	#[serde(rename = "folders")]
	pub dirs: Vec<SharedDirOut<'a>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SharedFileOut<'a> {
	pub uuid: UuidStr,
	pub parent: Option<UuidStr>,
	pub metadata: Cow<'a, EncryptedString>,
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
pub struct SharedDirOut<'a> {
	pub uuid: UuidStr,
	pub parent: Option<UuidStr>,
	pub metadata: Cow<'a, EncryptedString>,
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
