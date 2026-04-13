use chrono::{DateTime, Utc};
use serde::{
	Deserialize, Deserializer, Serialize,
	de::{DeserializeSeed, MapAccess, Visitor},
};
use serde_json::value::RawValue;
use std::{borrow::Cow, fmt::Formatter, marker::PhantomData};
use yoke::Yokeable;

use crate::{
	api::v3::{
		chat::{
			conversations::ChatConversationParticipant,
			messages::{ChatMessageEncrypted, ChatMessagePartialEncrypted},
			typing::ChatTypingType,
		},
		dir::color::DirColor,
		notes::{NoteParticipant, NoteType},
	},
	auth::FileEncryptionVersion,
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	fs::{ObjectType, ParentUuid, UuidStr},
	serde::cow::CowStrWrapper,
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

/// Wraps a serde_json object deserializer, injecting a `"type"` tag entry
/// before the object's own entries. Presents wire-format data
/// `["event-name", {fields...}]` as internally-tagged `{"type": "eventName", fields...}`.
pub(crate) struct InjectTagDeserializer<'de> {
	pub variant: Cow<'de, str>,
	pub data: &'de RawValue,
}

impl<'de> Deserializer<'de> for InjectTagDeserializer<'de> {
	type Error = serde_json::Error;

	fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
		self.deserialize_map(visitor)
	}

	fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
		let wrapper = InjectTagVisitor {
			inner: visitor,
			variant: self.variant,
		};
		let mut de = serde_json::Deserializer::from_str(self.data.get());
		de.deserialize_map(wrapper)
	}

	fn deserialize_struct<V>(
		self,
		_name: &'static str,
		_fields: &'static [&'static str],
		visitor: V,
	) -> Result<V::Value, Self::Error>
	where
		V: Visitor<'de>,
	{
		self.deserialize_map(visitor)
	}

	serde::forward_to_deserialize_any! {
		bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
		bytes byte_buf option unit unit_struct newtype_struct seq tuple
		tuple_struct enum identifier ignored_any
	}
}

/// Intercepts serde_json's visit_map to wrap the MapAccess with tag injection.
struct InjectTagVisitor<'de, V> {
	inner: V,
	variant: Cow<'de, str>,
}

impl<'de, V: Visitor<'de>> Visitor<'de> for InjectTagVisitor<'de, V> {
	type Value = V::Value;

	fn expecting(&self, f: &mut Formatter) -> std::fmt::Result {
		self.inner.expecting(f)
	}

	fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<V::Value, A::Error> {
		self.inner.visit_map(InjectTagMapAccess {
			variant: Some(self.variant),
			inner: map,
		})
	}
}

/// MapAccess wrapper that prepends a "type" entry, then delegates to inner.
struct InjectTagMapAccess<'de, A> {
	variant: Option<Cow<'de, str>>,
	inner: A,
}

impl<'de, A: MapAccess<'de>> MapAccess<'de> for InjectTagMapAccess<'de, A> {
	type Error = A::Error;

	fn next_key_seed<K: DeserializeSeed<'de>>(
		&mut self,
		seed: K,
	) -> Result<Option<K::Value>, Self::Error> {
		if self.variant.is_some() {
			// Yield "type" as the first key
			seed.deserialize(serde::de::value::BorrowedStrDeserializer::new("enum_type"))
				.map(Some)
		} else {
			self.inner.next_key_seed(seed)
		}
	}

	fn next_value_seed<V: DeserializeSeed<'de>>(
		&mut self,
		seed: V,
	) -> Result<V::Value, Self::Error> {
		match self.variant.take() {
			// Yield the variant name as the value for "type"
			Some(variant) => seed.deserialize(CowStrDeserializer(variant, PhantomData)),
			// Delegate to serde_json for all real data fields
			None => self.inner.next_value_seed(seed),
		}
	}
}

/// Deserializes a Cow<str>, preserving borrowed vs owned.
struct CowStrDeserializer<'de, E>(Cow<'de, str>, PhantomData<E>);

