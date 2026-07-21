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
	api::v3::chat::messages::ChatMessageEncrypted,
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	fs::Uuid,
	traits::CowHelpers,
};

pub const ENDPOINT: &str = "v3/chat/conversations";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(transparent)]
pub struct Response<'a>(pub Vec<ChatConversation<'a>>);

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ChatConversation<'a> {
	pub uuid: Uuid,
	pub last_message_full: Option<ChatMessageEncrypted<'a>>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub owner_id: u64,
	pub owner_metadata: Option<EncryptedString<'a>>,
	pub name: Option<EncryptedString<'a>>,
	pub participants: Vec<ChatConversationParticipant<'a>>,
	pub muted: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub created_timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::time::optional")]
	pub last_focus: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct ChatConversationParticipant<'a> {
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub user_id: u64,
	pub email: Cow<'a, str>,
	pub avatar: Option<Cow<'a, str>>,
	#[serde(with = "crate::serde::option::str_empty_is_none_owned")]
	pub nick_name: Option<String>,
	pub metadata: RSAEncryptedString<'a>,
	#[serde(with = "crate::serde::boolean::number")]
	pub permissions_add: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub added_timestamp: DateTime<Utc>,
	pub appear_offline: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub last_active: DateTime<Utc>,
}
