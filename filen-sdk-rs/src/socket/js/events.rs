use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::{
		chat::typing::ChatTypingType,
		notes::NoteType,
		socket::{
			ChatConversationDeleted, ChatConversationParticipantLeft, ChatMessageDelete,
			ChatMessageEmbedDisabled, FileArchived, FileDeletedPermanent, FileTrash,
			FolderDeletedPermanent, FolderTrash, NoteArchived, NoteDeleted, NoteNew,
			NoteParticipantPermissions, NoteParticipantRemoved, NoteRestored,
		},
	},
	crypto::MaybeEncrypted,
	fs::UuidStr,
	traits::{CowHelpers, CowHelpersExt},
};

use crate::{
	js::{Dir, DirColor, DirMeta, File, FileMeta, NonRootItemTagged},
	notes::NoteParticipant,
	socket::events::ChatConversationParticipantNew,
};

use crate::socket::events::DecryptedSocketEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(tag = "type", rename_all = "camelCase"),
	tsify(into_wasm_abi, large_number_types_as_bigints, hashmap_as_object)
)]
pub enum SocketEvent {
	/// Sent after successful authentication, including on reconnect
	AuthSuccess,
	/// Sent after failed authentication, including on reconnect, after which the socket is closed and all listeners removed
	AuthFailed,
	/// Sent when the socket has unexpectedly closed and begins attempting to reconnect
	Reconnecting,
	/// Sent when the handle to the event listener has been dropped and the listener is removed
	Unsubscribed,
	NewEvent(NewEvent),
	FileRename(FileRename),
	FileArchiveRestored(FileArchiveRestored),
	FileNew(FileNew),
	FileRestore(FileRestore),
	FileMove(FileMove),
	FileTrash(FileTrash),
	FileArchived(FileArchived),
	FolderRename(FolderRename),
	FolderTrash(FolderTrash),
	FolderMove(FolderMove),
	FolderSubCreated(FolderSubCreated),
	FolderRestore(FolderRestore),
	FolderColorChanged(FolderColorChanged),
	TrashEmpty,
	PasswordChanged,
	ChatMessageNew(ChatMessageNew),
	ChatTyping(ChatTyping),
	ChatConversationsNew(ChatConversationsNew),
	ChatMessageDelete(ChatMessageDelete),
	NoteContentEdited(NoteContentEdited),
	NoteArchived(NoteArchived),
	NoteDeleted(NoteDeleted),
	NoteTitleEdited(NoteTitleEdited),
	NoteParticipantPermissions(NoteParticipantPermissions),
	NoteRestored(NoteRestored),
	NoteParticipantRemoved(NoteParticipantRemoved),
	NoteParticipantNew(NoteParticipantNew),
	NoteNew(NoteNew),
	ChatMessageEmbedDisabled(ChatMessageEmbedDisabled),
	ChatConversationParticipantLeft(ChatConversationParticipantLeft),
	ChatConversationDeleted(ChatConversationDeleted),
	ChatMessageEdited(ChatMessageEdited),
	ChatConversationNameEdited(ChatConversationNameEdited),
	ContactRequestReceived(ContactRequestReceived),
	ItemFavorite(ItemFavorite),
	ChatConversationParticipantNew(ChatConversationParticipantNew),
	FileDeletedPermanent(FileDeletedPermanent),
	FolderMetadataChanged(FolderMetadataChanged),
	FolderDeletedPermanent(FolderDeletedPermanent),
	FileMetadataChanged(FileMetadataChanged),
}