impl<'de, E: serde::de::Error> Deserializer<'de> for CowStrDeserializer<'de, E> {
	type Error = E;

	fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
		match self.0 {
			Cow::Borrowed(s) => visitor.visit_borrowed_str(s),
			Cow::Owned(s) => visitor.visit_string(s),
		}
	}

	serde::forward_to_deserialize_any! {
		bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
		bytes byte_buf option unit unit_struct newtype_struct seq tuple
		tuple_struct map struct enum identifier ignored_any
	}
}

macro_rules! extract_id {
	($raw:expr, $field:literal) => {{
		#[derive(Deserialize)]
		struct ExtractId {
			#[serde(rename = $field)]
			id: u64,
		}
		serde_json::from_str::<ExtractId>($raw.get()).map(|h| h.id)
	}};
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventCategory {
	Drive,
	Chat,
	Note,
	Contact,
	General,
}

/// Maps the raw wire event name to its category and the camelCase serde variant name.
fn classify_event(name: &str) -> Option<(EventCategory, &'static str)> {
	use EventCategory::*;
	Some(match name {
		// Drive events (kebab-case from backend)
		"file-new" => (Drive, "fileNew"),
		"file-move" => (Drive, "fileMove"),
		"file-trash" => (Drive, "fileTrash"),
		"file-rename" => (Drive, "fileRename"),
		"file-restore" => (Drive, "fileRestore"),
		"file-versioned" => (Drive, "fileArchived"),
		"file-archive-restored" => (Drive, "fileArchiveRestored"),
		"file-deleted-permanent" => (Drive, "fileDeletedPermanent"),
		"file-metadata-changed" => (Drive, "fileMetadataChanged"),
		"folder-sub-created" => (Drive, "folderSubCreated"),
		"folder-move" => (Drive, "folderMove"),
		"folder-trash" => (Drive, "folderTrash"),
		"folder-rename" => (Drive, "folderRename"),
		"folder-restore" => (Drive, "folderRestore"),
		"folder-color-changed" => (Drive, "folderColorChanged"),
		"folder-metadata-changed" => (Drive, "folderMetadataChanged"),
		"folder-deleted-permanent" => (Drive, "folderDeletedPermanent"),
		"item-favorite" => (Drive, "itemFavorite"),
		"trash-empty" => (Drive, "trashEmpty"),
		"deleteAll" => (Drive, "deleteAll"),
		"deleteVersioned" => (Drive, "deleteVersioned"),

		// Chat events (camelCase from backend)
		"chatMessageNew" => (Chat, "chatMessageNew"),
		"chatMessageDelete" => (Chat, "chatMessageDelete"),
		"chatMessageEdited" => (Chat, "chatMessageEdited"),
		"chatMessageEmbedDisabled" => (Chat, "chatMessageEmbedDisabled"),
		"chatTyping" => (Chat, "chatTyping"),
		"chatConversationsNew" => (Chat, "chatConversationsNew"),
		"chatConversationDeleted" => (Chat, "chatConversationDeleted"),
		"chatConversationNameEdited" => (Chat, "chatConversationNameEdited"),
		"chatConversationParticipantLeft" => (Chat, "chatConversationParticipantLeft"),
		"chatConversationParticipantNew" => (Chat, "chatConversationParticipantNew"),

		// Note events (camelCase from backend)
		"noteNew" => (Note, "noteNew"),
		"noteArchived" => (Note, "noteArchived"),
		"noteDeleted" => (Note, "noteDeleted"),
		"noteRestored" => (Note, "noteRestored"),
		"noteContentEdited" => (Note, "noteContentEdited"),
		"noteTitleEdited" => (Note, "noteTitleEdited"),
		"noteParticipantNew" => (Note, "noteParticipantNew"),
		"noteParticipantRemoved" => (Note, "noteParticipantRemoved"),
		"noteParticipantPermissions" => (Note, "noteParticipantPermissions"),

		// Contact events
		"contactRequestReceived" => (Contact, "contactRequestReceived"),

		// General events
		"new-event" => (General, "newEvent"),
		"passwordChanged" => (General, "passwordChanged"),

		_ => return None,
	})
}

// ── Per-category event enums ────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(tag = "enum_type", rename_all = "camelCase")]
pub enum DriveEventType<'a> {
	FileArchived(FileArchived),
	#[serde(borrow)]
	FileArchiveRestored(FileArchiveRestored<'a>),
	FileDeletedPermanent(FileDeletedPermanent),
	#[serde(borrow)]
	FileMetadataChanged(FileMetadataChanged<'a>),
	#[serde(borrow)]
	FileRename(FileRename<'a>),
	#[serde(borrow)]
	FileMove(FileMove<'a>),
	#[serde(borrow)]
	FileNew(FileNew<'a>),
	#[serde(borrow)]
	FileRestore(FileRestore<'a>),
	FileTrash(FileTrash),

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
	#[serde(borrow)]
	FolderMetadataChanged(FolderMetadataChanged<'a>),
	FolderDeletedPermanent(FolderDeletedPermanent),

	#[serde(borrow)]
	ItemFavorite(ItemFavorite<'a>),

	TrashEmpty,
	DeleteAll,
	DeleteVersioned,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(tag = "enum_type", rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)]
pub enum ChatEventType<'a> {
	#[serde(borrow)]
	ChatMessageNew(ChatMessageNew<'a>),
	#[serde(borrow)]
	ChatTyping(ChatTyping<'a>),
	#[serde(borrow)]
	ChatConversationsNew(ChatConversationsNew<'a>),
	ChatMessageDelete(ChatMessageDelete),
	ChatMessageEmbedDisabled(ChatMessageEmbedDisabled),
	ChatConversationParticipantLeft(ChatConversationParticipantLeft),
	ChatConversationDeleted(ChatConversationDeleted),
	#[serde(borrow)]
	ChatMessageEdited(ChatMessageEdited<'a>),
	#[serde(borrow)]
	ChatConversationNameEdited(ChatConversationNameEdited<'a>),
	#[serde(borrow)]
	ChatConversationParticipantNew(ChatConversationParticipantNew<'a>),
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(tag = "enum_type", rename_all = "camelCase")]
pub enum NoteEventType<'a> {
	NoteArchived(NoteArchived),
	#[serde(borrow)]
	NoteContentEdited(NoteContentEdited<'a>),
	NoteDeleted(NoteDeleted),
	#[serde(borrow)]
	NoteTitleEdited(NoteTitleEdited<'a>),
	NoteParticipantPermissions(NoteParticipantPermissions),
	NoteRestored(NoteRestored),
	NoteParticipantRemoved(NoteParticipantRemoved),
	#[serde(borrow)]
	NoteParticipantNew(NoteParticipantNew<'a>),
	#[serde(borrow)]
	NoteNew(NoteNew<'a>),
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(tag = "enum_type", rename_all = "camelCase")]
pub enum ContactEventType<'a> {
	#[serde(borrow)]
	ContactRequestReceived(ContactRequestReceived<'a>),
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(tag = "enum_type", rename_all = "camelCase")]
pub enum GeneralEventType<'a> {
	PasswordChanged,
	#[serde(borrow)]
	NewEvent(NewEvent<'a>),
}

// ── Top-level event ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers, Yokeable)]
pub enum SocketEvent<'a> {
	Drive {
		inner: DriveEventType<'a>,
		drive_message_id: u64,
	},
	Chat {
		inner: ChatEventType<'a>,
		chat_message_id: u64,
	},
	Note {
		inner: NoteEventType<'a>,
		note_message_id: u64,
	},
	Contact {
		inner: ContactEventType<'a>,
		contact_message_id: u64,
	},
	General {
		inner: GeneralEventType<'a>,
		general_message_id: u64,
	},
}

impl<'de> Deserialize<'de> for SocketEvent<'de> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		struct SocketEventVisitor;

		impl<'de> serde::de::Visitor<'de> for SocketEventVisitor {
			type Value = SocketEvent<'de>;

			fn expecting(&self, formatter: &mut Formatter) -> std::fmt::Result {
				formatter.write_str("a tuple of [event_name, event_data]")
			}

			fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
			where
				A: serde::de::SeqAccess<'de>,
			{
				let event_name: CowStrWrapper = seq
					.next_element()?
					.ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
				let raw: &'de RawValue = seq
					.next_element()?
					.ok_or_else(|| serde::de::Error::invalid_length(1, &self))?;

				let (category, variant) =
					classify_event(event_name.0.as_ref()).ok_or_else(|| {
						serde::de::Error::custom(format!(
							"unknown socket event type: {}",
							event_name.0
						))
					})?;
				let inject = InjectTagDeserializer {
					variant: Cow::Borrowed(variant),
					data: raw,
				};

				let event = match category {
					EventCategory::Drive => {
						let inner = DriveEventType::deserialize(inject)
							.map_err(serde::de::Error::custom)?;
						let drive_message_id =
							extract_id!(raw, "driveMessageId").map_err(serde::de::Error::custom)?;
						SocketEvent::Drive {
							inner,
							drive_message_id,
						}
					}
					EventCategory::Chat => {
						let inner =
							ChatEventType::deserialize(inject).map_err(serde::de::Error::custom)?;
						let chat_message_id =
							extract_id!(raw, "chatMessageId").map_err(serde::de::Error::custom)?;
						SocketEvent::Chat {
							inner,
							chat_message_id,
						}
					}
					EventCategory::Note => {
						let inner =
							NoteEventType::deserialize(inject).map_err(serde::de::Error::custom)?;
						let note_message_id =
							extract_id!(raw, "noteMessageId").map_err(serde::de::Error::custom)?;
						SocketEvent::Note {
							inner,
							note_message_id,
						}
					}
					EventCategory::Contact => {
						let inner = ContactEventType::deserialize(inject)
							.map_err(serde::de::Error::custom)?;
						let contact_message_id = extract_id!(raw, "contactMessageId")
							.map_err(serde::de::Error::custom)?;
						SocketEvent::Contact {
							inner,
							contact_message_id,
						}
					}
					EventCategory::General => {
						let inner = GeneralEventType::deserialize(inject)
							.map_err(serde::de::Error::custom)?;
						let general_message_id = extract_id!(raw, "generalMessageId")
							.map_err(serde::de::Error::custom)?;
						SocketEvent::General {
							inner,
							general_message_id,
						}
					}
				};

				Ok(event)
			}
		}

		deserializer.deserialize_seq(SocketEventVisitor)
	}
}

impl DriveEventType<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			Self::FileArchived(_) => "fileArchived",
			Self::FileArchiveRestored(_) => "fileArchiveRestored",
			Self::FileDeletedPermanent(_) => "fileDeletedPermanent",
			Self::FileMetadataChanged(_) => "fileMetadataChanged",
			Self::FileRename(_) => "fileRename",
			Self::FileMove(_) => "fileMove",
			Self::FileNew(_) => "fileNew",
			Self::FileRestore(_) => "fileRestore",
			Self::FileTrash(_) => "fileTrash",
			Self::FolderRename(_) => "folderRename",
			Self::FolderTrash(_) => "folderTrash",
			Self::FolderMove(_) => "folderMove",
			Self::FolderSubCreated(_) => "folderSubCreated",
			Self::FolderRestore(_) => "folderRestore",
			Self::FolderColorChanged(_) => "folderColorChanged",
			Self::FolderMetadataChanged(_) => "folderMetadataChanged",
			Self::FolderDeletedPermanent(_) => "folderDeletedPermanent",
			Self::ItemFavorite(_) => "itemFavorite",
			Self::TrashEmpty => "trashEmpty",
			Self::DeleteAll => "deleteAll",
			Self::DeleteVersioned => "deleteVersioned",
		}
	}
}

impl ChatEventType<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			Self::ChatMessageNew(_) => "chatMessageNew",
			Self::ChatTyping(_) => "chatTyping",
			Self::ChatConversationsNew(_) => "chatConversationsNew",
			Self::ChatMessageDelete(_) => "chatMessageDelete",
			Self::ChatMessageEmbedDisabled(_) => "chatMessageEmbedDisabled",
			Self::ChatConversationParticipantLeft(_) => "chatConversationParticipantLeft",
			Self::ChatConversationDeleted(_) => "chatConversationDeleted",
			Self::ChatMessageEdited(_) => "chatMessageEdited",
			Self::ChatConversationNameEdited(_) => "chatConversationNameEdited",
			Self::ChatConversationParticipantNew(_) => "chatConversationParticipantNew",
		}
	}
}

impl NoteEventType<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			Self::NoteArchived(_) => "noteArchived",
			Self::NoteContentEdited(_) => "noteContentEdited",
			Self::NoteDeleted(_) => "noteDeleted",
			Self::NoteTitleEdited(_) => "noteTitleEdited",
			Self::NoteParticipantPermissions(_) => "noteParticipantPermissions",
			Self::NoteRestored(_) => "noteRestored",
			Self::NoteParticipantRemoved(_) => "noteParticipantRemoved",
			Self::NoteParticipantNew(_) => "noteParticipantNew",
			Self::NoteNew(_) => "noteNew",
		}
	}
}

impl ContactEventType<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			Self::ContactRequestReceived(_) => "contactRequestReceived",
		}
	}
}

impl GeneralEventType<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			Self::PasswordChanged => "passwordChanged",
			Self::NewEvent(_) => "newEvent",
		}
	}
}

