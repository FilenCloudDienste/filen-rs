pub mod create;
pub mod delete;
pub mod leave;
pub mod name;
pub mod online;
pub mod participants;
pub mod read;
pub mod unread;

use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "/v3/chat/conversations";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(transparent)]
pub struct Response<'a>(pub Vec<ChatConversation<'a>>);

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ChatConversation<'a> {
	pub uuid: UuidStr,
	pub last_message_sender: u64,
	pub last_message: Option<Cow<'a, EncryptedString>>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub last_message_timestamp: DateTime<Utc>,
	pub last_message_uuid: Option<UuidStr>,
	pub owner_id: u64,
	pub owner_metadata: Option<Cow<'a, EncryptedString>>,
	pub name: Option<Cow<'a, EncryptedString>>,
	pub participants: Vec<ChatConversationParticipant<'a>>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub created_timestamp: DateTime<Utc>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ChatConversationParticipant<'a> {
	pub user_id: u64,
	pub email: Cow<'a, str>,
	pub avatar: Option<Cow<'a, str>>,
	pub nick_name: Cow<'a, EncryptedString>,
	pub metadata: Cow<'a, EncryptedString>,
	#[serde(with = "crate::serde::boolean::number")]
	pub permissions_add: bool,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub added_timestamp: DateTime<Utc>,
}
