use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/chat/messages";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub conversation: UuidStr,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(transparent)]
pub struct Response<'a>(pub Vec<ChatMessage<'a>>);

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage<'a> {
	pub conversation: UuidStr,
	pub uuid: UuidStr,
	pub sender_id: u64,
	pub sender_email: Cow<'a, str>,
	pub sender_avatar: Option<Cow<'a, str>>,
	pub sender_nick_name: Cow<'a, str>,
	pub message: EncryptedString<'a>,
	pub reply_to: Option<ChatMessageReplyInfo<'a>>,
	pub embed_disabled: bool,
	pub edited: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub edited_timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub sent_timestamp: DateTime<Utc>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessageReplyInfo<'a> {
	pub uuid: UuidStr,
	pub sender_id: u64,
	pub sender_email: Cow<'a, str>,
	pub sender_avatar: Cow<'a, str>,
	pub sender_nick_name: Cow<'a, str>,
	pub message: Cow<'a, str>,
}
