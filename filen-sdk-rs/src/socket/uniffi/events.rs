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
			SocketEvent as BorrowedSocketEvent,
		},
	},
	auth::FileEncryptionVersion,
	crypto::EncryptedString,
	fs::{ObjectType, UuidStr},
	traits::CowHelpers,
};

use crate::{js::DirColor, notes::NoteParticipant};

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum SocketEvent {
	/// Sent after successful authentication, including on reconnect
	AuthSuccess,
	/// Sent after failed authentication, including on reconnect, after which the socket is closed and all listeners removed
	AuthFailed,
	/// Sent when the socket has unexpectedly closed and begins attempting to reconnect
	Reconnecting,
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

impl From<&BorrowedSocketEvent<'_>> for SocketEvent {
	fn from(event: &BorrowedSocketEvent<'_>) -> Self {
		match event {
			BorrowedSocketEvent::AuthSuccess => Self::AuthSuccess,
			BorrowedSocketEvent::AuthFailed => Self::AuthFailed,
			BorrowedSocketEvent::Reconnecting => Self::Reconnecting,
			BorrowedSocketEvent::NewEvent(e) => Self::NewEvent(e.into()),
			BorrowedSocketEvent::FileRename(e) => Self::FileRename(e.into()),
			BorrowedSocketEvent::FileArchiveRestored(e) => Self::FileArchiveRestored(e.into()),
			BorrowedSocketEvent::FileNew(e) => Self::FileNew(e.into()),
			BorrowedSocketEvent::FileRestore(e) => Self::FileRestore(e.into()),
			BorrowedSocketEvent::FileMove(e) => Self::FileMove(e.into()),
			BorrowedSocketEvent::FileTrash(e) => Self::FileTrash(e.clone()),
			BorrowedSocketEvent::FileArchived(e) => Self::FileArchived(e.clone()),
			BorrowedSocketEvent::FolderRename(e) => Self::FolderRename(e.into()),
			BorrowedSocketEvent::FolderTrash(e) => Self::FolderTrash(e.clone()),
			BorrowedSocketEvent::FolderMove(e) => Self::FolderMove(e.into()),
			BorrowedSocketEvent::FolderSubCreated(e) => Self::FolderSubCreated(e.into()),
			BorrowedSocketEvent::FolderRestore(e) => Self::FolderRestore(e.into()),
			BorrowedSocketEvent::FolderColorChanged(e) => Self::FolderColorChanged(e.into()),
			BorrowedSocketEvent::TrashEmpty => Self::TrashEmpty,
			BorrowedSocketEvent::PasswordChanged => Self::PasswordChanged,
			BorrowedSocketEvent::ChatMessageNew(e) => Self::ChatMessageNew(e.into()),
			BorrowedSocketEvent::ChatTyping(e) => Self::ChatTyping(e.into()),
			BorrowedSocketEvent::ChatConversationsNew(e) => Self::ChatConversationsNew(e.into()),
			BorrowedSocketEvent::ChatMessageDelete(e) => Self::ChatMessageDelete(e.clone()),
			BorrowedSocketEvent::NoteContentEdited(e) => Self::NoteContentEdited(e.into()),
			BorrowedSocketEvent::NoteArchived(e) => Self::NoteArchived(e.clone()),
			BorrowedSocketEvent::NoteDeleted(e) => Self::NoteDeleted(e.clone()),
			BorrowedSocketEvent::NoteTitleEdited(e) => Self::NoteTitleEdited(e.into()),
			BorrowedSocketEvent::NoteParticipantPermissions(e) => {
				Self::NoteParticipantPermissions(e.clone())
			}
			BorrowedSocketEvent::NoteRestored(e) => Self::NoteRestored(e.clone()),
			BorrowedSocketEvent::NoteParticipantRemoved(e) => {
				Self::NoteParticipantRemoved(e.clone())
			}
			BorrowedSocketEvent::NoteParticipantNew(e) => Self::NoteParticipantNew(e.into()),
			BorrowedSocketEvent::NoteNew(e) => Self::NoteNew(e.clone()),
			BorrowedSocketEvent::ChatMessageEmbedDisabled(e) => {
				Self::ChatMessageEmbedDisabled(e.clone())
			}
			BorrowedSocketEvent::ChatConversationParticipantLeft(e) => {
				Self::ChatConversationParticipantLeft(e.clone())
			}
			BorrowedSocketEvent::ChatConversationDeleted(e) => {
				Self::ChatConversationDeleted(e.clone())
			}
			BorrowedSocketEvent::ChatMessageEdited(e) => Self::ChatMessageEdited(e.into()),
			BorrowedSocketEvent::ChatConversationNameEdited(e) => {
				Self::ChatConversationNameEdited(e.into())
			}
			BorrowedSocketEvent::ContactRequestReceived(e) => {
				Self::ContactRequestReceived(e.into())
			}
			BorrowedSocketEvent::ItemFavorite(e) => Self::ItemFavorite(e.into()),
			BorrowedSocketEvent::ChatConversationParticipantNew(e) => {
				Self::ChatConversationParticipantNew(e.into())
			}
			BorrowedSocketEvent::FileDeletedPermanent(e) => Self::FileDeletedPermanent(e.clone()),
			BorrowedSocketEvent::FolderMetadataChanged(e) => Self::FolderMetadataChanged(e.into()),
			BorrowedSocketEvent::FolderDeletedPermanent(e) => {
				Self::FolderDeletedPermanent(e.clone())
			}
			BorrowedSocketEvent::FileMetadataChanged(e) => Self::FileMetadataChanged(e.into()),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NewEvent {
	pub uuid: UuidStr,
	pub event_type: String,
	pub timestamp: DateTime<Utc>,
	pub info: String,
}

impl From<&filen_types::api::v3::socket::NewEvent<'_>> for NewEvent {
	fn from(event: &filen_types::api::v3::socket::NewEvent<'_>) -> Self {
		Self {
			uuid: event.uuid,
			event_type: event.event_type.to_string(),
			timestamp: event.timestamp,
			info: event.info.to_string(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FileRename {
	pub uuid: UuidStr,
	pub metadata: EncryptedString<'static>,
}

impl From<&filen_types::api::v3::socket::FileRename<'_>> for FileRename {
	fn from(event: &filen_types::api::v3::socket::FileRename<'_>) -> Self {
		Self {
			uuid: event.uuid,
			metadata: event.metadata.as_borrowed_cow().into_owned_cow(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FileArchiveRestored {
	pub current_uuid: UuidStr,
	pub parent: UuidStr,
	pub uuid: UuidStr,
	pub metadata: EncryptedString<'static>,
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	pub bucket: String,
	pub region: String,
	pub version: FileEncryptionVersion,
	pub favorited: bool,
}

impl From<&filen_types::api::v3::socket::FileArchiveRestored<'_>> for FileArchiveRestored {
	fn from(event: &filen_types::api::v3::socket::FileArchiveRestored<'_>) -> Self {
		Self {
			current_uuid: event.current_uuid,
			parent: event.parent,
			uuid: event.uuid,
			metadata: event.metadata.as_borrowed_cow().into_owned_cow(),
			timestamp: event.timestamp,
			chunks: event.chunks,
			bucket: event.bucket.to_string(),
			region: event.region.to_string(),
			version: event.version,
			favorited: event.favorited,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FileNew {
	pub parent: UuidStr,
	pub uuid: UuidStr,
	pub metadata: EncryptedString<'static>,
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	pub bucket: String,
	pub region: String,
	pub version: FileEncryptionVersion,
	pub favorited: bool,
}

impl From<&filen_types::api::v3::socket::FileNew<'_>> for FileNew {
	fn from(event: &filen_types::api::v3::socket::FileNew<'_>) -> Self {
		Self {
			parent: event.parent,
			uuid: event.uuid,
			metadata: event.metadata.as_borrowed_cow().into_owned_cow(),
			timestamp: event.timestamp,
			chunks: event.chunks,
			bucket: event.bucket.to_string(),
			region: event.region.to_string(),
			version: event.version,
			favorited: event.favorited,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FileRestore {
	pub parent: UuidStr,
	pub uuid: UuidStr,
	pub metadata: EncryptedString<'static>,
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	pub bucket: String,
	pub region: String,
	pub version: FileEncryptionVersion,
	pub favorited: bool,
}

impl From<&filen_types::api::v3::socket::FileRestore<'_>> for FileRestore {
	fn from(event: &filen_types::api::v3::socket::FileRestore<'_>) -> Self {
		Self {
			parent: event.parent,
			uuid: event.uuid,
			metadata: event.metadata.as_borrowed_cow().into_owned_cow(),
			timestamp: event.timestamp,
			chunks: event.chunks,
			bucket: event.bucket.to_string(),
			region: event.region.to_string(),
			version: event.version,
			favorited: event.favorited,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FileMove {
	pub parent: UuidStr,
	pub uuid: UuidStr,
	pub metadata: EncryptedString<'static>,
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	pub bucket: String,
	pub region: String,
	pub version: FileEncryptionVersion,
	pub favorited: bool,
}

impl From<&filen_types::api::v3::socket::FileMove<'_>> for FileMove {
	fn from(event: &filen_types::api::v3::socket::FileMove<'_>) -> Self {
		Self {
			parent: event.parent,
			uuid: event.uuid,
			metadata: event.metadata.as_borrowed_cow().into_owned_cow(),
			timestamp: event.timestamp,
			chunks: event.chunks,
			bucket: event.bucket.to_string(),
			region: event.region.to_string(),
			version: event.version,
			favorited: event.favorited,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]

pub struct FolderRename {
	pub name: EncryptedString<'static>,
	pub uuid: UuidStr,
}

impl From<&filen_types::api::v3::socket::FolderRename<'_>> for FolderRename {
	fn from(event: &filen_types::api::v3::socket::FolderRename<'_>) -> Self {
		Self {
			name: event.name.as_borrowed_cow().into_owned_cow(),
			uuid: event.uuid,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]

pub struct FolderMove {
	pub name: EncryptedString<'static>,
	pub uuid: UuidStr,
	pub parent: UuidStr,
	pub timestamp: DateTime<Utc>,
	pub favorited: bool,
}

impl From<&filen_types::api::v3::socket::FolderMove<'_>> for FolderMove {
	fn from(event: &filen_types::api::v3::socket::FolderMove<'_>) -> Self {
		Self {
			name: event.name.as_borrowed_cow().into_owned_cow(),
			uuid: event.uuid,
			parent: event.parent,
			timestamp: event.timestamp,
			favorited: event.favorited,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]

pub struct FolderSubCreated {
	pub name: EncryptedString<'static>,
	pub uuid: UuidStr,
	pub parent: UuidStr,
	pub timestamp: DateTime<Utc>,
	pub favorited: bool,
}

impl From<&filen_types::api::v3::socket::FolderSubCreated<'_>> for FolderSubCreated {
	fn from(event: &filen_types::api::v3::socket::FolderSubCreated<'_>) -> Self {
		Self {
			name: event.name.as_borrowed_cow().into_owned_cow(),
			uuid: event.uuid,
			parent: event.parent,
			timestamp: event.timestamp,
			favorited: event.favorited,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]

pub struct FolderRestore {
	pub name: EncryptedString<'static>,
	pub uuid: UuidStr,
	pub parent: UuidStr,
	pub timestamp: DateTime<Utc>,
	pub favorited: bool,
}

impl From<&filen_types::api::v3::socket::FolderRestore<'_>> for FolderRestore {
	fn from(event: &filen_types::api::v3::socket::FolderRestore<'_>) -> Self {
		Self {
			name: event.name.as_borrowed_cow().into_owned_cow(),
			uuid: event.uuid,
			parent: event.parent,
			timestamp: event.timestamp,
			favorited: event.favorited,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]

pub struct FolderColorChanged {
	pub uuid: UuidStr,
	pub color: DirColor,
}

impl From<&filen_types::api::v3::socket::FolderColorChanged<'_>> for FolderColorChanged {
	fn from(event: &filen_types::api::v3::socket::FolderColorChanged<'_>) -> Self {
		Self {
			uuid: event.uuid,
			color: DirColor::from(event.color.as_borrowed_cow()),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChatMessagePartialEncrypted {
	pub uuid: UuidStr,
	pub sender_id: u64,
	pub sender_email: String,
	pub sender_avatar: Option<String>,
	pub sender_nick_name: String,
	pub message: EncryptedString<'static>,
}

impl From<&filen_types::api::v3::chat::messages::ChatMessagePartialEncrypted<'_>>
	for ChatMessagePartialEncrypted
{
	fn from(
		message: &filen_types::api::v3::chat::messages::ChatMessagePartialEncrypted<'_>,
	) -> Self {
		Self {
			uuid: message.uuid,
			sender_id: message.sender_id,
			sender_email: message.sender_email.to_string(),
			sender_avatar: message.sender_avatar.as_ref().map(|s| s.to_string()),
			sender_nick_name: message.sender_nick_name.to_string(),
			message: message.message.as_borrowed_cow().into_owned_cow(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]

pub struct ChatMessageNew {
	pub chat: UuidStr,
	pub inner: ChatMessagePartialEncrypted,
	pub reply_to: Option<ChatMessagePartialEncrypted>,
	pub embed_disabled: bool,
	pub sent_timestamp: DateTime<Utc>,
}

impl From<&filen_types::api::v3::socket::ChatMessageNew<'_>> for ChatMessageNew {
	fn from(event: &filen_types::api::v3::socket::ChatMessageNew<'_>) -> Self {
		Self {
			chat: event.chat,
			inner: (&event.inner).into(),
			reply_to: event.reply_to.as_ref().map(|msg| msg.into()),
			embed_disabled: event.embed_disabled,
			sent_timestamp: event.sent_timestamp,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]

pub struct ChatTyping {
	pub chat: UuidStr,
	pub sender_avatar: Option<String>,
	pub sender_email: String,
	pub sender_nick_name: String,
	pub sender_id: u64,
	pub timestamp: DateTime<Utc>,
	pub typing_type: ChatTypingType,
}

impl From<&filen_types::api::v3::socket::ChatTyping<'_>> for ChatTyping {
	fn from(event: &filen_types::api::v3::socket::ChatTyping<'_>) -> Self {
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

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChatConversationsNew {
	pub uuid: UuidStr,
	pub metadata: EncryptedString<'static>,
	pub added_timestamp: DateTime<Utc>,
}

impl From<&filen_types::api::v3::socket::ChatConversationsNew<'_>> for ChatConversationsNew {
	fn from(event: &filen_types::api::v3::socket::ChatConversationsNew<'_>) -> Self {
		Self {
			uuid: event.uuid,
			metadata: event.metadata.as_borrowed_cow().into_owned_cow(),
			added_timestamp: event.added_timestamp,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NoteContentEdited {
	pub note: UuidStr,
	pub content: EncryptedString<'static>,
	pub note_type: NoteType,
	pub editor_id: u64,
	pub edited_timestamp: DateTime<Utc>,
}

impl From<&filen_types::api::v3::socket::NoteContentEdited<'_>> for NoteContentEdited {
	fn from(event: &filen_types::api::v3::socket::NoteContentEdited<'_>) -> Self {
		Self {
			note: event.note,
			content: event.content.as_borrowed_cow().into_owned_cow(),
			note_type: event.note_type,
			editor_id: event.editor_id,
			edited_timestamp: event.edited_timestamp,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NoteTitleEdited {
	pub note: UuidStr,
	pub title: EncryptedString<'static>,
}

impl From<&filen_types::api::v3::socket::NoteTitleEdited<'_>> for NoteTitleEdited {
	fn from(event: &filen_types::api::v3::socket::NoteTitleEdited<'_>) -> Self {
		Self {
			note: event.note,
			title: event.title.as_borrowed_cow().into_owned_cow(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NoteParticipantNew {
	pub note: UuidStr,
	pub participant: NoteParticipant,
}

impl From<&filen_types::api::v3::socket::NoteParticipantNew<'_>> for NoteParticipantNew {
	fn from(event: &filen_types::api::v3::socket::NoteParticipantNew<'_>) -> Self {
		Self {
			note: event.note,
			participant: NoteParticipant::from(event.participant.as_borrowed_cow()),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChatMessageEdited {
	pub chat: UuidStr,
	pub uuid: UuidStr,
	pub message: EncryptedString<'static>,
	pub edited_timestamp: DateTime<Utc>,
}

impl From<&filen_types::api::v3::socket::ChatMessageEdited<'_>> for ChatMessageEdited {
	fn from(event: &filen_types::api::v3::socket::ChatMessageEdited<'_>) -> Self {
		Self {
			chat: event.chat,
			uuid: event.uuid,
			message: event.message.as_borrowed_cow().into_owned_cow(),
			edited_timestamp: event.edited_timestamp,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChatConversationNameEdited {
	pub uuid: UuidStr,
	pub name: EncryptedString<'static>,
}

impl From<&filen_types::api::v3::socket::ChatConversationNameEdited<'_>>
	for ChatConversationNameEdited
{
	fn from(event: &filen_types::api::v3::socket::ChatConversationNameEdited<'_>) -> Self {
		Self {
			uuid: event.uuid,
			name: event.name.as_borrowed_cow().into_owned_cow(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ContactRequestReceived {
	pub uuid: UuidStr,
	pub sender_id: u64,
	pub sender_email: String,
	pub sender_avatar: Option<String>,
	pub sender_nick_name: Option<String>,
	pub sent_timestamp: DateTime<Utc>,
}

impl From<&filen_types::api::v3::socket::ContactRequestReceived<'_>> for ContactRequestReceived {
	fn from(event: &filen_types::api::v3::socket::ContactRequestReceived<'_>) -> Self {
		Self {
			uuid: event.uuid,
			sender_id: event.sender_id,
			sender_email: event.sender_email.to_string(),
			sender_avatar: event.sender_avatar.as_ref().map(|s| s.to_string()),
			sender_nick_name: event.sender_nick_name.as_ref().map(|s| s.to_string()),
			sent_timestamp: event.sent_timestamp,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ItemFavorite {
	pub uuid: UuidStr,
	pub item_type: ObjectType,
	pub value: bool,
	pub metadata: EncryptedString<'static>,
}

impl From<&filen_types::api::v3::socket::ItemFavorite<'_>> for ItemFavorite {
	fn from(event: &filen_types::api::v3::socket::ItemFavorite<'_>) -> Self {
		Self {
			uuid: event.uuid,
			item_type: event.item_type,
			value: event.value,
			metadata: event.metadata.as_borrowed_cow().into_owned_cow(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChatConversationParticipantNew {
	pub chat: UuidStr,
	pub user_id: u64,
	pub email: String,
	pub avatar: Option<String>,
	pub nick_name: Option<String>,
	pub metadata: EncryptedString<'static>,
	pub permissions_add: bool,
	pub added_timestamp: DateTime<Utc>,
}

impl From<&filen_types::api::v3::socket::ChatConversationParticipantNew<'_>>
	for ChatConversationParticipantNew
{
	fn from(event: &filen_types::api::v3::socket::ChatConversationParticipantNew<'_>) -> Self {
		Self {
			chat: event.chat,
			user_id: event.user_id,
			email: event.email.to_string(),
			avatar: event.avatar.as_ref().map(|s| s.to_string()),
			nick_name: event.nick_name.as_ref().map(|s| s.to_string()),
			metadata: event.metadata.as_borrowed_cow().into_owned_cow(),
			permissions_add: event.permissions_add,
			added_timestamp: event.added_timestamp,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FolderMetadataChanged {
	pub uuid: UuidStr,
	pub name: EncryptedString<'static>,
}

impl From<&filen_types::api::v3::socket::FolderMetadataChanged<'_>> for FolderMetadataChanged {
	fn from(event: &filen_types::api::v3::socket::FolderMetadataChanged<'_>) -> Self {
		Self {
			uuid: event.uuid,
			name: event.name.as_borrowed_cow().into_owned_cow(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FileMetadataChanged {
	pub uuid: UuidStr,
	pub name: EncryptedString<'static>,
	pub metadata: EncryptedString<'static>,
	pub old_metadata: EncryptedString<'static>,
}

impl From<&filen_types::api::v3::socket::FileMetadataChanged<'_>> for FileMetadataChanged {
	fn from(event: &filen_types::api::v3::socket::FileMetadataChanged<'_>) -> Self {
		Self {
			uuid: event.uuid,
			name: event.name.as_borrowed_cow().into_owned_cow(),
			metadata: event.metadata.as_borrowed_cow().into_owned_cow(),
			old_metadata: event.old_metadata.as_borrowed_cow().into_owned_cow(),
		}
	}
}
