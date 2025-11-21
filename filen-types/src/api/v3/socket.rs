use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::borrow::Cow;
use yoke::Yokeable;

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
	fs::{ObjectType, ParentUuid, UuidStr},
	traits::CowHelpers,
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

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct HandShake<'a> {
	#[serde(borrow)]
	pub sid: Cow<'a, str>,
	#[serde(borrow)]
	pub upgrades: Vec<Cow<'a, str>>,
	pub ping_interval: u64,
	pub ping_timeout: u64,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq, CowHelpers, Yokeable)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
#[cfg_attr(
	all(
		target_family = "wasm",
		target_os = "unknown",
		not(feature = "service-worker")
	),
	derive(tsify::Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints, hashmap_as_object)
)]
pub enum SocketEvent<'a> {
	/// Sent after successful authentication, including on reconnect
	AuthSuccess,
	/// Sent after failed authentication, including on reconnect, after which the socket is closed and all listeners removed
	AuthFailed,
	/// Sent when the socket has unexpectedly closed and begins attempting to reconnect
	Reconnecting,
	/// Sent when the handle to the event listener has been dropped and the listener is removed
	Unsubscribed,
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

impl<'de> Deserialize<'de> for SocketEvent<'de> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		struct SocketEventVisitor;

		impl<'de> serde::de::Visitor<'de> for SocketEventVisitor {
			type Value = SocketEvent<'de>;

			fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
				formatter.write_str("a tuple of [event_name, optional_event_data]")
			}

			fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
			where
				A: serde::de::SeqAccess<'de>,
			{
				let event_name = seq
					.next_element::<crate::serde::cow::CowStrWrapper>()?
					.ok_or_else(|| serde::de::Error::invalid_length(0, &self))?
					.0;
				let event_name = kebab_to_camel(event_name);

				let event = match event_name.as_ref() {
					"authSuccess" => Some(SocketEvent::AuthSuccess),
					"authFailed" => Some(SocketEvent::AuthFailed),
					"reconnecting" => Some(SocketEvent::Reconnecting),
					"trashEmpty" => Some(SocketEvent::TrashEmpty),
					"passwordChanged" => Some(SocketEvent::PasswordChanged),
					"newEvent" => seq.next_element()?.map(SocketEvent::NewEvent),
					"fileRename" => seq.next_element()?.map(SocketEvent::FileRename),
					"fileArchiveRestored" => {
						seq.next_element()?.map(SocketEvent::FileArchiveRestored)
					}
					"fileNew" => seq.next_element()?.map(SocketEvent::FileNew),
					"fileRestore" => seq.next_element()?.map(SocketEvent::FileRestore),
					"fileMove" => seq.next_element()?.map(SocketEvent::FileMove),
					"fileTrash" => seq.next_element()?.map(SocketEvent::FileTrash),
					"fileArchived" => seq.next_element()?.map(SocketEvent::FileArchived),
					"folderRename" => seq.next_element()?.map(SocketEvent::FolderRename),
					"folderTrash" => seq.next_element()?.map(SocketEvent::FolderTrash),
					"folderMove" => seq.next_element()?.map(SocketEvent::FolderMove),
					"folderSubCreated" => seq.next_element()?.map(SocketEvent::FolderSubCreated),
					"folderRestore" => seq.next_element()?.map(SocketEvent::FolderRestore),
					"folderColorChanged" => {
						seq.next_element()?.map(SocketEvent::FolderColorChanged)
					}
					"chatMessageNew" => seq.next_element()?.map(SocketEvent::ChatMessageNew),
					"chatTyping" => seq.next_element()?.map(SocketEvent::ChatTyping),
					"chatConversationsNew" => {
						seq.next_element()?.map(SocketEvent::ChatConversationsNew)
					}
					"chatMessageDelete" => seq.next_element()?.map(SocketEvent::ChatMessageDelete),
					"noteContentEdited" => seq.next_element()?.map(SocketEvent::NoteContentEdited),
					"noteArchived" => seq.next_element()?.map(SocketEvent::NoteArchived),
					"noteDeleted" => seq.next_element()?.map(SocketEvent::NoteDeleted),
					"noteTitleEdited" => seq.next_element()?.map(SocketEvent::NoteTitleEdited),
					"noteParticipantPermissions" => seq
						.next_element()?
						.map(SocketEvent::NoteParticipantPermissions),
					"noteRestored" => seq.next_element()?.map(SocketEvent::NoteRestored),
					"noteParticipantRemoved" => {
						seq.next_element()?.map(SocketEvent::NoteParticipantRemoved)
					}
					"noteParticipantNew" => {
						seq.next_element()?.map(SocketEvent::NoteParticipantNew)
					}
					"noteNew" => seq.next_element()?.map(SocketEvent::NoteNew),
					"chatMessageEmbedDisabled" => seq
						.next_element()?
						.map(SocketEvent::ChatMessageEmbedDisabled),
					"chatConversationParticipantLeft" => seq
						.next_element()?
						.map(SocketEvent::ChatConversationParticipantLeft),
					"chatConversationDeleted" => seq
						.next_element()?
						.map(SocketEvent::ChatConversationDeleted),
					"chatMessageEdited" => seq.next_element()?.map(SocketEvent::ChatMessageEdited),
					"chatConversationNameEdited" => seq
						.next_element()?
						.map(SocketEvent::ChatConversationNameEdited),
					"contactRequestReceived" => {
						seq.next_element()?.map(SocketEvent::ContactRequestReceived)
					}
					"itemFavorite" => seq.next_element()?.map(SocketEvent::ItemFavorite),
					"chatConversationParticipantNew" => seq
						.next_element()?
						.map(SocketEvent::ChatConversationParticipantNew),
					"fileDeletedPermanent" => {
						seq.next_element()?.map(SocketEvent::FileDeletedPermanent)
					}
					"folderMetadataChanged" => {
						seq.next_element()?.map(SocketEvent::FolderMetadataChanged)
					}
					"folderDeletedPermanent" => {
						seq.next_element()?.map(SocketEvent::FolderDeletedPermanent)
					}
					"fileMetadataChanged" => {
						seq.next_element()?.map(SocketEvent::FileMetadataChanged)
					}
					unknown => {
						return Err(serde::de::Error::custom(format!(
							"unknown event type: {}",
							unknown
						)));
					}
				}
				.ok_or_else(|| {
					serde::de::Error::custom(format!("missing data for event type: {}", event_name))
				})?;

				Ok(event)
			}
		}

		deserializer.deserialize_seq(SocketEventVisitor)
	}
}

