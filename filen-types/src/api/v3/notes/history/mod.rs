use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{api::v3::notes::NoteType, fs::UuidStr};

pub mod restore;

pub const ENDPOINT: &str = "v3/notes/content";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: UuidStr,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(transparent)]
pub struct Response<'a>(pub Vec<NoteHistory<'a>>);

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NoteHistory<'a> {
	pub id: u64,
	pub preview: Cow<'a, str>,
	pub content: Cow<'a, str>,
	pub edited_timestamp: DateTime<Utc>,
	pub editor_id: u64,
	pub r#type: NoteType,
}