impl SocketEvent<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			Self::Drive { inner, .. } => inner.event_type(),
			Self::Chat { inner, .. } => inner.event_type(),
			Self::Note { inner, .. } => inner.event_type(),
			Self::Contact { inner, .. } => inner.event_type(),
			Self::General { inner, .. } => inner.event_type(),
		}
	}
}

#[cfg(all(
	target_family = "wasm",
	target_os = "unknown",
	not(feature = "service-worker")
))]
#[wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)]
const TS_SOCKET_EVENT_TYPE: &str =
	r#"export type SocketEventTypeNames = SocketEventCategory["type"]"#;

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct NewEvent<'a> {
	pub uuid: UuidStr,
	#[serde(rename = "type")]
	#[serde(borrow)]
	pub event_type: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(borrow)]
	pub info: Cow<'a, str>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FileRename<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
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
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	pub size: u64,
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
pub struct FileNew<'a> {
	pub parent: ParentUuid,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	// #[serde(borrow)]
	// rm: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	pub size: u64,
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
pub struct FileRestore<'a> {
	pub parent: ParentUuid,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	// #[serde(borrow)]
	// rm: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	pub size: u64,
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
pub struct FileMove<'a> {
	pub parent: ParentUuid,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	// #[serde(borrow)]
	// pub rm: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	pub size: u64,
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
pub struct FolderMove<'a> {
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
	pub uuid: UuidStr,
	pub parent: ParentUuid,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub favorited: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FolderSubCreated<'a> {
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
	pub uuid: UuidStr,
	pub parent: ParentUuid,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub favorited: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FolderRestore<'a> {
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
	pub uuid: UuidStr,
	pub parent: ParentUuid,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub favorited: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FolderColorChanged<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub color: DirColor<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
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
	#[serde(borrow)]
	pub metadata: RSAEncryptedString<'a>,
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
pub struct ChatConversationsNew<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub metadata: EncryptedString<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub added_timestamp: DateTime<Utc>,
	pub owner_id: u64,
	#[serde(borrow)]
	pub participants: Vec<ChatConversationParticipant<'a>>,
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
pub struct NoteContentEdited<'a> {
	pub note: UuidStr,
	#[serde(borrow)]
	pub content: EncryptedString<'a>,
	#[serde(rename = "type")]
	pub note_type: NoteType,
	pub editor_id: u64,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub edited_timestamp: DateTime<Utc>,
	#[serde(borrow)]
	pub metadata: RSAEncryptedString<'a>,
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
pub struct NoteTitleEdited<'a> {
	pub note: UuidStr,
	#[serde(borrow)]
	pub title: EncryptedString<'a>,
	#[serde(borrow)]
	pub metadata: RSAEncryptedString<'a>,
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
pub struct NoteParticipantNew<'a> {
	pub note: UuidStr,
	#[serde(flatten, borrow)]
	pub participant: NoteParticipant<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct NoteNew<'a> {
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
pub struct ChatMessageEdited<'a> {
	#[serde(rename = "conversation")]
	pub chat: UuidStr,
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub message: EncryptedString<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub edited_timestamp: DateTime<Utc>,
	pub metadata: RSAEncryptedString<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct ChatConversationNameEdited<'a> {
	pub uuid: UuidStr,
	#[serde(borrow)]
	pub name: EncryptedString<'a>,
	#[serde(borrow)]
	pub metadata: RSAEncryptedString<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct ContactRequestReceived<'a> {
	pub uuid: UuidStr,
	pub sender_id: u64,
	#[serde(borrow)]
	pub sender_email: Cow<'a, str>,
	#[serde(borrow)]
	pub sender_avatar: Option<Cow<'a, str>>,
	#[serde(borrow)]
	pub sender_nick_name: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub sent_timestamp: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct ItemFavorite<'a> {
	pub uuid: UuidStr,
	#[serde(rename = "type")]
	pub item_type: ObjectType,
	#[serde(deserialize_with = "crate::serde::boolean::number::deserialize")]
	pub value: bool,
	pub parent: ParentUuid,
	#[serde(borrow)]
	pub metadata: Option<EncryptedString<'a>>,
	#[serde(borrow)]
	pub name_encrypted: Option<EncryptedString<'a>>,
	#[serde(borrow)]
	pub region: Option<Cow<'a, str>>,
	#[serde(borrow)]
	pub bucket: Option<Cow<'a, str>>,
	pub size: Option<u64>,
	pub chunks: Option<u64>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(borrow)]
	pub color: DirColor<'a>,
	pub version: Option<FileEncryptionVersion>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct ChatConversationParticipantNew<'a> {
	#[serde(rename = "conversation")]
	pub chat: UuidStr,
	pub user_id: u64,
	#[serde(borrow)]
	pub email: Cow<'a, str>,
	#[serde(borrow)]
	pub avatar: Option<Cow<'a, str>>,
	#[serde(borrow)]
	pub nick_name: Cow<'a, str>,
	pub permissions_add: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub added_timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub last_active: DateTime<Utc>,
	pub appear_offline: bool,
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
pub struct FolderMetadataChanged<'a> {
	pub uuid: UuidStr,
	#[serde(borrow, rename = "name")]
	pub meta: EncryptedString<'a>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, CowHelpers)]