fn kebab_to_camel<'a>(event_name: impl Into<Cow<'a, str>>) -> Cow<'a, str> {
	let mut event_name = event_name.into();
	let mut curr_idx = 0;

	while let Some(i) = event_name[curr_idx..].find('-') {
		let mut_string = event_name.to_mut();
		mut_string.remove(curr_idx + i);

		if mut_string[curr_idx + i..]
			.chars()
			.next()
			.is_some_and(|c| c.is_ascii_lowercase())
		{
			mut_string[curr_idx + i..=curr_idx + i].make_ascii_uppercase();
		}
		curr_idx += i;
	}
	event_name
}

impl SocketEvent<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			SocketEvent::AuthSuccess => "authSuccess",
			SocketEvent::AuthFailed => "authFailed",
			SocketEvent::Reconnecting => "reconnecting",
			SocketEvent::Unsubscribed => "unsubscribed",
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

#[cfg(all(
	target_family = "wasm",
	target_os = "unknown",
	not(feature = "service-worker")
))]
#[wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)]
const TS_SOCKET_EVENT_TYPE: &str = r#"export type SocketEventType = SocketEvent["type"]"#;

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
pub struct NewEvent<'a> {
	pub uuid: UuidStr,
	#[serde(rename = "type")]
	#[serde(borrow)]
	pub event_type: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	#[serde(borrow)]
	pub info: Cow<'a, str>,
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
pub struct FileRename<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
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
pub struct FileArchiveRestored<'a> {
	#[serde(rename = "currentUUID")]
	pub current_uuid: UuidStr,
	pub parent: ParentUuid,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	// #[serde(borrow)]
	// rm: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
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
pub struct FileNew<'a> {
	pub parent: ParentUuid,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	// #[serde(borrow)]
	// rm: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
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
pub struct FileRestore<'a> {
	pub parent: ParentUuid,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	// #[serde(borrow)]
	// rm: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
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
pub struct FileMove<'a> {
	pub parent: ParentUuid,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	// #[serde(borrow)]
	// pub rm: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
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
	all(
		target_family = "wasm",
		target_os = "unknown",
		not(feature = "service-worker")
	),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct FileTrash {
	pub uuid: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct FileArchived {
	pub uuid: UuidStr,
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
pub struct FolderRename<'a> {
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
	pub uuid: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct FolderTrash {
	pub parent: UuidStr,
	pub uuid: UuidStr,
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
pub struct FolderMove<'a> {
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
	pub uuid: UuidStr,
	pub parent: ParentUuid,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub favorited: bool,
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
pub struct FolderSubCreated<'a> {
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
	pub uuid: UuidStr,
	pub parent: ParentUuid,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub favorited: bool,
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
pub struct FolderRestore<'a> {
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
	pub uuid: UuidStr,
	pub parent: ParentUuid,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub favorited: bool,
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
pub struct FolderColorChanged<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub color: DirColor<'a>,
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
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	#[serde(rename = "type")]
	pub typing_type: ChatTypingType,
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
pub struct ChatConversationsNew<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	pub added_timestamp: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ChatMessageDelete {
	pub uuid: UuidStr,
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
pub struct NoteContentEdited<'a> {
	pub note: UuidStr,
	pub content: EncryptedString<'a>,
	#[serde(rename = "type")]
	pub note_type: NoteType,
	pub editor_id: u64,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	pub edited_timestamp: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct NoteArchived {
	pub note: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct NoteDeleted {
	pub note: UuidStr,
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
pub struct NoteTitleEdited<'a> {
	pub note: UuidStr,
	#[serde(borrow)]
	pub title: EncryptedString<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct NoteParticipantPermissions {
	pub note: UuidStr,
	pub user_id: u64,
	pub permissions_write: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct NoteRestored {
	pub note: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct NoteParticipantRemoved {
	pub note: UuidStr,
	pub user_id: u64,
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
	all(
		target_family = "wasm",
		target_os = "unknown",
		not(feature = "service-worker")
	),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct NoteNew {
	pub note: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ChatMessageEmbedDisabled {
	pub uuid: UuidStr,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ChatConversationParticipantLeft {
	pub uuid: UuidStr,
	pub user_id: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ChatConversationDeleted {
	pub uuid: UuidStr,
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
pub struct ChatMessageEdited<'a> {
	#[serde(rename = "conversation")]
	pub chat: UuidStr,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub message: EncryptedString<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	pub edited_timestamp: DateTime<Utc>,
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
pub struct ChatConversationNameEdited<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
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
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
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
pub struct ItemFavorite<'a> {
	pub uuid: UuidStr,
	#[serde(rename = "type")]
	pub item_type: ObjectType,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub value: bool,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
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
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	pub added_timestamp: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct FileDeletedPermanent {
	pub uuid: UuidStr,
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
pub struct FolderMetadataChanged<'a> {
	pub uuid: UuidStr,
	#[serde(borrow, rename = "name")]
	pub meta: EncryptedString<'a>,
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
	all(
		target_family = "wasm",
		target_os = "unknown",
		not(feature = "service-worker")
	),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct FolderDeletedPermanent {
	pub uuid: UuidStr,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn camelify_name_from_kebab() {
		assert_eq!(kebab_to_camel("file-rename"), "fileRename");
		assert_eq!(
			kebab_to_camel("file-archive-restored"),
			"fileArchiveRestored"
		);
		assert_eq!(kebab_to_camel("auth-success"), "authSuccess");
		assert_eq!(kebab_to_camel("simpleevent"), "simpleevent");
		assert_eq!(kebab_to_camel("simpleEvent"), "simpleEvent");
		assert_eq!(kebab_to_camel("-----"), "");
		assert_eq!(kebab_to_camel("-----a"), "A");
		assert_eq!(kebab_to_camel("-----aaa"), "Aaa");
	}
}
