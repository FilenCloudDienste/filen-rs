use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	fs::UuidStr,
};

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

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
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
	pub name: EncryptedString<'a>,
	pub favorite: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub edited_timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub created_timestamp: DateTime<Utc>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NoteParticipant<'a> {
	pub user_id: u64,
	pub is_owner: bool,
	pub email: Cow<'a, str>,
	pub avatar: Option<Cow<'a, str>>,
	pub nick_name: Cow<'a, str>,
	pub metadata: RSAEncryptedString<'a>,
	pub permissions_write: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub added_timestamp: DateTime<Utc>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Note<'a> {
	pub uuid: UuidStr,
	pub owner_id: u64,
	pub editor_id: u64,
	pub is_owner: bool,
	pub favorite: bool,
	pub pinned: bool,
	pub tags: Vec<NoteTag<'a>>,
	#[serde(rename = "type")]
	pub note_type: NoteType,
	pub metadata: EncryptedString<'a>,
	pub title: EncryptedString<'a>,
	pub preview: EncryptedString<'a>,
	pub trash: bool,
	pub archive: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub created_timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub edited_timestamp: DateTime<Utc>,
	pub participants: Vec<NoteParticipant<'a>>,
}

pub const ENDPOINT: &str = "v3/notes";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(transparent)]
pub struct Response<'a>(pub Vec<Note<'a>>);