#[serde(rename_all = "camelCase")]
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
	fn classify_known_events() {
		assert_eq!(
			classify_event("file-new"),
			Some((EventCategory::Drive, "fileNew"))
		);
		assert_eq!(
			classify_event("chatMessageNew"),
			Some((EventCategory::Chat, "chatMessageNew"))
		);
		assert_eq!(
			classify_event("noteArchived"),
			Some((EventCategory::Note, "noteArchived"))
		);
		assert_eq!(
			classify_event("contactRequestReceived"),
			Some((EventCategory::Contact, "contactRequestReceived"))
		);
		assert_eq!(
			classify_event("new-event"),
			Some((EventCategory::General, "newEvent"))
		);
		assert_eq!(classify_event("unknown-event"), None);
	}

	#[test]
	fn deserialize_drive_event() {
		let json = r#"["file-new",{"driveMessageId":42,"parent":"00000000-0000-0000-0000-000000000000","uuid":"11111111-1111-1111-1111-111111111111","metadata":"encrypted","timestamp":1700000,"chunks":1,"size":100,"bucket":"b","region":"r","version":2,"favorited":0}]"#;
		let event: SocketEvent = serde_json::from_str(json).unwrap();
		assert!(matches!(
			event,
			SocketEvent::Drive {
				drive_message_id: 42,
				..
			}
		));
	}

	#[test]
	fn deserialize_chat_event() {
		let json = r#"["chatMessageDelete",{"chatMessageId":7,"uuid":"11111111-1111-1111-1111-111111111111"}]"#;
		let event: SocketEvent = serde_json::from_str(json).unwrap();
		assert!(matches!(
			event,
			SocketEvent::Chat {
				chat_message_id: 7,
				..
			}
		));
	}

	#[test]
	fn deserialize_general_event() {
		let json = r#"["passwordChanged",{"generalMessageId":1}]"#;
		let event: SocketEvent = serde_json::from_str(json).unwrap();
		assert!(matches!(
			event,
			SocketEvent::General {
				general_message_id: 1,
				..
			}
		));
	}

	#[test]
	fn deserialize_unknown_event_fails() {
		let json = r#"["unknown-event",{"messageId":1}]"#;
		assert!(serde_json::from_str::<SocketEvent>(json).is_err());
	}
}
