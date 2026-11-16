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
	fs::Uuid,
	traits::{CowHelpers, CowHelpersExt},
};

use crate::{
	chats::{Chat, ChatMessage},
	js::{Dir, DirColor, DirMeta, File, FileMeta, NonRootItemTagged},
	notes::NoteParticipant,
	socket::events::{ChatConversationParticipantNew, DecryptedSocketEvent},
};

use crate::socket::events::{
	DecryptedChatEvent, DecryptedContactEvent, DecryptedDriveEvent, DecryptedGeneralEvent,
	DecryptedLockEvent, DecryptedNoteEvent,
};

#[js_type(export, no_deser, tagged)]
pub enum SocketEvent {
	AuthSuccess,
	AuthFailed,
	Reconnecting,
	Unsubscribed,
	Drive(DriveSocketEvent),
	DriveMalformed(DriveMalformedSocketEvent),
	Chat(ChatSocketEvent),
	Note(NoteSocketEvent),
	Contact(ContactSocketEvent),
	General(GeneralSocketEvent),
	Lock(LockSocketEvent),
}

#[js_type(export, no_deser)]
pub struct DriveSocketEvent {
	pub inner: DriveEvent,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub drive_message_id: u64,
}

/// A drive event that was received (its id is known) but whose kind is unknown
/// or whose payload could not be parsed/decrypted. Carries only the id so apps
/// tracking drive message ids can advance their own counters past it.
#[js_type(export, no_deser)]
pub struct DriveMalformedSocketEvent {
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub drive_message_id: u64,
}

#[js_type(export, no_deser)]
pub struct ChatSocketEvent {
	pub inner: ChatEvent,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub chat_message_id: u64,
}

#[js_type(export, no_deser)]
pub struct NoteSocketEvent {
	pub inner: NoteEvent,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub note_message_id: u64,
}

#[js_type(export, no_deser)]
pub struct ContactSocketEvent {
	pub inner: ContactEvent,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub contact_message_id: u64,
}

#[js_type(export, no_deser)]
pub struct GeneralSocketEvent {
	pub inner: GeneralEvent,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub general_message_id: u64,
}

/// A user lock's lease began or ended, whoever caused it. A hint for showing lock state, never
/// proof of holding a lock: announcements may arrive out of order or be lost, and nothing is
/// replayed after a reconnect.
#[js_type(export, no_deser)]
pub struct LockSocketEvent {
	pub inner: LockAnnouncementEvent,
	/// The user's lock announcement counter (`lock` in `v3/messageIds`). It does not give the
	/// order the changes happened in.
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub lock_message_id: u64,
}

#[js_type(export, no_deser, tagged)]
pub enum DriveEvent {
	FileNew(FileNew),
	FileRestore(FileRestore),
	FileMove(FileMove),
	FileTrash(FileTrash),
	FileArchived(FileArchived),
	FileArchiveRestored(FileArchiveRestored),
	FileDeletedPermanent(FileDeletedPermanent),
	FileMetadataChanged(FileMetadataChanged),
	FolderSubCreated(FolderSubCreated),
	FolderMove(FolderMove),
	FolderTrash(FolderTrash),
	FolderRestore(FolderRestore),
	FolderColorChanged(FolderColorChanged),
	FolderMetadataChanged(FolderMetadataChanged),
	FolderDeletedPermanent(FolderDeletedPermanent),
	ItemFavorite(ItemFavorite),
	TrashEmpty,
	DeleteAll,
	DeleteVersioned,
}

#[js_type(export, no_deser, tagged)]
pub enum ChatEvent {
	MessageNew(ChatMessageNew),
	Typing(ChatTyping),
	ConversationsNew(ChatConversationsNew),
	MessageDelete(ChatMessageDelete),
	MessageEmbedDisabled(ChatMessageEmbedDisabled),
	ConversationParticipantLeft(ChatConversationParticipantLeft),
	ConversationDeleted(ChatConversationDeleted),
	MessageEdited(ChatMessageEdited),
	ConversationNameEdited(ChatConversationNameEdited),
	ConversationParticipantNew(ChatConversationParticipantNew),
}

