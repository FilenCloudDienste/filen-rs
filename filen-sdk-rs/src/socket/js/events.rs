use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::{
	api::v3::{
		chat::typing::ChatTypingType,
		notes::NoteType,
		socket::{
			ChatConversationDeleted, ChatConversationParticipantLeft, ChatMessageDelete,
			ChatMessageEmbedDisabled, FileArchived, FileDeletedPermanent, FileTrash,
			FolderDeletedPermanent, FolderTrash, NoteArchived, NoteDeleted,
			NoteParticipantPermissions, NoteParticipantRemoved, NoteRestored,
		},
	},
	crypto::MaybeEncrypted,
	fs::UuidStr,
	traits::{CowHelpers, CowHelpersExt},
};

use crate::{
	chats::{Chat, ChatMessage},
	js::{Dir, DirColor, DirMeta, File, FileMeta, NonRootItemTagged},
	notes::NoteParticipant,
	socket::events::{ChatConversationParticipantNew, DecryptedSocketEvent},
};

use crate::socket::events::DecryptedSocketEventType;

#[js_type(export, no_deser)]
pub struct SocketEvent {
	pub inner: SocketEventType,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint | null"))]
	pub global_message_id: Option<u64>,
}

#[js_type(export, no_deser, tagged)]
pub enum SocketEventType {
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
		let event_type = match event.inner() {
			DecryptedSocketEventType::AuthSuccess => SocketEventType::AuthSuccess,
			DecryptedSocketEventType::AuthFailed => SocketEventType::AuthFailed,
			DecryptedSocketEventType::Reconnecting => SocketEventType::Reconnecting,
			DecryptedSocketEventType::Unsubscribed => SocketEventType::Unsubscribed,
			DecryptedSocketEventType::NewEvent(e) => SocketEventType::NewEvent(e.into()),
			DecryptedSocketEventType::FileRename(e) => SocketEventType::FileRename(e.into()),
			DecryptedSocketEventType::FileArchiveRestored(e) => {
				SocketEventType::FileArchiveRestored(e.into())
			}
			DecryptedSocketEventType::FileNew(e) => SocketEventType::FileNew(e.into()),
			DecryptedSocketEventType::FileRestore(e) => SocketEventType::FileRestore(e.into()),
			DecryptedSocketEventType::FileMove(e) => SocketEventType::FileMove(e.into()),
			DecryptedSocketEventType::FileTrash(e) => SocketEventType::FileTrash(e.clone()),
			DecryptedSocketEventType::FileArchived(e) => SocketEventType::FileArchived(e.clone()),
			DecryptedSocketEventType::FolderRename(e) => SocketEventType::FolderRename(e.into()),
			DecryptedSocketEventType::FolderTrash(e) => SocketEventType::FolderTrash(e.clone()),
			DecryptedSocketEventType::FolderMove(e) => SocketEventType::FolderMove(e.into()),
			DecryptedSocketEventType::FolderSubCreated(e) => {
				SocketEventType::FolderSubCreated(e.into())
			}
			DecryptedSocketEventType::FolderRestore(e) => SocketEventType::FolderRestore(e.into()),
			DecryptedSocketEventType::FolderColorChanged(e) => {
				SocketEventType::FolderColorChanged(e.into())
			}
			DecryptedSocketEventType::TrashEmpty => SocketEventType::TrashEmpty,
			DecryptedSocketEventType::PasswordChanged => SocketEventType::PasswordChanged,
			DecryptedSocketEventType::ChatMessageNew(e) => {
				SocketEventType::ChatMessageNew(e.into())
			}
			DecryptedSocketEventType::ChatTyping(e) => SocketEventType::ChatTyping(e.into()),
			DecryptedSocketEventType::ChatConversationsNew(e) => {
				SocketEventType::ChatConversationsNew(e.into())
			}
			DecryptedSocketEventType::ChatMessageDelete(e) => {
				SocketEventType::ChatMessageDelete(e.clone())
			}
			DecryptedSocketEventType::NoteContentEdited(e) => {
				SocketEventType::NoteContentEdited(e.into())
			}
			DecryptedSocketEventType::NoteArchived(e) => SocketEventType::NoteArchived(e.clone()),
			DecryptedSocketEventType::NoteDeleted(e) => SocketEventType::NoteDeleted(e.clone()),
			DecryptedSocketEventType::NoteTitleEdited(e) => {
				SocketEventType::NoteTitleEdited(e.into())
			}
			DecryptedSocketEventType::NoteParticipantPermissions(e) => {
				SocketEventType::NoteParticipantPermissions(e.clone())
			}
			DecryptedSocketEventType::NoteRestored(e) => SocketEventType::NoteRestored(e.clone()),
			DecryptedSocketEventType::NoteParticipantRemoved(e) => {
				SocketEventType::NoteParticipantRemoved(e.clone())
			}
			DecryptedSocketEventType::NoteParticipantNew(e) => {
				SocketEventType::NoteParticipantNew(e.into())
			}
			DecryptedSocketEventType::NoteNew(e) => SocketEventType::NoteNew(e.into()),
			DecryptedSocketEventType::ChatMessageEmbedDisabled(e) => {
				SocketEventType::ChatMessageEmbedDisabled(e.clone())
			}
			DecryptedSocketEventType::ChatConversationParticipantLeft(e) => {
				SocketEventType::ChatConversationParticipantLeft(e.clone())
			}
			DecryptedSocketEventType::ChatConversationDeleted(e) => {
				SocketEventType::ChatConversationDeleted(e.clone())
			}
			DecryptedSocketEventType::ChatMessageEdited(e) => {
				SocketEventType::ChatMessageEdited(e.into())
			}
			DecryptedSocketEventType::ChatConversationNameEdited(e) => {
				SocketEventType::ChatConversationNameEdited(e.into())
			}
			DecryptedSocketEventType::ContactRequestReceived(e) => {
				SocketEventType::ContactRequestReceived(e.into())
			}
			DecryptedSocketEventType::ItemFavorite(e) => SocketEventType::ItemFavorite(e.into()),
			DecryptedSocketEventType::ChatConversationParticipantNew(e) => {
				SocketEventType::ChatConversationParticipantNew(e.clone())
			}
			DecryptedSocketEventType::FileDeletedPermanent(e) => {
				SocketEventType::FileDeletedPermanent(e.clone())
			}
			DecryptedSocketEventType::FolderMetadataChanged(e) => {
				SocketEventType::FolderMetadataChanged(e.into())
			}
			DecryptedSocketEventType::FolderDeletedPermanent(e) => {
				SocketEventType::FolderDeletedPermanent(e.clone())
			}
			DecryptedSocketEventType::FileMetadataChanged(e) => {
				SocketEventType::FileMetadataChanged(e.into())
			}
		};
		SocketEvent {
			inner: event_type,
			global_message_id: event.global_message_id(),
		}
	}
}

