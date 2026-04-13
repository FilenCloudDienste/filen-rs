use std::{borrow::Cow, ops::Deref};

use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::{
	api::v3::{
		chat::messages::ChatMessageEncrypted,
		dir::color::DirColor,
		notes::NoteType,
		socket::{
			ChatEventType, ContactEventType, DriveEventType, GeneralEventType, MessageType,
			NoteEventType, PacketType, SocketEvent,
		},
	},
	auth::FileEncryptionVersion,
	crypto::{EncryptedString, MaybeEncrypted},
	fs::UuidStr,
	traits::CowHelpers,
};
use rsa::RsaPrivateKey;
use stable_deref_trait::StableDeref;
use yoke::{Yoke, Yokeable};

pub use filen_types::api::v3::socket::{
	ChatConversationDeleted, ChatConversationParticipantLeft, ChatMessageDelete,
	ChatMessageEmbedDisabled, ChatTyping, ContactRequestReceived, FileArchived,
	FileDeletedPermanent, FileTrash, FolderColorChanged, FolderDeletedPermanent, FolderTrash,
	NewEvent, NoteArchived, NoteDeleted, NoteParticipantPermissions, NoteParticipantRemoved,
	NoteRestored,
};

use crate::{
	Error, ErrorKind,
	chats::{Chat, ChatMessage, ChatParticipant},
	crypto::{
		notes_and_chats::{NoteOrChatCarrierCryptoExt, NoteOrChatKeyStruct},
		shared::MetaCrypter,
	},
	error::ResultExt,
	fs::{
		categories::{NonRootItemType, Normal},
		dir::{RemoteDirectory, meta::DirectoryMeta},
		file::{RemoteFile, meta::FileMeta},
	},
	notes::NoteParticipant,
	runtime,
};

use super::consts::{ARCHIVED_EVENT_PREFIX, AUTHED_TRUE};

pub(super) fn try_parse_message_from_str<T>(
	msg: T,
) -> Result<Option<Yoke<SocketEvent<'static>, T>>, Error>
where
	T: StableDeref,
	<T as Deref>::Target: AsRef<str> + 'static,
{
	let yoked: Yoke<Option<SocketEvent<'static>>, T> = Yoke::try_attach_to_cart(msg, |msg| {
		let msg = msg.as_ref();
		let mut text_bytes = msg.bytes();
		let Some(packet_type) = text_bytes.next() else {
			return Err(Error::custom(
				ErrorKind::Server,
				"Empty message received over WebSocket",
			));
		};

		match PacketType::try_from(packet_type) {
			Err(e) => {
				return Err(Error::custom(
					ErrorKind::Server,
					format!("Invalid packet type: {}", e),
				));
			}
			Ok(PacketType::Message) => {}
			Ok(PacketType::Connect) => {
				return Err(Error::custom(
					ErrorKind::InvalidState,
					"Received unexpected connect packet after initialization",
				));
			}
			Ok(_) => {
				return Ok(None);
			}
		}

		let Some(message_type) = text_bytes.next() else {
			return Err(Error::custom(
				ErrorKind::Server,
				"PacketType::Message received with no MessageType",
			));
		};

		match MessageType::try_from(message_type) {
			Err(e) => {
				return Err(Error::custom(
					ErrorKind::Server,
					format!("Invalid message type: {}", e),
				));
			}
			Ok(MessageType::Event) => {
				// continue
			}
			Ok(_) => {
				// ignore other message types for now
				return Ok(None);
			}
		}

		let event_str = &msg[2..];

		if event_str == AUTHED_TRUE {
			// ignore authed true messages
			return Ok(None);
		}

		// these are duplicates of FileVersioned, so we can just ignore them
		if event_str.starts_with(ARCHIVED_EVENT_PREFIX) {
			return Ok(None);
		}

		match serde_json::from_str::<SocketEvent>(event_str) {
			Ok(parsed_event) => Ok(Some(parsed_event)),
			Err(e) => Err(Error::custom_with_source(
				ErrorKind::Conversion,
				e,
				Some("deserializing SocketEvent"),
			)),
		}
	})?;

	Ok(yoked
		.try_map_project(|maybe_event: Option<SocketEvent<'_>>, _| maybe_event.ok_or(()))
		.ok())
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers, Yokeable)]
pub enum DecryptedSocketEvent<'a> {
	// Local events (not from wire, no message ID)
	AuthSuccess,
	AuthFailed,
	Reconnecting,
	Unsubscribed,

	// Wire events by category
	Drive {
		inner: DecryptedDriveEvent<'a>,
		drive_message_id: u64,
	},
	Chat {
		inner: DecryptedChatEvent<'a>,
		chat_message_id: u64,
	},
	Note {
		inner: DecryptedNoteEvent<'a>,
		note_message_id: u64,
	},
	Contact {
		inner: DecryptedContactEvent<'a>,
		contact_message_id: u64,
	},
	General {
		inner: DecryptedGeneralEvent<'a>,
		general_message_id: u64,
	},
}

