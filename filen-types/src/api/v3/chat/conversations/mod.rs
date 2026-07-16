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
	#[serde(with = "crate::serde::time::optional", default)]
	pub last_focus: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct ChatConversationParticipant<'a> {
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub user_id: u64,
	pub email: Cow<'a, str>,
	pub avatar: Option<Cow<'a, str>>,
	#[serde(with = "crate::serde::option::str_empty_is_none_owned", default)]
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn conversation_deserializes_when_last_focus_absent() {
		// A server omitting `lastFocus` for a single item must not fail the whole
		// list_chats response.
		let json = r#"{
			"uuid":"00000000-0000-0000-0000-000000000000",
			"ownerId":1,
			"participants":[],
			"muted":false,
			"createdTimestamp":1700000000000
		}"#;
		let conv: ChatConversation = serde_json::from_str(json).unwrap();
		assert_eq!(conv.last_focus, None);
	}

	#[test]
	fn participant_deserializes_when_nick_name_absent() {
		let json = r#"{
			"userId":1,
			"email":"a@b.c",
			"metadata":"enc",
			"permissionsAdd":0,
			"addedTimestamp":1700000000000,
			"appearOffline":false,
			"lastActive":1700000000000
		}"#;
		let participant: ChatConversationParticipant = serde_json::from_str(json).unwrap();
		assert_eq!(participant.nick_name, None);
	}
}
