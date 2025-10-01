use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use crate::{
	api::v3::{
		chat::{
			messages::{ChatMessageEncrypted, ChatMessagePartialEncrypted},
			typing::ChatTypingType,
		},
		dir::color::DirColor,
		notes::{NoteParticipant, NoteType},
	},
	auth::FileEncryptionVersion,
	crypto::EncryptedString,
	fs::{ObjectType, UuidStr},
};

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
	Connect = b'0',
	Disconnect = b'1',
	Ping = b'2',
	Pong = b'3',
	Message = b'4',
	Upgrade = b'5',
	Noop = b'6',
}

impl TryFrom<u8> for PacketType {
	type Error = u8;

	fn try_from(value: u8) -> Result<Self, u8> {
		match value {
			b'0' => Ok(PacketType::Connect),
			b'1' => Ok(PacketType::Disconnect),
			b'2' => Ok(PacketType::Ping),
			b'3' => Ok(PacketType::Pong),
			b'4' => Ok(PacketType::Message),
			b'5' => Ok(PacketType::Upgrade),
			b'6' => Ok(PacketType::Noop),
			other => Err(other),
		}
	}
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
	Connect = b'0',
	Disconnect = b'1',
	Event = b'2',
	Ack = b'3',
	Error = b'4',
	BinaryEvent = b'5',
	BinaryAck = b'6',
}

impl TryFrom<u8> for MessageType {
	type Error = u8;