impl DecryptedSocketEvent<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			Self::AuthSuccess => "authSuccess",
			Self::AuthFailed => "authFailed",
			Self::Reconnecting => "reconnecting",
			Self::Unsubscribed => "unsubscribed",
			Self::Drive { inner, .. } => inner.event_type(),
			Self::Chat { inner, .. } => inner.event_type(),
			Self::Note { inner, .. } => inner.event_type(),
			Self::Contact { inner, .. } => inner.event_type(),
			Self::General { inner, .. } => inner.event_type(),
		}
	}

	pub fn message_id(&self) -> Option<u64> {
		match self {
			Self::AuthSuccess | Self::AuthFailed | Self::Reconnecting | Self::Unsubscribed => None,
			Self::Drive {
				drive_message_id, ..
			} => Some(*drive_message_id),
			Self::Chat {
				chat_message_id, ..
			} => Some(*chat_message_id),
			Self::Note {
				note_message_id, ..
			} => Some(*note_message_id),
			Self::Contact {
				contact_message_id, ..
			} => Some(*contact_message_id),
			Self::General {
				general_message_id, ..
			} => Some(*general_message_id),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub enum DecryptedDriveEvent<'a> {
	FileArchiveRestored(FileArchiveRestored),
	FileNew(FileNew),
	FileRestore(FileRestore),
	FileMove(FileMove),
	FileTrash(FileTrash),
	FileArchived(FileArchived),
	FolderTrash(FolderTrash),
	FolderMove(FolderMove),
	FolderSubCreated(FolderSubCreated),
	FolderRestore(FolderRestore),
	FolderColorChanged(FolderColorChanged<'a>),
	TrashEmpty,
	ItemFavorite(ItemFavorite),
	FileDeletedPermanent(FileDeletedPermanent),
	FolderMetadataChanged(FolderMetadataChanged<'a>),
	FolderDeletedPermanent(FolderDeletedPermanent),
	FileMetadataChanged(FileMetadataChanged<'a>),
	DeleteAll,
	DeleteVersioned,
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub enum DecryptedChatEvent<'a> {
	MessageNew(ChatMessageNew),
	Typing(ChatTyping<'a>),
	ConversationsNew(ChatConversationsNew),
	MessageDelete(ChatMessageDelete),
	MessageEmbedDisabled(ChatMessageEmbedDisabled),
	ConversationParticipantLeft(ChatConversationParticipantLeft),
	ConversationDeleted(ChatConversationDeleted),
	MessageEdited(ChatMessageEdited<'a>),
	ConversationNameEdited(ChatConversationNameEdited<'a>),
	ConversationParticipantNew(ChatConversationParticipantNew),
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub enum DecryptedNoteEvent<'a> {
	ContentEdited(NoteContentEdited<'a>),
	Archived(NoteArchived),
	Deleted(NoteDeleted),
	TitleEdited(NoteTitleEdited<'a>),
	ParticipantPermissions(NoteParticipantPermissions),
	Restored(NoteRestored),
	ParticipantRemoved(NoteParticipantRemoved),
	ParticipantNew(NoteParticipantNew),
	New(NoteNew),
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub enum DecryptedContactEvent<'a> {
	ContactRequestReceived(ContactRequestReceived<'a>),
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub enum DecryptedGeneralEvent<'a> {
	PasswordChanged,
	NewEvent(NewEvent<'a>),
}

impl DecryptedDriveEvent<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			Self::FileArchiveRestored(_) => "fileArchiveRestored",
			Self::FileNew(_) => "fileNew",
			Self::FileRestore(_) => "fileRestore",
			Self::FileMove(_) => "fileMove",
			Self::FileTrash(_) => "fileTrash",
			Self::FileArchived(_) => "fileArchived",
			Self::FolderTrash(_) => "folderTrash",
			Self::FolderMove(_) => "folderMove",
			Self::FolderSubCreated(_) => "folderSubCreated",
			Self::FolderRestore(_) => "folderRestore",
			Self::FolderColorChanged(_) => "folderColorChanged",
			Self::TrashEmpty => "trashEmpty",
			Self::ItemFavorite(_) => "itemFavorite",
			Self::FileDeletedPermanent(_) => "fileDeletedPermanent",
			Self::FolderMetadataChanged(_) => "folderMetadataChanged",
			Self::FolderDeletedPermanent(_) => "folderDeletedPermanent",
			Self::FileMetadataChanged(_) => "fileMetadataChanged",
			Self::DeleteAll => "deleteAll",
			Self::DeleteVersioned => "deleteVersioned",
		}
	}
}

impl DecryptedChatEvent<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			Self::MessageNew(_) => "chatMessageNew",
			Self::Typing(_) => "chatTyping",
			Self::ConversationsNew(_) => "chatConversationsNew",
			Self::MessageDelete(_) => "chatMessageDelete",
			Self::MessageEmbedDisabled(_) => "chatMessageEmbedDisabled",
			Self::ConversationParticipantLeft(_) => "chatConversationParticipantLeft",
			Self::ConversationDeleted(_) => "chatConversationDeleted",
			Self::MessageEdited(_) => "chatMessageEdited",
			Self::ConversationNameEdited(_) => "chatConversationNameEdited",
			Self::ConversationParticipantNew(_) => "chatConversationParticipantNew",
		}
	}
}

impl DecryptedNoteEvent<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			Self::ContentEdited(_) => "noteContentEdited",
			Self::Archived(_) => "noteArchived",
			Self::Deleted(_) => "noteDeleted",
			Self::TitleEdited(_) => "noteTitleEdited",
			Self::ParticipantPermissions(_) => "noteParticipantPermissions",
			Self::Restored(_) => "noteRestored",
			Self::ParticipantRemoved(_) => "noteParticipantRemoved",
			Self::ParticipantNew(_) => "noteParticipantNew",
			Self::New(_) => "noteNew",
		}
	}
}

impl DecryptedContactEvent<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			Self::ContactRequestReceived(_) => "contactRequestReceived",
		}
	}
}