#[js_type]
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

#[js_type]
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

#[js_type]
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

#[js_type]
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

#[js_type]
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

#[js_type]
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

#[js_type]
pub struct FolderRename {
	pub name: MaybeEncrypted<'static, str>,
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

#[js_type]
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

#[js_type]
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

#[js_type]
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

#[js_type]
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

#[js_type]
pub struct ChatMessageNew {
	pub msg: ChatMessage,
}

impl From<&crate::socket::events::ChatMessageNew> for ChatMessageNew {
	fn from(event: &crate::socket::events::ChatMessageNew) -> Self {
		Self {
			msg: event.0.clone(),
		}
	}
}

#[js_type]
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

#[js_type]
pub struct ChatConversationsNew {
	pub chat: Chat,
}

impl From<&crate::socket::events::ChatConversationsNew> for ChatConversationsNew {
	fn from(event: &crate::socket::events::ChatConversationsNew) -> Self {
		Self {
			chat: event.0.clone(),
		}
	}
}

#[js_type]
pub struct NoteContentEdited {
	pub note: UuidStr,
	pub content: MaybeEncrypted<'static, str>,
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

#[js_type]
pub struct NoteTitleEdited {
	pub note: UuidStr,
	pub new_title: MaybeEncrypted<'static, str>,
}

impl From<&crate::socket::events::NoteTitleEdited<'_>> for NoteTitleEdited {
	fn from(event: &crate::socket::events::NoteTitleEdited<'_>) -> Self {
		Self {
			note: event.note,
			new_title: event.new_title.to_owned_cow(),
		}
	}
}

#[js_type]
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

#[js_type]
pub struct NoteNew {
	pub note: UuidStr,
}
impl From<&crate::socket::events::NoteNew> for NoteNew {
	fn from(value: &crate::socket::events::NoteNew) -> Self {
		Self { note: value.note }
	}
}

#[js_type]
pub struct ChatMessageEdited {
	pub chat: UuidStr,
	pub uuid: UuidStr,
	pub new_content: MaybeEncrypted<'static, str>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
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

#[js_type]
pub struct ChatConversationNameEdited {
	pub chat: UuidStr,
	pub new_name: MaybeEncrypted<'static, str>,
}

impl From<&crate::socket::events::ChatConversationNameEdited<'_>> for ChatConversationNameEdited {
	fn from(event: &crate::socket::events::ChatConversationNameEdited<'_>) -> Self {
		Self {
			chat: event.chat,
			new_name: event.new_name.to_owned_cow(),
		}
	}
}

#[js_type]
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

#[js_type(no_deser)]
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

#[js_type]
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

#[js_type]
pub struct FileMetadataChanged {
	pub uuid: UuidStr,
	pub name: MaybeEncrypted<'static, str>,
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