impl From<&DecryptedSocketEvent<'_>> for SocketEvent {
	fn from(event: &DecryptedSocketEvent<'_>) -> Self {
		match event {
			DecryptedSocketEvent::AuthSuccess => Self::AuthSuccess,
			DecryptedSocketEvent::AuthFailed => Self::AuthFailed,
			DecryptedSocketEvent::Reconnecting => Self::Reconnecting,
			DecryptedSocketEvent::Unsubscribed => Self::Unsubscribed,
			DecryptedSocketEvent::NewEvent(e) => Self::NewEvent(e.into()),
			DecryptedSocketEvent::FileRename(e) => Self::FileRename(e.into()),
			DecryptedSocketEvent::FileArchiveRestored(e) => Self::FileArchiveRestored(e.into()),
			DecryptedSocketEvent::FileNew(e) => Self::FileNew(e.into()),
			DecryptedSocketEvent::FileRestore(e) => Self::FileRestore(e.into()),
			DecryptedSocketEvent::FileMove(e) => Self::FileMove(e.into()),
			DecryptedSocketEvent::FileTrash(e) => Self::FileTrash(e.clone()),
			DecryptedSocketEvent::FileArchived(e) => Self::FileArchived(e.clone()),
			DecryptedSocketEvent::FolderRename(e) => Self::FolderRename(e.into()),
			DecryptedSocketEvent::FolderTrash(e) => Self::FolderTrash(e.clone()),
			DecryptedSocketEvent::FolderMove(e) => Self::FolderMove(e.into()),
			DecryptedSocketEvent::FolderSubCreated(e) => Self::FolderSubCreated(e.into()),
			DecryptedSocketEvent::FolderRestore(e) => Self::FolderRestore(e.into()),
			DecryptedSocketEvent::FolderColorChanged(e) => Self::FolderColorChanged(e.into()),
			DecryptedSocketEvent::TrashEmpty => Self::TrashEmpty,
			DecryptedSocketEvent::PasswordChanged => Self::PasswordChanged,
			DecryptedSocketEvent::ChatMessageNew(e) => Self::ChatMessageNew(e.into()),
			DecryptedSocketEvent::ChatTyping(e) => Self::ChatTyping(e.into()),
			DecryptedSocketEvent::ChatConversationsNew(e) => Self::ChatConversationsNew(e.into()),
			DecryptedSocketEvent::ChatMessageDelete(e) => Self::ChatMessageDelete(e.clone()),
			DecryptedSocketEvent::NoteContentEdited(e) => Self::NoteContentEdited(e.into()),
			DecryptedSocketEvent::NoteArchived(e) => Self::NoteArchived(e.clone()),
			DecryptedSocketEvent::NoteDeleted(e) => Self::NoteDeleted(e.clone()),
			DecryptedSocketEvent::NoteTitleEdited(e) => Self::NoteTitleEdited(e.into()),
			DecryptedSocketEvent::NoteParticipantPermissions(e) => {
				Self::NoteParticipantPermissions(e.clone())
			}
			DecryptedSocketEvent::NoteRestored(e) => Self::NoteRestored(e.clone()),
			DecryptedSocketEvent::NoteParticipantRemoved(e) => {
				Self::NoteParticipantRemoved(e.clone())
			}
			DecryptedSocketEvent::NoteParticipantNew(e) => Self::NoteParticipantNew(e.into()),
			DecryptedSocketEvent::NoteNew(e) => Self::NoteNew(e.clone()),
			DecryptedSocketEvent::ChatMessageEmbedDisabled(e) => {
				Self::ChatMessageEmbedDisabled(e.clone())
			}
			DecryptedSocketEvent::ChatConversationParticipantLeft(e) => {
				Self::ChatConversationParticipantLeft(e.clone())
			}
			DecryptedSocketEvent::ChatConversationDeleted(e) => {
				Self::ChatConversationDeleted(e.clone())
			}
			DecryptedSocketEvent::ChatMessageEdited(e) => Self::ChatMessageEdited(e.into()),
			DecryptedSocketEvent::ChatConversationNameEdited(e) => {
				Self::ChatConversationNameEdited(e.into())
			}
			DecryptedSocketEvent::ContactRequestReceived(e) => {
				Self::ContactRequestReceived(e.into())
			}
			DecryptedSocketEvent::ItemFavorite(e) => Self::ItemFavorite(e.into()),
			DecryptedSocketEvent::ChatConversationParticipantNew(e) => {
				Self::ChatConversationParticipantNew(e.clone())
			}
			DecryptedSocketEvent::FileDeletedPermanent(e) => Self::FileDeletedPermanent(e.clone()),
			DecryptedSocketEvent::FolderMetadataChanged(e) => Self::FolderMetadataChanged(e.into()),
			DecryptedSocketEvent::FolderDeletedPermanent(e) => {
				Self::FolderDeletedPermanent(e.clone())
			}
			DecryptedSocketEvent::FileMetadataChanged(e) => Self::FileMetadataChanged(e.into()),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct NewEvent {
	pub uuid: UuidStr,
	pub event_type: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	pub info: String,
}

impl From<&crate::socket::events::NewEvent<'_>> for NewEvent {
	fn from(event: &crate::socket::events::NewEvent<'_>) -> Self {
		Self {
			uuid: event.uuid,
			event_type: event.event_type.to_string(),
			timestamp: event.timestamp,
			info: event.info.to_string(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct FileRename {
	pub uuid: UuidStr,
	pub metadata: FileMeta,
}

impl From<&crate::socket::events::FileRename<'_>> for FileRename {
	fn from(event: &crate::socket::events::FileRename<'_>) -> Self {
		Self {
			uuid: event.uuid,
			metadata: event.metadata.as_borrowed_cow().into(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct FileArchiveRestored {
	pub current_uuid: UuidStr,
	pub file: File,
}

impl From<&crate::socket::events::FileArchiveRestored> for FileArchiveRestored {
	fn from(event: &crate::socket::events::FileArchiveRestored) -> Self {
		Self {
			current_uuid: event.current_uuid,
			file: event.file.clone().into(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct FileNew {
	pub file: File,
}

impl From<&crate::socket::events::FileNew> for FileNew {
	fn from(event: &crate::socket::events::FileNew) -> Self {
		Self {
			file: event.0.clone().into(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct FileRestore {
	pub file: File,
}

impl From<&crate::socket::events::FileRestore> for FileRestore {
	fn from(event: &crate::socket::events::FileRestore) -> Self {
		Self {
			file: event.0.clone().into(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct FileMove {
	pub file: File,
}

impl From<&crate::socket::events::FileMove> for FileMove {
	fn from(event: &crate::socket::events::FileMove) -> Self {
		Self {
			file: event.0.clone().into(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderRename {
	pub name: MaybeEncrypted<'static>,
	pub uuid: UuidStr,
}

impl From<&crate::socket::events::FolderRename<'_>> for FolderRename {
	fn from(event: &crate::socket::events::FolderRename<'_>) -> Self {
		Self {
			name: event.name.to_owned_cow(),
			uuid: event.uuid,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderMove {
	pub dir: Dir,
}

impl From<&crate::socket::events::FolderMove> for FolderMove {
	fn from(event: &crate::socket::events::FolderMove) -> Self {
		Self {
			dir: event.0.clone().into(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderSubCreated {
	pub dir: Dir,
}

impl From<&crate::socket::events::FolderSubCreated> for FolderSubCreated {
	fn from(event: &crate::socket::events::FolderSubCreated) -> Self {
		Self {
			dir: event.0.clone().into(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderRestore {
	pub dir: Dir,
}

impl From<&crate::socket::events::FolderRestore> for FolderRestore {
	fn from(event: &crate::socket::events::FolderRestore) -> Self {
		Self {
			dir: event.0.clone().into(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderColorChanged {
	pub uuid: UuidStr,
	pub color: DirColor,
}

impl From<&crate::socket::events::FolderColorChanged<'_>> for FolderColorChanged {
	fn from(event: &crate::socket::events::FolderColorChanged<'_>) -> Self {
		Self {
			uuid: event.uuid,
			color: DirColor::from(event.color.as_borrowed_cow()),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatMessageNew {}

impl From<&crate::socket::events::ChatMessageNew> for ChatMessageNew {
	fn from(_event: &crate::socket::events::ChatMessageNew) -> Self {
		Self {}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatTyping {
	pub chat: UuidStr,
	pub sender_avatar: Option<String>,
	pub sender_email: String,
	pub sender_nick_name: String,
	pub sender_id: u64,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	pub typing_type: ChatTypingType,
}

impl From<&crate::socket::events::ChatTyping<'_>> for ChatTyping {
	fn from(event: &crate::socket::events::ChatTyping<'_>) -> Self {
		Self {
			chat: event.chat,
			sender_avatar: event.sender_avatar.as_ref().map(|s| s.to_string()),
			sender_email: event.sender_email.to_string(),
			sender_nick_name: event.sender_nick_name.to_string(),
			sender_id: event.sender_id,
			timestamp: event.timestamp,
			typing_type: event.typing_type,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatConversationsNew {}

impl From<&crate::socket::events::ChatConversationsNew> for ChatConversationsNew {
	fn from(_event: &crate::socket::events::ChatConversationsNew) -> Self {
		Self {}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct NoteContentEdited {
	pub note: UuidStr,
	pub content: MaybeEncrypted<'static>,
	pub note_type: NoteType,
	pub editor_id: u64,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	pub edited_timestamp: DateTime<Utc>,
}

impl From<&crate::socket::events::NoteContentEdited<'_>> for NoteContentEdited {
	fn from(event: &crate::socket::events::NoteContentEdited<'_>) -> Self {
		Self {
			note: event.note,
			content: event.content.to_owned_cow(),
			note_type: event.note_type,
			editor_id: event.editor_id,
			edited_timestamp: event.edited_timestamp,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct NoteTitleEdited {
	pub note: UuidStr,
	pub new_title: MaybeEncrypted<'static>,
}

impl From<&crate::socket::events::NoteTitleEdited<'_>> for NoteTitleEdited {
	fn from(event: &crate::socket::events::NoteTitleEdited<'_>) -> Self {
		Self {
			note: event.note,
			new_title: event.new_title.to_owned_cow(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct NoteParticipantNew {
	pub note: UuidStr,
	pub participant: NoteParticipant,
}

impl From<&crate::socket::events::NoteParticipantNew> for NoteParticipantNew {
	fn from(event: &crate::socket::events::NoteParticipantNew) -> Self {
		Self {
			note: event.note,
			participant: event.participant.clone(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatMessageEdited {
	pub chat: UuidStr,
	pub uuid: UuidStr,
	pub new_content: MaybeEncrypted<'static>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		serde(with = "chrono::serde::ts_milliseconds")
	)]
	pub edited_timestamp: DateTime<Utc>,
}

impl From<&crate::socket::events::ChatMessageEdited<'_>> for ChatMessageEdited {
	fn from(event: &crate::socket::events::ChatMessageEdited<'_>) -> Self {
		Self {
			chat: event.chat,
			uuid: event.uuid,
			new_content: event.new_content.to_owned_cow(),
			edited_timestamp: event.edited_timestamp,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct ChatConversationNameEdited {
	pub chat: UuidStr,
	pub new_name: MaybeEncrypted<'static>,
}

impl From<&crate::socket::events::ChatConversationNameEdited<'_>> for ChatConversationNameEdited {
	fn from(event: &crate::socket::events::ChatConversationNameEdited<'_>) -> Self {
		Self {
			chat: event.chat,
			new_name: event.new_name.to_owned_cow(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct ContactRequestReceived {
	pub uuid: UuidStr,
	pub sender_id: u64,
	pub sender_email: String,
	pub sender_avatar: Option<String>,
	pub sender_nick_name: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub sent_timestamp: DateTime<Utc>,
}

impl From<&crate::socket::events::ContactRequestReceived<'_>> for ContactRequestReceived {
	fn from(event: &crate::socket::events::ContactRequestReceived<'_>) -> Self {
		Self {
			uuid: event.uuid,
			sender_id: event.sender_id,
			sender_email: event.sender_email.to_string(),
			sender_avatar: event.sender_avatar.as_ref().map(|s| s.to_string()),
			sender_nick_name: event.sender_nick_name.to_string(),
			sent_timestamp: event.sent_timestamp,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct ItemFavorite {
	pub item: NonRootItemTagged,
}

impl From<&crate::socket::events::ItemFavorite> for ItemFavorite {
	fn from(event: &crate::socket::events::ItemFavorite) -> Self {
		Self {
			item: event.0.clone().into(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct FolderMetadataChanged {
	pub uuid: UuidStr,
	pub meta: DirMeta,
}

impl From<&crate::socket::events::FolderMetadataChanged<'_>> for FolderMetadataChanged {
	fn from(event: &crate::socket::events::FolderMetadataChanged<'_>) -> Self {
		Self {
			uuid: event.uuid,
			meta: event.meta.as_borrowed_cow().into(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, tsify::Tsify),
	serde(rename_all = "camelCase"),
	tsify(large_number_types_as_bigints)
)]
pub struct FileMetadataChanged {
	pub uuid: UuidStr,
	pub name: MaybeEncrypted<'static>,
	pub metadata: FileMeta,
	pub old_metadata: FileMeta,
}

impl From<&crate::socket::events::FileMetadataChanged<'_>> for FileMetadataChanged {
	fn from(event: &crate::socket::events::FileMetadataChanged<'_>) -> Self {
		Self {
			uuid: event.uuid,
			name: event.name.to_owned_cow(),
			metadata: event.metadata.as_borrowed_cow().into(),
			old_metadata: event.old_metadata.as_borrowed_cow().into(),
		}
	}
}
