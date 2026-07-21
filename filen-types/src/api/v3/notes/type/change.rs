use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{api::v3::notes::NoteType, crypto::EncryptedString, fs::Uuid};

pub const ENDPOINT: &str = "v3/notes/type/change";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub preview: EncryptedString<'a>,
	pub content: EncryptedString<'a>,
	#[serde(rename = "type")]
	pub note_type: NoteType,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	pub uuid: Uuid,
	#[serde(rename = "type")]
	pub note_type: NoteType,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub editor_id: u64,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub edited_timestamp: DateTime<Utc>,
}
