use std::{borrow::Cow, ops::Deref};

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::{
		chat::messages::ChatMessageEncrypted,
		dir::color::DirColor,
		notes::NoteType,
		socket::{MessageType, PacketType, SocketEvent},
	},
	auth::FileEncryptionVersion,
	crypto::MaybeEncrypted,
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
	NewEvent, NoteArchived, NoteDeleted, NoteNew, NoteParticipantPermissions,
	NoteParticipantRemoved, NoteRestored,
};

use crate::{
	Error, ErrorKind,
	chats::{Chat, ChatMessage, ChatParticipant},
	consts::CHUNK_SIZE,
	crypto::{
		notes_and_chats::{NoteOrChatCarrierCryptoExt, NoteOrChatKeyStruct},
		shared::MetaCrypter,
	},
	error::ResultExt,
	fs::{
		NonRootFSObject,
		dir::{RemoteDirectory, meta::DirectoryMeta},
		file::{RemoteFile, meta::FileMeta},
	},
	notes::NoteParticipant,
	runtime,
};

use super::consts::{AUTHED_TRUE, VERSIONED_EVENT_PREFIXES};

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

		log::info!("Received WebSocket event: {}", event_str);

		if event_str == AUTHED_TRUE {
			// ignore authed true messages
			return Ok(None);
		}

		// these are duplicates of FileArchived, so we can just ignore them
		if VERSIONED_EVENT_PREFIXES
			.iter()
			.any(|prefix| event_str.starts_with(prefix))
		{
			// ignore versioned events for now
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
	/// Sent after successful authentication, including on reconnect
	AuthSuccess, // tested
	/// Sent after failed authentication, including on reconnect, after which the socket is closed and all listeners removed
	AuthFailed, // tested
	/// Sent when the socket has unexpectedly closed and begins attempting to reconnect
	Reconnecting,
	/// Sent when the handle to the event listener has been dropped and the listener is removed
	Unsubscribed, // tested
	NewEvent(NewEvent<'a>),                     // unused by rust, legacy
	FileRename(FileRename<'a>),                 // rust never uses this, so no way to test it
	FileArchiveRestored(FileArchiveRestored),   // not sure what this is for
	FileNew(FileNew),                           // tested, needs size added
	FileRestore(FileRestore),                   // tested, needs size added
	FileMove(FileMove),                         // tested, needs size added
	FileTrash(FileTrash), // tested, might want to add enough info to build a RemoteFile here
	FileArchived(FileArchived), // tested
	FolderRename(FolderRename<'a>), // rust never uses this, so no way to test it
	FolderTrash(FolderTrash), // tested, might want to add enough info to build a RemoteFolder here
	FolderMove(FolderMove), // tested, needs color added
	FolderSubCreated(FolderSubCreated), // tested, needs color added
	FolderRestore(FolderRestore), // tested, needs color added
	FolderColorChanged(FolderColorChanged<'a>), // tested
	TrashEmpty,
	PasswordChanged,
	ChatMessageNew(ChatMessageNew), // tested
	ChatTyping(ChatTyping<'a>),
	ChatConversationsNew(ChatConversationsNew),
	ChatMessageDelete(ChatMessageDelete),
	NoteContentEdited(NoteContentEdited<'a>),
	NoteArchived(NoteArchived),
	NoteDeleted(NoteDeleted),
	NoteTitleEdited(NoteTitleEdited<'a>),
	NoteParticipantPermissions(NoteParticipantPermissions),
	NoteRestored(NoteRestored),
	NoteParticipantRemoved(NoteParticipantRemoved),
	NoteParticipantNew(NoteParticipantNew),
	NoteNew(NoteNew),
	ChatMessageEmbedDisabled(ChatMessageEmbedDisabled),
	ChatConversationParticipantLeft(ChatConversationParticipantLeft),
	ChatConversationDeleted(ChatConversationDeleted),
	ChatMessageEdited(ChatMessageEdited<'a>),
	ChatConversationNameEdited(ChatConversationNameEdited<'a>),
	ContactRequestReceived(ContactRequestReceived<'a>),
	ItemFavorite(ItemFavorite),
	ChatConversationParticipantNew(ChatConversationParticipantNew),
	FileDeletedPermanent(FileDeletedPermanent), // tested
	FolderMetadataChanged(FolderMetadataChanged<'a>), // tested
	FolderDeletedPermanent(FolderDeletedPermanent), // tested
	FileMetadataChanged(FileMetadataChanged<'a>), // tested
}

impl DecryptedSocketEvent<'_> {
	pub fn event_type(&self) -> &'static str {
		match self {
			DecryptedSocketEvent::AuthSuccess => "authSuccess",
			DecryptedSocketEvent::AuthFailed => "authFailed",
			DecryptedSocketEvent::Reconnecting => "reconnecting",
			DecryptedSocketEvent::Unsubscribed => "unsubscribed",
			DecryptedSocketEvent::NewEvent(_) => "newEvent",
			DecryptedSocketEvent::FileRename(_) => "fileRename",
			DecryptedSocketEvent::FileArchiveRestored(_) => "fileArchiveRestored",
			DecryptedSocketEvent::FileNew(_) => "fileNew",
			DecryptedSocketEvent::FileRestore(_) => "fileRestore",
			DecryptedSocketEvent::FileMove(_) => "fileMove",
			DecryptedSocketEvent::FileTrash(_) => "fileTrash",
			DecryptedSocketEvent::FileArchived(_) => "fileArchived",
			DecryptedSocketEvent::FolderRename(_) => "folderRename",
			DecryptedSocketEvent::FolderTrash(_) => "folderTrash",
			DecryptedSocketEvent::FolderMove(_) => "folderMove",
			DecryptedSocketEvent::FolderSubCreated(_) => "folderSubCreated",
			DecryptedSocketEvent::FolderRestore(_) => "folderRestore",
			DecryptedSocketEvent::FolderColorChanged(_) => "folderColorChanged",
			DecryptedSocketEvent::TrashEmpty => "trashEmpty",
			DecryptedSocketEvent::PasswordChanged => "passwordChanged",
			DecryptedSocketEvent::ChatMessageNew(_) => "chatMessageNew",
			DecryptedSocketEvent::ChatTyping(_) => "chatTyping",
			DecryptedSocketEvent::ChatConversationsNew(_) => "chatConversationsNew",
			DecryptedSocketEvent::ChatMessageDelete(_) => "chatMessageDelete",
			DecryptedSocketEvent::NoteContentEdited(_) => "noteContentEdited",
			DecryptedSocketEvent::NoteArchived(_) => "noteArchived",
			DecryptedSocketEvent::NoteDeleted(_) => "noteDeleted",
			DecryptedSocketEvent::NoteTitleEdited(_) => "noteTitleEdited",
			DecryptedSocketEvent::NoteParticipantPermissions(_) => "noteParticipantPermissions",
			DecryptedSocketEvent::NoteRestored(_) => "noteRestored",
			DecryptedSocketEvent::NoteParticipantRemoved(_) => "noteParticipantRemoved",
			DecryptedSocketEvent::NoteParticipantNew(_) => "noteParticipantNew",
			DecryptedSocketEvent::NoteNew(_) => "noteNew",
			DecryptedSocketEvent::ChatMessageEmbedDisabled(_) => "chatMessageEmbedDisabled",
			DecryptedSocketEvent::ChatConversationParticipantLeft(_) => {
				"chatConversationParticipantLeft"
			}
			DecryptedSocketEvent::ChatConversationDeleted(_) => "chatConversationDeleted",
			DecryptedSocketEvent::ChatMessageEdited(_) => "chatMessageEdited",
			DecryptedSocketEvent::ChatConversationNameEdited(_) => "chatConversationNameEdited",
			DecryptedSocketEvent::ContactRequestReceived(_) => "contactRequestReceived",
			DecryptedSocketEvent::ItemFavorite(_) => "itemFavorite",
			DecryptedSocketEvent::ChatConversationParticipantNew(_) => {
				"chatConversationParticipantNew"
			}
			DecryptedSocketEvent::FileDeletedPermanent(_) => "fileDeletedPermanent",
			DecryptedSocketEvent::FolderMetadataChanged(_) => "folderMetadataChanged",
			DecryptedSocketEvent::FolderDeletedPermanent(_) => "folderDeletedPermanent",
			DecryptedSocketEvent::FileMetadataChanged(_) => "fileMetadataChanged",
		}
	}

	pub(crate) async fn try_from_encrypted<'a>(
		crypter: &impl MetaCrypter,
		private_key: &RsaPrivateKey,
		user_id: u64,
		event: SocketEvent<'a>,
	) -> Result<DecryptedSocketEvent<'a>, Error> {
		Ok(match event {
			SocketEvent::NewEvent(e) => DecryptedSocketEvent::NewEvent(e),
			SocketEvent::FileRename(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::FileRename(FileRename::blocking_from_encrypted(
						crypter, e,
					))
				})
				.await
			}
			SocketEvent::FileArchiveRestored(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::FileArchiveRestored(
						FileArchiveRestored::blocking_from_encrypted(crypter, e),
					)
				})
				.await
			}
			SocketEvent::FileNew(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::FileNew(FileNew::blocking_from_encrypted(crypter, e))
				})
				.await
			}
			SocketEvent::FileRestore(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::FileRestore(FileRestore::blocking_from_encrypted(
						crypter, e,
					))
				})
				.await
			}
			SocketEvent::FileMove(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::FileMove(FileMove::blocking_from_encrypted(crypter, e))
				})
				.await
			}
			SocketEvent::FileTrash(e) => DecryptedSocketEvent::FileTrash(e),
			SocketEvent::FileArchived(e) => DecryptedSocketEvent::FileArchived(e),
			SocketEvent::FolderRename(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::FolderRename(FolderRename::blocking_from_encrypted(
						crypter, e,
					))
				})
				.await
			}
			SocketEvent::FolderTrash(e) => DecryptedSocketEvent::FolderTrash(e),
			SocketEvent::FolderMove(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::FolderMove(FolderMove::blocking_from_encrypted(
						crypter, e,
					))
				})
				.await
			}
			SocketEvent::FolderSubCreated(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::FolderSubCreated(
						FolderSubCreated::blocking_from_encrypted(crypter, e),
					)
				})
				.await
			}
			SocketEvent::FolderRestore(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::FolderRestore(FolderRestore::blocking_from_encrypted(
						crypter, e,
					))
				})
				.await
			}
			SocketEvent::FolderColorChanged(e) => DecryptedSocketEvent::FolderColorChanged(e),
			SocketEvent::TrashEmpty => DecryptedSocketEvent::TrashEmpty,
			SocketEvent::PasswordChanged => DecryptedSocketEvent::PasswordChanged,
			SocketEvent::ChatMessageNew(e) => {
				runtime::do_cpu_intensive(|| {
					Ok::<_, Error>(DecryptedSocketEvent::ChatMessageNew(
						ChatMessageNew::try_blocking_from_rsa_encrypted(private_key, e)?,
					))
				})
				.await?
			}
			SocketEvent::ChatTyping(e) => DecryptedSocketEvent::ChatTyping(e),
			SocketEvent::ChatConversationsNew(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::ChatConversationsNew(
						ChatConversationsNew::blocking_from_rsa_encrypted(private_key, user_id, e),
					)
				})
				.await
			}
			SocketEvent::ChatMessageDelete(e) => DecryptedSocketEvent::ChatMessageDelete(e),
			SocketEvent::NoteContentEdited(e) => {
				runtime::do_cpu_intensive(|| {
					Ok::<_, Error>(DecryptedSocketEvent::NoteContentEdited(
						NoteContentEdited::try_blocking_from_rsa_encrypted(private_key, e)?,
					))
				})
				.await?
			}
			SocketEvent::NoteArchived(e) => DecryptedSocketEvent::NoteArchived(e),
			SocketEvent::NoteDeleted(e) => DecryptedSocketEvent::NoteDeleted(e),
			SocketEvent::NoteTitleEdited(e) => {
				runtime::do_cpu_intensive(|| {
					Ok::<_, Error>(DecryptedSocketEvent::NoteTitleEdited(
						NoteTitleEdited::try_blocking_from_rsa_encrypted(private_key, e)?,
					))
				})
				.await?
			}
			SocketEvent::NoteParticipantPermissions(e) => {
				DecryptedSocketEvent::NoteParticipantPermissions(e)
			}
			SocketEvent::NoteRestored(e) => DecryptedSocketEvent::NoteRestored(e),
			SocketEvent::NoteParticipantRemoved(e) => {
				DecryptedSocketEvent::NoteParticipantRemoved(e)
			}
			SocketEvent::NoteParticipantNew(e) => {
				DecryptedSocketEvent::NoteParticipantNew(e.into())
			}
			SocketEvent::NoteNew(e) => DecryptedSocketEvent::NoteNew(e),
			SocketEvent::ChatMessageEmbedDisabled(e) => {
				DecryptedSocketEvent::ChatMessageEmbedDisabled(e)
			}
			SocketEvent::ChatConversationParticipantLeft(e) => {
				DecryptedSocketEvent::ChatConversationParticipantLeft(e)
			}
			SocketEvent::ChatConversationDeleted(e) => {
				DecryptedSocketEvent::ChatConversationDeleted(e)
			}
			SocketEvent::ChatMessageEdited(e) => {
				runtime::do_cpu_intensive(|| {
					Ok::<_, Error>(DecryptedSocketEvent::ChatMessageEdited(
						ChatMessageEdited::try_blocking_from_rsa_encrypted(private_key, e)?,
					))
				})
				.await?
			}
			SocketEvent::ChatConversationNameEdited(e) => {
				runtime::do_cpu_intensive(|| {
					Ok::<_, Error>(DecryptedSocketEvent::ChatConversationNameEdited(
						ChatConversationNameEdited::try_blocking_from_rsa_encrypted(
							private_key,
							e,
						)?,
					))
				})
				.await?
			}
			SocketEvent::ContactRequestReceived(e) => {
				DecryptedSocketEvent::ContactRequestReceived(e)
			}
			SocketEvent::ItemFavorite(e) => {
				runtime::do_cpu_intensive(|| {
					Ok::<_, Error>(DecryptedSocketEvent::ItemFavorite(
						ItemFavorite::try_blocking_from_encrypted(crypter, e)?,
					))
				})
				.await?
			}
			SocketEvent::ChatConversationParticipantNew(e) => {
				DecryptedSocketEvent::ChatConversationParticipantNew(e.into())
			}
			SocketEvent::FileDeletedPermanent(e) => DecryptedSocketEvent::FileDeletedPermanent(e),
			SocketEvent::FolderMetadataChanged(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::FolderMetadataChanged(
						FolderMetadataChanged::blocking_from_encrypted(crypter, e),
					)
				})
				.await
			}
			SocketEvent::FolderDeletedPermanent(e) => {
				DecryptedSocketEvent::FolderDeletedPermanent(e)
			}
			SocketEvent::FileMetadataChanged(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::FileMetadataChanged(
						FileMetadataChanged::blocking_from_encrypted(crypter, e),
					)
				})
				.await
			}
		})
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

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct FolderRename<'a> {
	pub name: MaybeEncrypted<'a>,
	pub uuid: UuidStr,
}

