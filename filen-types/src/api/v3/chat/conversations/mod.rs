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

use crate::{
	api::v3::chat::messages::ChatMessage,
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	fs::UuidStr,
};

pub const ENDPOINT: &str = "v3/chat/conversations";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(transparent)]
pub struct Response<'a>(pub Vec<ChatConversation<'a>>);

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ChatConversation<'a> {
	pub uuid: UuidStr,
	pub last_message_full: Option<ChatMessage<'a>>,
	pub owner_id: u64,
	pub owner_metadata: Option<EncryptedString<'a>>,
	pub name: Option<EncryptedString<'a>>,
	pub participants: Vec<ChatConversationParticipant<'a>>,
	pub muted: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub created_timestamp: DateTime<Utc>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ChatConversationParticipant<'a> {
	pub user_id: u64,
	pub email: Cow<'a, str>,
	pub avatar: Option<Cow<'a, str>>,
	pub nick_name: Cow<'a, str>,
	pub metadata: RSAEncryptedString<'a>,
	#[serde(with = "crate::serde::boolean::number")]
	pub permissions_add: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub added_timestamp: DateTime<Utc>,
}