impl DecryptedGeneralEvent<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			Self::PasswordChanged => "passwordChanged",
			Self::NewEvent(_) => "newEvent",
		}
	}
}

impl DecryptedSocketEvent<'_> {
	pub(crate) async fn try_from_encrypted<T>(
		crypter: &impl MetaCrypter,
		private_key: &RsaPrivateKey,
		user_id: u64,
		event: Yoke<SocketEvent<'static>, T>,
	) -> Result<Yoke<DecryptedSocketEvent<'static>, T>, Error>
	where
		T: StableDeref + Send,
		<T as Deref>::Target: AsRef<str> + 'static,
	{
		runtime::do_cpu_intensive(|| {
			event.try_map_project(|e, _| {
				Ok(match e {
					SocketEvent::Drive {
						inner,
						drive_message_id,
					} => {
						let inner = match inner {
							DriveEventType::FileRename(e) => {
								DecryptedDriveEvent::FileMetadataChanged(
									FileMetadataChanged::blocking_from_encrypted(
										crypter, e.uuid, e.metadata,
									),
								)
							}
							DriveEventType::FileArchiveRestored(e) => {
								DecryptedDriveEvent::FileArchiveRestored(
									FileArchiveRestored::blocking_from_encrypted(crypter, e),
								)
							}
							DriveEventType::FileNew(e) => DecryptedDriveEvent::FileNew(
								FileNew::blocking_from_encrypted(crypter, e),
							),
							DriveEventType::FileRestore(e) => DecryptedDriveEvent::FileRestore(
								FileRestore::blocking_from_encrypted(crypter, e),
							),
							DriveEventType::FileMove(e) => DecryptedDriveEvent::FileMove(
								FileMove::blocking_from_encrypted(crypter, e),
							),
							DriveEventType::FileTrash(e) => DecryptedDriveEvent::FileTrash(e),
							DriveEventType::FileArchived(e) => DecryptedDriveEvent::FileArchived(e),
							DriveEventType::FolderRename(e) => {
								DecryptedDriveEvent::FolderMetadataChanged(
									FolderMetadataChanged::blocking_from_encrypted(
										crypter, e.uuid, e.name,
									),
								)
							}
							DriveEventType::FolderTrash(e) => DecryptedDriveEvent::FolderTrash(e),
							DriveEventType::FolderMove(e) => DecryptedDriveEvent::FolderMove(
								FolderMove::blocking_from_encrypted(crypter, e),
							),
							DriveEventType::FolderSubCreated(e) => {
								DecryptedDriveEvent::FolderSubCreated(
									FolderSubCreated::blocking_from_encrypted(crypter, e),
								)
							}
							DriveEventType::FolderRestore(e) => DecryptedDriveEvent::FolderRestore(
								FolderRestore::blocking_from_encrypted(crypter, e),
							),
							DriveEventType::FolderColorChanged(e) => {
								DecryptedDriveEvent::FolderColorChanged(e)
							}
							DriveEventType::FolderMetadataChanged(e) => {
								DecryptedDriveEvent::FolderMetadataChanged(
									FolderMetadataChanged::blocking_from_encrypted(
										crypter, e.uuid, e.meta,
									),
								)
							}
							DriveEventType::FolderDeletedPermanent(e) => {
								DecryptedDriveEvent::FolderDeletedPermanent(e)
							}
							DriveEventType::FileDeletedPermanent(e) => {
								DecryptedDriveEvent::FileDeletedPermanent(e)
							}
							DriveEventType::FileMetadataChanged(e) => {
								DecryptedDriveEvent::FileMetadataChanged(
									FileMetadataChanged::blocking_from_encrypted(
										crypter, e.uuid, e.metadata,
									),
								)
							}
							DriveEventType::ItemFavorite(e) => DecryptedDriveEvent::ItemFavorite(
								ItemFavorite::try_blocking_from_encrypted(crypter, e)?,
							),
							DriveEventType::TrashEmpty => DecryptedDriveEvent::TrashEmpty,
							DriveEventType::DeleteAll => DecryptedDriveEvent::DeleteAll,
							DriveEventType::DeleteVersioned => DecryptedDriveEvent::DeleteVersioned,
						};
						DecryptedSocketEvent::Drive {
							inner,
							drive_message_id,
						}
					}
					SocketEvent::Chat {
						inner,
						chat_message_id,
					} => {
						let inner = match inner {
							ChatEventType::ChatMessageNew(e) => DecryptedChatEvent::MessageNew(
								ChatMessageNew::try_blocking_from_rsa_encrypted(private_key, e)?,
							),
							ChatEventType::ChatTyping(e) => DecryptedChatEvent::Typing(e),
							ChatEventType::ChatConversationsNew(e) => {
								DecryptedChatEvent::ConversationsNew(
									ChatConversationsNew::blocking_from_rsa_encrypted(
										private_key,
										user_id,
										e,
									),
								)
							}
							ChatEventType::ChatMessageDelete(e) => {
								DecryptedChatEvent::MessageDelete(e)
							}
							ChatEventType::ChatMessageEmbedDisabled(e) => {
								DecryptedChatEvent::MessageEmbedDisabled(e)
							}
							ChatEventType::ChatConversationParticipantLeft(e) => {
								DecryptedChatEvent::ConversationParticipantLeft(e)
							}
							ChatEventType::ChatConversationDeleted(e) => {
								DecryptedChatEvent::ConversationDeleted(e)
							}
							ChatEventType::ChatMessageEdited(e) => {
								DecryptedChatEvent::MessageEdited(
									ChatMessageEdited::try_blocking_from_rsa_encrypted(
										private_key,
										e,
									)?,
								)
							}
							ChatEventType::ChatConversationNameEdited(e) => {
								DecryptedChatEvent::ConversationNameEdited(
									ChatConversationNameEdited::try_blocking_from_rsa_encrypted(
										private_key,
										e,
									)?,
								)
							}
							ChatEventType::ChatConversationParticipantNew(e) => {
								DecryptedChatEvent::ConversationParticipantNew(e.into())
							}
						};
						DecryptedSocketEvent::Chat {
							inner,
							chat_message_id,
						}
					}
					SocketEvent::Note {
						inner,
						note_message_id,
					} => {
						let inner = match inner {
							NoteEventType::NoteContentEdited(e) => {
								DecryptedNoteEvent::ContentEdited(
									NoteContentEdited::try_blocking_from_rsa_encrypted(
										private_key,
										e,
									)?,
								)
							}
							NoteEventType::NoteArchived(e) => DecryptedNoteEvent::Archived(e),
							NoteEventType::NoteDeleted(e) => DecryptedNoteEvent::Deleted(e),
							NoteEventType::NoteTitleEdited(e) => DecryptedNoteEvent::TitleEdited(
								NoteTitleEdited::try_blocking_from_rsa_encrypted(private_key, e)?,
							),
							NoteEventType::NoteParticipantPermissions(e) => {
								DecryptedNoteEvent::ParticipantPermissions(e)
							}
							NoteEventType::NoteRestored(e) => DecryptedNoteEvent::Restored(e),
							NoteEventType::NoteParticipantRemoved(e) => {
								DecryptedNoteEvent::ParticipantRemoved(e)
							}
							NoteEventType::NoteParticipantNew(e) => {
								DecryptedNoteEvent::ParticipantNew(e.into())
							}
							NoteEventType::NoteNew(e) => DecryptedNoteEvent::New(e.into()),
						};
						DecryptedSocketEvent::Note {
							inner,
							note_message_id,
						}
					}
					SocketEvent::Contact {
						inner,
						contact_message_id,
					} => {
						let inner = match inner {
							ContactEventType::ContactRequestReceived(e) => {
								DecryptedContactEvent::ContactRequestReceived(e)
							}
						};
						DecryptedSocketEvent::Contact {
							inner,
							contact_message_id,
						}
					}
					SocketEvent::General {
						inner,
						general_message_id,
					} => {
						let inner = match inner {
							GeneralEventType::PasswordChanged => {
								DecryptedGeneralEvent::PasswordChanged
							}
							GeneralEventType::NewEvent(e) => DecryptedGeneralEvent::NewEvent(e),
						};
						DecryptedSocketEvent::General {
							inner,
							general_message_id,
						}
					}
				})
			})
		})
		.await
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct FileRename<'a> {
	pub uuid: UuidStr,
	pub metadata: FileMeta<'a>,
}