#[js_type(export, no_deser, tagged)]
pub enum NoteEvent {
	ContentEdited(NoteContentEdited),
	Archived(NoteArchived),
	Deleted(NoteDeleted),
	TitleEdited(NoteTitleEdited),
	ParticipantPermissions(NoteParticipantPermissions),
	Restored(NoteRestored),
	ParticipantRemoved(NoteParticipantRemoved),
	ParticipantNew(NoteParticipantNew),
	New(NoteNew),
}

#[js_type(export, no_deser, tagged)]
pub enum ContactEvent {
	ContactRequestReceived(ContactRequestReceived),
}

#[js_type(export, no_deser, tagged)]
pub enum GeneralEvent {
	PasswordChanged,
	NewEvent(NewEvent),
}

#[js_type(export, no_deser, tagged)]
pub enum LockAnnouncementEvent {
	/// A new lease began: an acquire took the free lock.
	Acquired(LockAnnouncement),
	/// The lease's holder released it.
	Released(LockAnnouncement),
	/// The lease ran out without being released.
	Timeout(LockAnnouncement),
}

/// One lease of a user lock, as a lock announcement reports it.
#[js_type]
pub struct LockAnnouncement {
	/// The lock's resource (e.g. `drive-write`).
	pub resource: String,
	/// The lease's holder: the `uuid` of the request that acquired it (the old holder's for a
	/// timeout).
	pub uuid: Uuid,
	/// Identifies the lease, opaque; a holder acquiring again gets a new one.
	pub lease_id: String,
	/// Server time of the announcement. Informational only: do not order by it.
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
}

impl From<&crate::socket::events::LockAnnouncement<'_>> for LockAnnouncement {
	fn from(event: &crate::socket::events::LockAnnouncement<'_>) -> Self {
		Self {
			resource: event.resource.to_string(),
			uuid: event.uuid,
			lease_id: event.lease_id.to_string(),
			timestamp: event.timestamp,
		}
	}
}