impl<'a> FolderRename<'a> {
	fn blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		event: filen_types::api::v3::socket::FolderRename<'a>,
	) -> Self {
		Self {
			uuid: event.uuid,
			name: match crypter.blocking_decrypt_meta(&event.name) {
				Ok(decrypted_name) => MaybeEncrypted::Decrypted(Cow::Owned(decrypted_name)),
				Err(_) => MaybeEncrypted::Encrypted(event.name),
			},
		}
	}
}

// todo test meta vs name FolderMove
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
	pub content: MaybeEncrypted<'a>,
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
	pub new_title: MaybeEncrypted<'a>,
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

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct ChatMessageEdited<'a> {
	pub chat: UuidStr,
	pub uuid: UuidStr,
	pub edited_timestamp: DateTime<Utc>,
	pub new_content: MaybeEncrypted<'a>,
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
	pub new_name: MaybeEncrypted<'a>,
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
pub struct ItemFavorite(pub NonRootFSObject<'static>);
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
				NonRootFSObject::File(Cow::Owned(RemoteFile {
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
			filen_types::fs::ObjectType::Dir => NonRootFSObject::Dir(Cow::Owned(RemoteDirectory {
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

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify, serde::Serialize),
	tsify(large_number_types_as_bigints)
)]
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
		event: filen_types::api::v3::socket::FolderMetadataChanged<'a>,
	) -> Self {
		Self {
			uuid: event.uuid,
			meta: DirectoryMeta::blocking_from_encrypted(event.meta, crypter),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct FileMetadataChanged<'a> {
	pub uuid: UuidStr,
	pub name: MaybeEncrypted<'a>,
	pub metadata: FileMeta<'a>,
	pub old_metadata: FileMeta<'a>,
}

impl<'a> FileMetadataChanged<'a> {
	fn blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		event: filen_types::api::v3::socket::FileMetadataChanged<'a>,
	) -> Self {
		let (name, metadata, old_metadata) = runtime::blocking_join!(
			|| {
				match crypter.blocking_decrypt_meta(&event.name) {
					Ok(decrypted_name) => MaybeEncrypted::Decrypted(Cow::Owned(decrypted_name)),
					Err(_) => MaybeEncrypted::Encrypted(event.name),
				}
			},
			|| {
				FileMeta::blocking_from_encrypted(
					event.metadata,
					crypter,
					FileEncryptionVersion::V2,
				)
			},
			|| {
				FileMeta::blocking_from_encrypted(
					event.old_metadata,
					crypter,
					FileEncryptionVersion::V2,
				)
			}
		);

		Self {
			uuid: event.uuid,
			name,
			metadata,
			old_metadata,
		}
	}
}
