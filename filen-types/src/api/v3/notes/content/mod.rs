use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{api::v3::notes::NoteType, fs::UuidStr};

pub mod edit;

pub const ENDPOINT: &str = "v3/notes/content";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: UuidStr,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub preview: Cow<'a, str>,
	pub content: Cow<'a, str>,
	pub edited_timestamp: DateTime<Utc>,
	pub editor_id: u64,
	pub r#type: NoteType,
}