impl<'a> FileRename<'a> {
	fn blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		event: filen_types::api::v3::socket::FileRename<'a>,
	) -> Self {
		Self {
			uuid: event.uuid,
			metadata: FileMeta::blocking_from_encrypted(
				event.metadata,
				crypter,
				FileEncryptionVersion::V2,
			),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileArchiveRestored {
	pub current_uuid: UuidStr,
	pub file: RemoteFile,
}

impl<'a> FileArchiveRestored {
	fn blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		event: filen_types::api::v3::socket::FileArchiveRestored<'a>,
	) -> Self {
		Self {
			current_uuid: event.current_uuid,
			file: RemoteFile {
				uuid: event.uuid,
				meta: FileMeta::blocking_from_encrypted(event.metadata, crypter, event.version)
					.into_owned_cow(),
				parent: event.parent,
				size: event.size,
				favorited: event.favorited,
				region: event.region.into_owned(),
				bucket: event.bucket.into_owned(),
				timestamp: event.timestamp,
				chunks: event.chunks,
			},
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNew(pub RemoteFile);

impl<'a> FileNew {
	fn blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		event: filen_types::api::v3::socket::FileNew<'a>,
	) -> Self {
		Self(RemoteFile {
			uuid: event.uuid,
			meta: FileMeta::blocking_from_encrypted(event.metadata, crypter, event.version)
				.into_owned_cow(),
			parent: event.parent,
			size: event.size,
			favorited: event.favorited,
			region: event.region.into_owned(),
			bucket: event.bucket.into_owned(),
			timestamp: event.timestamp,
			chunks: event.chunks,
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRestore(pub RemoteFile);

impl<'a> FileRestore {
	fn blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		event: filen_types::api::v3::socket::FileRestore<'a>,
	) -> Self {
		Self(RemoteFile {
			uuid: event.uuid,
			meta: FileMeta::blocking_from_encrypted(event.metadata, crypter, event.version)
				.into_owned_cow(),
			parent: event.parent,
			size: event.size,
			favorited: event.favorited,
			region: event.region.into_owned(),
			bucket: event.bucket.into_owned(),
			timestamp: event.timestamp,
			chunks: event.chunks,
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMove(pub RemoteFile);
impl<'a> FileMove {
	fn blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		event: filen_types::api::v3::socket::FileMove<'a>,
	) -> Self {
		Self(RemoteFile {
			uuid: event.uuid,
			meta: FileMeta::blocking_from_encrypted(event.metadata, crypter, event.version)
				.into_owned_cow(),
			parent: event.parent,
			size: event.size,
			favorited: event.favorited,
			region: event.region.into_owned(),
			bucket: event.bucket.into_owned(),
			timestamp: event.timestamp,
			chunks: event.chunks,
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderMove(pub RemoteDirectory);
impl<'a> FolderMove {
	fn blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		event: filen_types::api::v3::socket::FolderMove<'a>,
	) -> Self {
		Self(RemoteDirectory {
			uuid: event.uuid,
			parent: event.parent,
			color: DirColor::Default,
			favorited: event.favorited,
			timestamp: event.timestamp,
			meta: DirectoryMeta::blocking_from_encrypted(event.name, crypter).into_owned_cow(),
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSubCreated(pub RemoteDirectory);
impl<'a> FolderSubCreated {
	fn blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		event: filen_types::api::v3::socket::FolderSubCreated<'a>,
	) -> Self {
		Self(RemoteDirectory {
			uuid: event.uuid,
			parent: event.parent,
			color: DirColor::Default,
			favorited: event.favorited,
			timestamp: event.timestamp,
			meta: DirectoryMeta::blocking_from_encrypted(event.name, crypter).into_owned_cow(),
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRestore(pub RemoteDirectory);
impl<'a> FolderRestore {
	fn blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		event: filen_types::api::v3::socket::FolderRestore<'a>,
	) -> Self {
		Self(RemoteDirectory {
			uuid: event.uuid,
			parent: event.parent,
			color: DirColor::Default,
			favorited: event.favorited,
			timestamp: event.timestamp,
			meta: DirectoryMeta::blocking_from_encrypted(event.name, crypter).into_owned_cow(),
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessageNew(pub ChatMessage);
impl<'a> ChatMessageNew {
	fn try_blocking_from_rsa_encrypted(
		private_key: &RsaPrivateKey,
		event: filen_types::api::v3::socket::ChatMessageNew<'a>,
	) -> Result<Self, Error> {
		let key = NoteOrChatKeyStruct::blocking_try_decrypt_rsa(private_key, &event.metadata)?;
		let chat_message_encrypted = ChatMessageEncrypted {
			chat: event.chat,
			inner: event.inner,
			reply_to: event.reply_to,
			embed_disabled: event.embed_disabled,
			edited: false,
			edited_timestamp: DateTime::<Utc>::default(),
			sent_timestamp: event.sent_timestamp,
		};
		Ok(Self(ChatMessage::blocking_decrypt(
			chat_message_encrypted,
			Some(&key),
		)))
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatConversationsNew(pub Chat);
impl<'a> ChatConversationsNew {
	fn blocking_from_rsa_encrypted(
		private_key: &RsaPrivateKey,
		user_id: u64,
		event: filen_types::api::v3::socket::ChatConversationsNew<'a>,
	) -> Self {
		let (key, _, _, participants) = crate::chats::blocking_decrypt_chat_parts(
			user_id,
			private_key,
			event.participants,
			None,
			None,
		);

		Self(Chat {
			uuid: event.uuid,
			last_message: None,
			owner_id: event.owner_id,
			key,
			name: None,
			participants,
			muted: false,
			created: event.added_timestamp,
			last_focus: None,
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct NoteContentEdited<'a> {
	pub note: UuidStr,
	pub content: MaybeEncrypted<'a, str>,
	pub note_type: NoteType,
	pub editor_id: u64,
	pub edited_timestamp: DateTime<Utc>,
}

impl<'a> NoteContentEdited<'a> {
	fn try_blocking_from_rsa_encrypted(
		private_key: &RsaPrivateKey,
		event: filen_types::api::v3::socket::NoteContentEdited<'a>,
	) -> Result<Self, Error> {
		let note_key = NoteOrChatKeyStruct::blocking_try_decrypt_rsa(private_key, &event.metadata)?;
		Ok(Self {
			note: event.note,
			note_type: event.note_type,
			editor_id: event.editor_id,
			edited_timestamp: event.edited_timestamp,
			content: match crate::notes::crypto::NoteContent::blocking_try_decrypt(
				&note_key,
				&event.content,
			) {
				Ok(decrypted_content) => MaybeEncrypted::Decrypted(Cow::Owned(decrypted_content)),
				Err(_) => MaybeEncrypted::Encrypted(event.content),
			},
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct NoteTitleEdited<'a> {
	pub note: UuidStr,
	pub new_title: MaybeEncrypted<'a, str>,
}

impl<'a> NoteTitleEdited<'a> {
	fn try_blocking_from_rsa_encrypted(
		private_key: &RsaPrivateKey,
		event: filen_types::api::v3::socket::NoteTitleEdited<'a>,
	) -> Result<Self, Error> {
		let note_key = NoteOrChatKeyStruct::blocking_try_decrypt_rsa(private_key, &event.metadata)
			.context("NoteTitleEdited")?;
		Ok(Self {
			note: event.note,
			new_title: match crate::notes::crypto::NoteTitle::blocking_try_decrypt(
				&note_key,
				&event.title,
			) {
				Ok(decrypted_title) => MaybeEncrypted::Decrypted(Cow::Owned(decrypted_title)),
				Err(_) => MaybeEncrypted::Encrypted(event.title),
			},
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteParticipantNew {
	pub note: UuidStr,
	pub participant: NoteParticipant,
}

impl From<filen_types::api::v3::socket::NoteParticipantNew<'_>> for NoteParticipantNew {
	fn from(value: filen_types::api::v3::socket::NoteParticipantNew<'_>) -> Self {
		Self {
			note: value.note,
			participant: value.participant.into(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteNew {
	pub note: UuidStr,
}
impl From<filen_types::api::v3::socket::NoteNew<'_>> for NoteNew {
	fn from(value: filen_types::api::v3::socket::NoteNew<'_>) -> Self {
		Self { note: value.note }
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct ChatMessageEdited<'a> {
	pub chat: UuidStr,
	pub uuid: UuidStr,
	pub edited_timestamp: DateTime<Utc>,
	pub new_content: MaybeEncrypted<'a, str>,
}
impl<'a> ChatMessageEdited<'a> {
	fn try_blocking_from_rsa_encrypted(
		private_key: &RsaPrivateKey,
		event: filen_types::api::v3::socket::ChatMessageEdited<'a>,
	) -> Result<Self, Error> {
		let chat_key = NoteOrChatKeyStruct::blocking_try_decrypt_rsa(private_key, &event.metadata)?;
		Ok(Self {
			chat: event.chat,
			uuid: event.uuid,
			edited_timestamp: event.edited_timestamp,
			new_content: match crate::chats::crypto::ChatMessage::blocking_try_decrypt(
				&chat_key,
				&event.message,
			) {
				Ok(decrypted_content) => MaybeEncrypted::Decrypted(Cow::Owned(decrypted_content)),
				Err(_) => MaybeEncrypted::Encrypted(event.message),
			},
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct ChatConversationNameEdited<'a> {
	pub chat: UuidStr,
	pub new_name: MaybeEncrypted<'a, str>,
}

impl<'a> ChatConversationNameEdited<'a> {
	fn try_blocking_from_rsa_encrypted(
		private_key: &RsaPrivateKey,
		event: filen_types::api::v3::socket::ChatConversationNameEdited<'a>,
	) -> Result<Self, Error> {
		let chat_key = NoteOrChatKeyStruct::blocking_try_decrypt_rsa(private_key, &event.metadata)
			.context("ChatConversationNameEdited")?;
		Ok(Self {
			chat: event.uuid,
			new_name: match crate::chats::crypto::ChatName::blocking_try_decrypt(
				&chat_key,
				&event.name,
			) {
				Ok(decrypted_name) => MaybeEncrypted::Decrypted(Cow::Owned(decrypted_name)),
				Err(_) => MaybeEncrypted::Encrypted(event.name),
			},
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemFavorite(pub NonRootItemType<'static, Normal>);
impl<'a> ItemFavorite {
	fn try_blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		event: filen_types::api::v3::socket::ItemFavorite<'a>,
	) -> Result<Self, Error> {
		Ok(ItemFavorite(match event.item_type {
			filen_types::fs::ObjectType::File => {
				let size = event.size.ok_or_else(|| {
					Error::custom(ErrorKind::Response, "missing size for file favorite event")
				})?;
				NonRootItemType::File(Cow::Owned(RemoteFile {
					uuid: event.uuid,
					meta: FileMeta::blocking_from_encrypted(
						event.metadata.ok_or_else(|| {
							Error::custom(
								ErrorKind::Response,
								"missing metadata for file favorite event",
							)
						})?,
						crypter,
						event.version.ok_or_else(|| {
							Error::custom(
								ErrorKind::Response,
								"missing version for file favorite event",
							)
						})?,
					)
					.into_owned_cow(),
					parent: event.parent,
					size,
					favorited: event.value,
					region: event
						.region
						.ok_or_else(|| {
							Error::custom(
								ErrorKind::Response,
								"missing region for file favorite event",
							)
						})?
						.into_owned(),
					bucket: event
						.bucket
						.ok_or_else(|| {
							Error::custom(
								ErrorKind::Response,
								"missing bucket for file favorite event",
							)
						})?
						.into_owned(),
					timestamp: event.timestamp,
					chunks: event.chunks.ok_or_else(|| {
						Error::custom(
							ErrorKind::Response,
							"missing chunks for file favorite event",
						)
					})?,
				}))
			}
			filen_types::fs::ObjectType::Dir => NonRootItemType::Dir(Cow::Owned(RemoteDirectory {
				uuid: event.uuid,
				parent: event.parent,
				color: event.color.into_owned_cow(),
				favorited: event.value,
				timestamp: event.timestamp,
				meta: DirectoryMeta::blocking_from_encrypted(
					event.name_encrypted.ok_or_else(|| {
						Error::custom(
							ErrorKind::Response,
							"missing metadata for file favorite event",
						)
					})?,
					crypter,
				)
				.into_owned_cow(),
			})),
		}))
	}
}

#[js_type]
pub struct ChatConversationParticipantNew {
	pub chat: UuidStr,
	pub participant: ChatParticipant,
}

impl<'a> From<filen_types::api::v3::socket::ChatConversationParticipantNew<'a>>
	for ChatConversationParticipantNew
{
	fn from(event: filen_types::api::v3::socket::ChatConversationParticipantNew<'a>) -> Self {
		Self {
			chat: event.chat,
			participant: ChatParticipant {
				user_id: event.user_id,
				email: event.email.into_owned(),
				avatar: event.avatar.map(|a| a.into_owned()),
				nick_name: event.nick_name.into_owned(),
				permissions_add: event.permissions_add,
				added: event.added_timestamp,
				appear_offline: event.appear_offline,
				last_active: event.last_active,
			},
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct FolderMetadataChanged<'a> {
	pub uuid: UuidStr,
	pub meta: DirectoryMeta<'a>,
}

impl<'a> FolderMetadataChanged<'a> {
	fn blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		uuid: UuidStr,
		encrypted_string: EncryptedString<'a>,
	) -> Self {
		Self {
			uuid,
			meta: DirectoryMeta::blocking_from_encrypted(encrypted_string, crypter),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct FileMetadataChanged<'a> {
	pub uuid: UuidStr,
	pub metadata: FileMeta<'a>,
}

impl<'a> FileMetadataChanged<'a> {
	fn blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		uuid: UuidStr,
		new_meta: EncryptedString<'a>,
	) -> Self {
		let metadata =
			FileMeta::blocking_from_encrypted(new_meta, crypter, FileEncryptionVersion::V2);

		Self { uuid, metadata }
	}
}