	fn try_from(value: u8) -> Result<Self, u8> {
		match value {
			b'0' => Ok(MessageType::Connect),
			b'1' => Ok(MessageType::Disconnect),
			b'2' => Ok(MessageType::Event),
			b'3' => Ok(MessageType::Ack),
			b'4' => Ok(MessageType::Error),
			b'5' => Ok(MessageType::BinaryEvent),
			b'6' => Ok(MessageType::BinaryAck),
			other => Err(other),
		}
	}
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HandShake<'a> {
	pub sid: Cow<'a, str>,
	pub upgrades: Vec<Cow<'a, str>>,
	pub ping_interval: u64,
	pub ping_timeout: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints, hashmap_as_object)
)]
pub enum SocketEvent<'a> {
	AuthSuccess,
	#[serde(borrow)]
	NewEvent(NewEvent<'a>),
	#[serde(borrow)]
	FileRename(FileRename<'a>),
	#[serde(borrow)]
	FileArchiveRestored(FileArchiveRestored<'a>),
	#[serde(borrow)]
	FileNew(FileNew<'a>),
	#[serde(borrow)]
	FileRestore(FileRestore<'a>),
	#[serde(borrow)]
	FileMove(FileMove<'a>),
	FileTrash(FileTrash),
	FileArchived(FileArchived),
	#[serde(borrow)]
	FolderRename(FolderRename<'a>),
	FolderTrash(FolderTrash),
	#[serde(borrow)]
	FolderMove(FolderMove<'a>),
	#[serde(borrow)]
	FolderSubCreated(FolderSubCreated<'a>),
	#[serde(borrow)]
	FolderRestore(FolderRestore<'a>),
	#[serde(borrow)]
	FolderColorChanged(FolderColorChanged<'a>),
	TrashEmpty,
	PasswordChanged,
	#[serde(borrow)]
	ChatMessageNew(ChatMessageNew<'a>),
	#[serde(borrow)]
	ChatTyping(ChatTyping<'a>),
	#[serde(borrow)]
	ChatConversationsNew(ChatConversationsNew<'a>),
	ChatMessageDelete(ChatMessageDelete),
	#[serde(borrow)]
	NoteContentEdited(NoteContentEdited<'a>),
	NoteArchived(NoteArchived),
	NoteDeleted(NoteDeleted),
	#[serde(borrow)]
	NoteTitleEdited(NoteTitleEdited<'a>),
	NoteParticipantPermissions(NoteParticipantPermissions),
	NoteRestored(NoteRestored),
	NoteParticipantRemoved(NoteParticipantRemoved),
	#[serde(borrow)]
	NoteParticipantNew(NoteParticipantNew<'a>),
	NoteNew(NoteNew),
	ChatMessageEmbedDisabled(ChatMessageEmbedDisabled),
	ChatConversationParticipantLeft(ChatConversationParticipantLeft),
	ChatConversationDeleted(ChatConversationDeleted),
	#[serde(borrow)]
	ChatMessageEdited(ChatMessageEdited<'a>),
	#[serde(borrow)]
	ChatConversationNameEdited(ChatConversationNameEdited<'a>),
	#[serde(borrow)]
	ContactRequestReceived(ContactRequestReceived<'a>),
	#[serde(borrow)]
	ItemFavorite(ItemFavorite<'a>),
	#[serde(borrow)]
	ChatConversationParticipantNew(ChatConversationParticipantNew<'a>),
	FileDeletedPermanent(FileDeletedPermanent),
	#[serde(borrow)]
	FolderMetadataChanged(FolderMetadataChanged<'a>),
	FolderDeletedPermanent(FolderDeletedPermanent),
	#[serde(borrow)]
	FileMetadataChanged(FileMetadataChanged<'a>),
}

impl SocketEvent<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			SocketEvent::AuthSuccess => "authSuccess",
			SocketEvent::NewEvent(_) => "newEvent",
			SocketEvent::FileRename(_) => "fileRename",
			SocketEvent::FileArchiveRestored(_) => "fileArchiveRestored",
			SocketEvent::FileNew(_) => "fileNew",
			SocketEvent::FileRestore(_) => "fileRestore",
			SocketEvent::FileMove(_) => "fileMove",
			SocketEvent::FileTrash(_) => "fileTrash",
			SocketEvent::FileArchived(_) => "fileArchived",
			SocketEvent::FolderRename(_) => "folderRename",
			SocketEvent::FolderTrash(_) => "folderTrash",
			SocketEvent::FolderMove(_) => "folderMove",
			SocketEvent::FolderSubCreated(_) => "folderSubCreated",
			SocketEvent::FolderRestore(_) => "folderRestore",
			SocketEvent::FolderColorChanged(_) => "folderColorChanged",
			SocketEvent::TrashEmpty => "trashEmpty",
			SocketEvent::PasswordChanged => "passwordChanged",
			SocketEvent::ChatMessageNew(_) => "chatMessageNew",
			SocketEvent::ChatTyping(_) => "chatTyping",
			SocketEvent::ChatConversationsNew(_) => "chatConversationsNew",
			SocketEvent::ChatMessageDelete(_) => "chatMessageDelete",
			SocketEvent::NoteContentEdited(_) => "noteContentEdited",
			SocketEvent::NoteArchived(_) => "noteArchived",
			SocketEvent::NoteDeleted(_) => "noteDeleted",
			SocketEvent::NoteTitleEdited(_) => "noteTitleEdited",
			SocketEvent::NoteParticipantPermissions(_) => "noteParticipantPermissions",
			SocketEvent::NoteRestored(_) => "noteRestored",
			SocketEvent::NoteParticipantRemoved(_) => "noteParticipantRemoved",
			SocketEvent::NoteParticipantNew(_) => "noteParticipantNew",
			SocketEvent::NoteNew(_) => "noteNew",
			SocketEvent::ChatMessageEmbedDisabled(_) => "chatMessageEmbedDisabled",
			SocketEvent::ChatConversationParticipantLeft(_) => "chatConversationParticipantLeft",
			SocketEvent::ChatConversationDeleted(_) => "chatConversationDeleted",
			SocketEvent::ChatMessageEdited(_) => "chatMessageEdited",
			SocketEvent::ChatConversationNameEdited(_) => "chatConversationNameEdited",
			SocketEvent::ContactRequestReceived(_) => "contactRequestReceived",
			SocketEvent::ItemFavorite(_) => "itemFavorite",
			SocketEvent::ChatConversationParticipantNew(_) => "chatConversationParticipantNew",
			SocketEvent::FileDeletedPermanent(_) => "fileDeletedPermanent",
			SocketEvent::FolderMetadataChanged(_) => "folderMetadataChanged",
			SocketEvent::FolderDeletedPermanent(_) => "folderDeletedPermanent",
			SocketEvent::FileMetadataChanged(_) => "fileMetadataChanged",
		}
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)]
const TS_SOCKET_EVENT_TYPE: &str = r#"export type SocketEventType = SocketEvent["type"]"#;

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct NewEvent<'a> {
	pub uuid: UuidStr,
	#[serde(rename = "type")]
	#[serde(borrow)]
	pub event_type: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	#[serde(borrow)]
	pub info: Cow<'a, str>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FileRename<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FileArchiveRestored<'a> {
	pub current_uuid: UuidStr,
	pub parent: UuidStr,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	// #[serde(borrow)]
	// rm: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	#[serde(borrow)]
	pub bucket: Cow<'a, str>,
	#[serde(borrow)]
	pub region: Cow<'a, str>,
	pub version: FileEncryptionVersion,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub favorited: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FileNew<'a> {
	pub parent: UuidStr,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	// #[serde(borrow)]
	// rm: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	#[serde(borrow)]
	pub bucket: Cow<'a, str>,
	#[serde(borrow)]
	pub region: Cow<'a, str>,
	pub version: FileEncryptionVersion,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub favorited: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FileRestore<'a> {
	pub parent: UuidStr,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	// #[serde(borrow)]
	// rm: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	#[serde(borrow)]
	pub bucket: Cow<'a, str>,
	#[serde(borrow)]
	pub region: Cow<'a, str>,
	pub version: FileEncryptionVersion,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub favorited: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FileMove<'a> {
	pub parent: UuidStr,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	// #[serde(borrow)]
	// pub rm: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	#[serde(borrow)]
	pub bucket: Cow<'a, str>,
	#[serde(borrow)]
	pub region: Cow<'a, str>,
	pub version: FileEncryptionVersion,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub favorited: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FileTrash {
	pub uuid: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FileArchived {
	pub uuid: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderRename<'a> {
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
	pub uuid: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderTrash {
	pub parent: UuidStr,
	pub uuid: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderMove<'a> {
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
	pub uuid: UuidStr,
	pub parent: UuidStr,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub favorited: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderSubCreated<'a> {
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
	pub uuid: UuidStr,
	pub parent: UuidStr,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub favorited: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderRestore<'a> {
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
	pub uuid: UuidStr,
	pub parent: UuidStr,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub favorited: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderColorChanged<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub color: DirColor<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints, hashmap_as_object)
)]
pub struct ChatMessageNew<'a> {
	#[serde(rename = "conversation")]
	pub chat: UuidStr,
	#[serde(flatten, borrow)]
	pub inner: ChatMessagePartialEncrypted<'a>,
	#[serde(
		borrow,
		deserialize_with = "crate::serde::option::result_to_option::deserialize",
		skip_serializing_if = "Option::is_none"
	)]
	// #[cfg_attr(all(target_family = "wasm", target_os = "unknown"), tsify(optional))]
	pub reply_to: Option<ChatMessagePartialEncrypted<'a>>,
	pub embed_disabled: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub sent_timestamp: DateTime<Utc>,
}

impl<'a> From<ChatMessageNew<'a>> for ChatMessageEncrypted<'a> {
	fn from(value: ChatMessageNew<'a>) -> Self {
		Self {
			chat: value.chat,
			inner: value.inner,
			reply_to: value.reply_to,
			embed_disabled: value.embed_disabled,
			edited: false,
			edited_timestamp: DateTime::<Utc>::default(),
			sent_timestamp: value.sent_timestamp,
		}
	}
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatTyping<'a> {
	#[serde(rename = "conversation")]
	pub chat: UuidStr,
	#[serde(borrow)]
	pub sender_avatar: Option<Cow<'a, str>>,
	#[serde(borrow)]
	pub sender_email: Cow<'a, str>,
	#[serde(borrow)]
	pub sender_nick_name: Cow<'a, str>,
	pub sender_id: u64,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	#[serde(rename = "type")]
	pub typing_type: ChatTypingType,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatConversationsNew<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub added_timestamp: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatMessageDelete {
	pub uuid: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct NoteContentEdited<'a> {
	pub note: UuidStr,
	pub content: EncryptedString<'a>,
	#[serde(rename = "type")]
	pub note_type: NoteType,
	pub editor_id: u64,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub edited_timestamp: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct NoteArchived {
	pub note: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct NoteDeleted {
	pub note: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct NoteTitleEdited<'a> {
	pub note: UuidStr,
	#[serde(borrow)]
	pub title: EncryptedString<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct NoteParticipantPermissions {
	pub note: UuidStr,
	pub user_id: u64,
	pub permissions_write: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct NoteRestored {
	pub note: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct NoteParticipantRemoved {
	pub note: UuidStr,
	pub user_id: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints, hashmap_as_object)
)]
pub struct NoteParticipantNew<'a> {
	pub note: UuidStr,
	#[serde(flatten, borrow)]
	pub participant: NoteParticipant<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct NoteNew {
	pub note: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatMessageEmbedDisabled {
	pub uuid: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatConversationParticipantLeft {
	pub uuid: UuidStr,
	pub user_id: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatConversationDeleted {
	pub uuid: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatMessageEdited<'a> {
	#[serde(rename = "conversation")]
	pub chat: UuidStr,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub message: EncryptedString<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub edited_timestamp: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatConversationNameEdited<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct ContactRequestReceived<'a> {
	pub uuid: UuidStr,
	pub sender_id: u64,
	#[serde(borrow)]
	pub sender_email: Cow<'a, str>,
	#[serde(borrow)]
	pub sender_avatar: Option<Cow<'a, str>>,
	#[serde(borrow)]
	pub sender_nick_name: Option<Cow<'a, str>>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub sent_timestamp: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct ItemFavorite<'a> {
	pub uuid: UuidStr,
	#[serde(rename = "type")]
	pub item_type: ObjectType,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub value: bool,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatConversationParticipantNew<'a> {
	#[serde(rename = "conversation")]
	pub chat: UuidStr,
	pub user_id: u64,
	#[serde(borrow)]
	pub email: Cow<'a, str>,
	#[serde(borrow)]
	pub avatar: Option<Cow<'a, str>>,
	#[serde(borrow)]
	pub nick_name: Option<Cow<'a, str>>,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	pub permissions_add: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub added_timestamp: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FileDeletedPermanent {
	pub uuid: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderMetadataChanged<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FileMetadataChanged<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	#[serde(borrow)]
	pub old_metadata: EncryptedString<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderDeletedPermanent {
	pub uuid: UuidStr,
}
