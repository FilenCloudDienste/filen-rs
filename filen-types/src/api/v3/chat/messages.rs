use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/chat/messages";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub conversation: UuidStr,
	#[serde(with = "chrono::serde::ts_milliseconds")]
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
	pub edited_timestamp: u64,
	pub sent_timestamp: u64,
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
