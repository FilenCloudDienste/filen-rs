use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_macros::CowHelpers;
use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::UuidStr, traits::CowHelpers};

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
pub struct Response<'a>(pub Vec<ChatMessageEncrypted<'a>>);

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessageEncrypted<'a> {
	#[serde(rename = "conversation")]
	pub chat: UuidStr,
	#[serde(flatten)]
	pub inner: ChatMessagePartialEncrypted<'a>,
	#[serde(deserialize_with = "crate::serde::option::result_to_option::deserialize")]
	pub reply_to: Option<ChatMessagePartialEncrypted<'a>>,
	pub embed_disabled: bool,
	pub edited: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub edited_timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub sent_timestamp: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(
		target_family = "wasm",
		target_os = "unknown",
		not(feature = "service-worker")
	),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatMessagePartialEncrypted<'a> {
	pub uuid: UuidStr,
	pub sender_id: u64,
	pub sender_email: Cow<'a, str>,
	pub sender_avatar: Option<Cow<'a, str>>,
	pub sender_nick_name: Cow<'a, str>,
	pub message: EncryptedString<'a>,
}
