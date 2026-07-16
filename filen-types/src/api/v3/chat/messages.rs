use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::Uuid, traits::CowHelpers};

pub const ENDPOINT: &str = "v3/chat/messages";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub conversation: Uuid,
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
	pub chat: Uuid,
	#[serde(flatten)]
	pub inner: ChatMessagePartialEncrypted<'a>,
	#[serde(
		deserialize_with = "crate::serde::option::result_to_option::deserialize",
		default
	)]
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
	pub uuid: Uuid,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub sender_id: u64,
	pub sender_email: Cow<'a, str>,
	pub sender_avatar: Option<Cow<'a, str>>,
	#[serde(with = "crate::serde::option::str_empty_is_none_owned", default)]
	pub sender_nick_name: Option<String>,
	pub message: EncryptedString<'a>,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn message_deserializes_when_reply_to_and_nick_name_absent() {
		// `replyTo` (result_to_option) and `senderNickName` (str_empty_is_none_owned)
		// are commonly omitted; a missing field must default to None rather than
		// failing the whole list_messages response.
		let json = r#"{
			"conversation":"00000000-0000-0000-0000-000000000000",
			"uuid":"11111111-1111-1111-1111-111111111111",
			"senderId":1,
			"senderEmail":"a@b.c",
			"message":"enc",
			"embedDisabled":false,
			"edited":false,
			"editedTimestamp":1700000000000,
			"sentTimestamp":1700000000000
		}"#;
		let msg: ChatMessageEncrypted = serde_json::from_str(json).unwrap();
		assert_eq!(msg.reply_to, None);
		assert_eq!(msg.inner.sender_nick_name, None);
	}
}