impl From<&DecryptedSocketEvent<'_>> for SocketEvent {
	fn from(event: &DecryptedSocketEvent<'_>) -> Self {
		match event {
			DecryptedSocketEvent::AuthSuccess => SocketEvent::AuthSuccess,
			DecryptedSocketEvent::AuthFailed => SocketEvent::AuthFailed,
			DecryptedSocketEvent::Reconnecting => SocketEvent::Reconnecting,
			DecryptedSocketEvent::Unsubscribed => SocketEvent::Unsubscribed,
			DecryptedSocketEvent::Drive {
				inner,
				drive_message_id,
			} => SocketEvent::Drive(DriveSocketEvent {
				drive_message_id: *drive_message_id,
				inner: match inner {
					DecryptedDriveEvent::FileNew(e) => DriveEvent::FileNew(e.into()),
					DecryptedDriveEvent::FileRestore(e) => DriveEvent::FileRestore(e.into()),
					DecryptedDriveEvent::FileMove(e) => DriveEvent::FileMove(e.into()),
					DecryptedDriveEvent::FileTrash(e) => DriveEvent::FileTrash(e.clone()),
					DecryptedDriveEvent::FileArchived(e) => DriveEvent::FileArchived(e.clone()),
					DecryptedDriveEvent::FileArchiveRestored(e) => {
						DriveEvent::FileArchiveRestored(e.into())
					}
					DecryptedDriveEvent::FileDeletedPermanent(e) => {
						DriveEvent::FileDeletedPermanent(e.clone())
					}
					DecryptedDriveEvent::FileMetadataChanged(e) => {
						DriveEvent::FileMetadataChanged(e.into())
					}
					DecryptedDriveEvent::FolderSubCreated(e) => {
						DriveEvent::FolderSubCreated(e.into())
					}
					DecryptedDriveEvent::FolderMove(e) => DriveEvent::FolderMove(e.into()),
					DecryptedDriveEvent::FolderTrash(e) => DriveEvent::FolderTrash(e.clone()),
					DecryptedDriveEvent::FolderRestore(e) => DriveEvent::FolderRestore(e.into()),
					DecryptedDriveEvent::FolderColorChanged(e) => {
						DriveEvent::FolderColorChanged(e.into())
					}
					DecryptedDriveEvent::FolderMetadataChanged(e) => {
						DriveEvent::FolderMetadataChanged(e.into())
					}
					DecryptedDriveEvent::FolderDeletedPermanent(e) => {
						DriveEvent::FolderDeletedPermanent(e.clone())
					}
					DecryptedDriveEvent::ItemFavorite(e) => DriveEvent::ItemFavorite(e.into()),
					DecryptedDriveEvent::TrashEmpty => DriveEvent::TrashEmpty,
					DecryptedDriveEvent::DeleteAll => DriveEvent::DeleteAll,
					DecryptedDriveEvent::DeleteVersioned => DriveEvent::DeleteVersioned,
				},
			}),
			DecryptedSocketEvent::DriveMalformed { drive_message_id } => {
				SocketEvent::DriveMalformed(DriveMalformedSocketEvent {
					drive_message_id: *drive_message_id,
				})
			}
			DecryptedSocketEvent::Chat {
				inner,
				chat_message_id,
			} => SocketEvent::Chat(ChatSocketEvent {
				chat_message_id: *chat_message_id,
				inner: match inner {
					DecryptedChatEvent::MessageNew(e) => ChatEvent::MessageNew(e.into()),
					DecryptedChatEvent::Typing(e) => ChatEvent::Typing(e.into()),
					DecryptedChatEvent::ConversationsNew(e) => {
						ChatEvent::ConversationsNew(e.into())
					}
					DecryptedChatEvent::MessageDelete(e) => ChatEvent::MessageDelete(e.clone()),
					DecryptedChatEvent::MessageEmbedDisabled(e) => {
						ChatEvent::MessageEmbedDisabled(e.clone())
					}
					DecryptedChatEvent::ConversationParticipantLeft(e) => {
						ChatEvent::ConversationParticipantLeft(e.clone())
					}
					DecryptedChatEvent::ConversationDeleted(e) => {
						ChatEvent::ConversationDeleted(e.clone())
					}
					DecryptedChatEvent::MessageEdited(e) => ChatEvent::MessageEdited(e.into()),
					DecryptedChatEvent::ConversationNameEdited(e) => {
						ChatEvent::ConversationNameEdited(e.into())
					}
					DecryptedChatEvent::ConversationParticipantNew(e) => {
						ChatEvent::ConversationParticipantNew(e.clone())
					}
				},
			}),
			DecryptedSocketEvent::Note {
				inner,
				note_message_id,
			} => SocketEvent::Note(NoteSocketEvent {
				note_message_id: *note_message_id,
				inner: match inner {
					DecryptedNoteEvent::ContentEdited(e) => NoteEvent::ContentEdited(e.into()),
					DecryptedNoteEvent::Archived(e) => NoteEvent::Archived(e.clone()),
					DecryptedNoteEvent::Deleted(e) => NoteEvent::Deleted(e.clone()),
					DecryptedNoteEvent::TitleEdited(e) => NoteEvent::TitleEdited(e.into()),
					DecryptedNoteEvent::ParticipantPermissions(e) => {
						NoteEvent::ParticipantPermissions(e.clone())
					}
					DecryptedNoteEvent::Restored(e) => NoteEvent::Restored(e.clone()),
					DecryptedNoteEvent::ParticipantRemoved(e) => {
						NoteEvent::ParticipantRemoved(e.clone())
					}
					DecryptedNoteEvent::ParticipantNew(e) => NoteEvent::ParticipantNew(e.into()),
					DecryptedNoteEvent::New(e) => NoteEvent::New(e.into()),
				},
			}),
			DecryptedSocketEvent::Contact {
				inner,
				contact_message_id,
			} => SocketEvent::Contact(ContactSocketEvent {
				contact_message_id: *contact_message_id,
				inner: match inner {
					DecryptedContactEvent::ContactRequestReceived(e) => {
						ContactEvent::ContactRequestReceived(e.into())
					}
				},
			}),
			DecryptedSocketEvent::General {
				inner,
				general_message_id,
			} => SocketEvent::General(GeneralSocketEvent {
				general_message_id: *general_message_id,
				inner: match inner {
					DecryptedGeneralEvent::PasswordChanged => GeneralEvent::PasswordChanged,
					DecryptedGeneralEvent::NewEvent(e) => GeneralEvent::NewEvent(e.into()),
				},
			}),
			DecryptedSocketEvent::Lock {
				inner,
				lock_message_id,
			} => SocketEvent::Lock(LockSocketEvent {
				lock_message_id: *lock_message_id,
				inner: match inner {
					DecryptedLockEvent::Acquired(e) => LockAnnouncementEvent::Acquired(e.into()),
					DecryptedLockEvent::Released(e) => LockAnnouncementEvent::Released(e.into()),
					DecryptedLockEvent::Timeout(e) => LockAnnouncementEvent::Timeout(e.into()),
				},
			}),
		}
	}
}

