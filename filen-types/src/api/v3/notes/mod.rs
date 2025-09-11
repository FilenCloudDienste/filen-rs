use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub mod archive;
pub mod content;
pub mod create;
pub mod delete;
pub mod favorite;
pub mod history;
pub mod participants;
pub mod pinned;
pub mod restore;
pub mod tag;
pub mod tags;
pub mod title;
pub mod trash;
pub mod r#type;
pub mod untag;

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub enum NoteType {
	Text,
	Md,
	Code,
	Rich,
	Checklist,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NoteTag<'a> {
	pub uuid: UuidStr,
	pub name: Cow<'a, str>,
	pub favorite: bool,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub edited_timestamp: DateTime<Utc>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub created_timestamp: DateTime<Utc>,
}