#[js_type]
pub struct NewEvent {
	pub uuid: Uuid,
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
pub struct FileArchiveRestored {
	pub current_uuid: Uuid,
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
	pub uuid: Uuid,
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
	pub chat: Uuid,
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
	pub note: Uuid,
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
	pub note: Uuid,
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
	pub note: Uuid,
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
	pub note: Uuid,
}
impl From<&crate::socket::events::NoteNew> for NoteNew {
	fn from(value: &crate::socket::events::NoteNew) -> Self {
		Self { note: value.note }
	}
}

#[js_type]
pub struct ChatMessageEdited {
	pub chat: Uuid,
	pub uuid: Uuid,
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
	pub chat: Uuid,
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
	pub uuid: Uuid,
	pub sender_id: u64,
	pub sender_email: String,
	pub sender_avatar: Option<String>,
	pub sender_nick_name: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		serde(with = "chrono::serde::ts_milliseconds"),
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
	pub uuid: Uuid,
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
	pub uuid: Uuid,
	pub metadata: FileMeta,
}

impl From<&crate::socket::events::FileMetadataChanged<'_>> for FileMetadataChanged {
	fn from(event: &crate::socket::events::FileMetadataChanged<'_>) -> Self {
		Self {
			uuid: event.uuid,
			metadata: event.metadata.as_borrowed_cow().into(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The malformed marker must reach app listeners (not be filtered out) so apps
	/// tracking drive message ids can advance their own counters past it.
	#[test]
	fn drive_malformed_passes_through_to_the_js_event() {
		let decrypted = DecryptedSocketEvent::DriveMalformed {
			drive_message_id: 42,
		};
		let js: SocketEvent = (&decrypted).into();
		assert!(matches!(
			js,
			SocketEvent::DriveMalformed(DriveMalformedSocketEvent {
				drive_message_id: 42
			})
		));
	}

	#[test]
	fn a_lock_announcement_reaches_the_js_event_with_every_field() {
		let decrypted = DecryptedSocketEvent::Lock {
			inner: DecryptedLockEvent::Timeout(crate::socket::events::LockAnnouncement {
				resource: "notes-write".into(),
				uuid: Uuid::from_u128(1),
				lease_id: "lease-7".into(),
				timestamp: DateTime::from_timestamp_millis(1_760_270_400_123).unwrap(),
			}),
			lock_message_id: 9,
		};
		let js: SocketEvent = (&decrypted).into();
		assert_eq!(
			js,
			SocketEvent::Lock(LockSocketEvent {
				inner: LockAnnouncementEvent::Timeout(LockAnnouncement {
					resource: "notes-write".to_string(),
					uuid: Uuid::from_u128(1),
					lease_id: "lease-7".to_string(),
					timestamp: DateTime::from_timestamp_millis(1_760_270_400_123).unwrap(),
				}),
				lock_message_id: 9,
			})
		);
	}
}
