use std::{borrow::Cow, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::{
		chat::messages::ChatMessageEncrypted,
		dir::color::DirColor,
		socket::{MessageType, PacketType, SocketEvent},
	},
	auth::FileEncryptionVersion,
	crypto::MaybeEncrypted,
	fs::UuidStr,
	traits::CowHelpers,
};
use rsa::RsaPrivateKey;
use yoke::Yokeable;

pub use filen_types::api::v3::socket::{
	ChatConversationDeleted, ChatConversationParticipantLeft, ChatMessageDelete,
	ChatMessageEmbedDisabled, ChatTyping, ContactRequestReceived, FileArchived,
	FileDeletedPermanent, FileTrash, FolderColorChanged, FolderDeletedPermanent, FolderTrash,
	NewEvent, NoteArchived, NoteDeleted, NoteNew, NoteParticipantNew, NoteParticipantPermissions,
	NoteParticipantRemoved, NoteRestored,
};

use crate::{
	Error, ErrorKind,
	auth::http::AuthClient,
	chats::{ChatMessage, ChatParticipant},
	consts::CHUNK_SIZE,
	crypto::{
		notes_and_chats::{NoteOrChatCarrierCryptoExt, NoteOrChatKeyStruct},
		shared::MetaCrypter,
	},
	fs::{
		NonRootFSObject,
		dir::{RemoteDirectory, meta::DirectoryMeta},
		file::{RemoteFile, meta::FileMeta},
	},
	runtime,
};

pub type EventListenerCallback = Box<dyn Fn(&DecryptedSocketEvent<'_>) + Send + 'static>;

pub(super) struct WebSocketConfig {
	pub(super) client: Arc<AuthClient>,
	pub(super) reconnect_delay: Duration,
	pub(super) max_reconnect_delay: Duration,
	pub(super) ping_interval: Duration,
}

pub(super) const MESSAGE_EVENT_PAYLOAD: &str =
	match str::from_utf8(&[PacketType::Message as u8, MessageType::Event as u8]) {
		Ok(s) => s,
		Err(_) => panic!("Failed to create handshake payload string"),
	};

pub(super) const MESSAGE_CONNECT_PAYLOAD: &str =
	match str::from_utf8(&[PacketType::Message as u8, MessageType::Connect as u8]) {
		Ok(s) => s,
		Err(_) => panic!("Failed to create handshake payload string"),
	};

pub(super) const PING_MESSAGE: &str = match str::from_utf8(&[PacketType::Ping as u8]) {
	Ok(s) => s,
	Err(_) => panic!("Failed to create ping message string"),
};

pub(super) const RECONNECT_DELAY: Duration = Duration::from_secs(1);
pub(super) const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
pub(super) const PING_INTERVAL: Duration = Duration::from_secs(15);

pub(super) const WEBSOCKET_URL_CORE: &str =
	"wss://socket.filen.io/socket.io/?EIO=3&transport=websocket&t=";

pub(super) const AUTHED_TRUE: &str = r#"["authed",true]"#;
pub(super) const VERSIONED_EVENT_PREFIXES: &[&str] =
	&[r#"["file-versioned","#, r#"["fileVersioned","#];

mod listener_manager {
	use std::{
		borrow::Cow,
		collections::HashMap,
		hash::{BuildHasherDefault, Hasher},
	};

	use filen_types::api::v3::socket::SocketEvent;

	use crate::{Error, socket::shared::DecryptedSocketEvent};

	use super::EventListenerCallback;

	#[derive(Default)]
	struct IdentityHasher(u64);

	impl Hasher for IdentityHasher {
		fn write(&mut self, _: &[u8]) {
			unreachable!("IdentityHasher only supports u64")
		}

		fn write_u64(&mut self, i: u64) {
			self.0 = i;
		}

		fn finish(&self) -> u64 {
			self.0
		}
	}

	type U64Map<V> = HashMap<u64, V, BuildHasherDefault<IdentityHasher>>;

	trait ListenerManager {
		fn callbacks(&self) -> &U64Map<EventListenerCallback>;
		fn callbacks_mut(&mut self) -> &mut U64Map<EventListenerCallback>;
		fn callbacks_for_event(&self) -> &HashMap<String, Vec<u64>>;
		fn callbacks_for_event_mut(&mut self) -> &mut HashMap<String, Vec<u64>>;
		fn global_callbacks(&self) -> &Vec<u64>;
		fn global_callbacks_mut(&mut self) -> &mut Vec<u64>;
		fn last_id(&mut self) -> &mut u64;
	}

	trait ListenerManagerExtInner: ListenerManager {
		fn broadcast_event(&self, event: &DecryptedSocketEvent<'_>) {
			if let Some(callback_ids) = self.callbacks_for_event().get(event.event_type()) {
				for &callback_id in callback_ids {
					if let Some(callback) = self.callbacks().get(&callback_id) {
						callback(event);
					}
				}
			}
			for &callback_id in self.global_callbacks() {
				if let Some(callback) = self.callbacks().get(&callback_id) {
					callback(event);
				}
			}
		}

		fn add_listener<'a>(
			&mut self,
			callback: EventListenerCallback,
			event_types: Option<impl Iterator<Item = Cow<'a, str>>>,
		) -> u64 {
			let this_id = *self.last_id();
			self.callbacks_mut().insert(this_id, callback);
			*self.last_id() += 1;

			if let Some(event_types) = event_types {
				for event_type in event_types {
					if let Some(value) = self.callbacks_for_event_mut().get_mut(event_type.as_ref())
					{
						// event type is already present
						value.push(this_id);
						continue;
					} else {
						// new event type
						self.callbacks_for_event_mut()
							.insert(event_type.into_owned(), vec![this_id]);
					}
				}
			} else {
				self.global_callbacks_mut().push(this_id);
			}

			this_id
		}

		fn remove_listener(&mut self, id: u64) -> Option<EventListenerCallback> {
			let callback = self.callbacks_mut().remove(&id);
			let mut empty_event_types = Vec::new();
			for (event_type, callbacks) in self.callbacks_for_event_mut().iter_mut() {
				callbacks.retain(|&callback_id| callback_id != id);
				if callbacks.is_empty() {
					empty_event_types.push(event_type.clone());
				}
			}
			for event_type in empty_event_types {
				self.callbacks_for_event_mut().remove(&event_type);
			}
			self.global_callbacks_mut()
				.retain(|&callback_id| callback_id != id);

			callback
		}
	}

	impl<T> ListenerManagerExtInner for T where T: ListenerManager {}

	// don't want it to be implemented outside of this module
	#[allow(private_bounds)]
	pub(crate) trait ListenerManagerExt: ListenerManager {
		fn is_empty(&self) -> bool {
			self.callbacks().is_empty()
		}

		fn add_listener<'a>(
			&mut self,
			callback: EventListenerCallback,
			cancel_receiver: tokio::sync::oneshot::Receiver<()>,
			id_sender: tokio::sync::oneshot::Sender<Result<u64, Error>>,
			event_types: Option<impl Iterator<Item = Cow<'a, str>>>,
		);

		fn remove_listener(&mut self, id: u64);
	}

	struct NewIdStruct {
		id: u64,
		id_sender: tokio::sync::oneshot::Sender<Result<u64, Error>>,
		cancel_receiver: tokio::sync::oneshot::Receiver<()>,
	}

	pub(crate) struct DisconnectedListenerManager {
		callbacks: U64Map<EventListenerCallback>,
		callbacks_for_event: HashMap<String, Vec<u64>>,
		global_callbacks: Vec<u64>,
		last_id: u64,
		new_ids: Vec<NewIdStruct>,
	}

	impl ListenerManager for DisconnectedListenerManager {
		fn callbacks(&self) -> &U64Map<EventListenerCallback> {
			&self.callbacks
		}

		fn callbacks_mut(&mut self) -> &mut U64Map<EventListenerCallback> {
			&mut self.callbacks
		}

		fn callbacks_for_event(&self) -> &HashMap<String, Vec<u64>> {
			&self.callbacks_for_event
		}

		fn callbacks_for_event_mut(&mut self) -> &mut HashMap<String, Vec<u64>> {
			&mut self.callbacks_for_event
		}

		fn global_callbacks(&self) -> &Vec<u64> {
			&self.global_callbacks
		}

		fn global_callbacks_mut(&mut self) -> &mut Vec<u64> {
			&mut self.global_callbacks
		}

		fn last_id(&mut self) -> &mut u64 {
			&mut self.last_id
		}
	}

	impl DisconnectedListenerManager {
		pub(crate) fn new() -> Self {
			Self {
				callbacks: U64Map::default(),
				callbacks_for_event: HashMap::new(),
				global_callbacks: Vec::new(),
				last_id: 0,
				new_ids: Vec::new(),
			}
		}

		pub(crate) fn broadcast_auth_failed(&mut self) {
			ListenerManagerExtInner::broadcast_event(self, &DecryptedSocketEvent::AuthFailed);
			for NewIdStruct {
				id_sender: sender, ..
			} in self.new_ids.drain(..)
			{
				let _ = sender.send(Err(Error::custom(
					crate::error::ErrorKind::Unauthenticated,
					"socket authentication failed",
				)));
			}
		}

		pub(crate) fn into_connected(mut self) -> ConnectedListenerManager {
			let mut ids_to_remove = Vec::new();

			// we drain here so we can call ListenerManagerExtInner::remove_listener
			for NewIdStruct {
				id,
				id_sender: sender,
				mut cancel_receiver,
			} in self.new_ids.drain(..)
			{
				if sender.send(Ok(id)).is_err() {
					ids_to_remove.push(id);
				} else if let Ok(()) = cancel_receiver.try_recv() {
					ids_to_remove.push(id);
				}
			}

			for id in ids_to_remove {
				ListenerManagerExtInner::remove_listener(&mut self, id);
			}

			let DisconnectedListenerManager {
				callbacks,
				callbacks_for_event,
				global_callbacks,
				last_id,
				..
			} = self;

			let new = ConnectedListenerManager {
				callbacks,
				callbacks_for_event,
				global_callbacks,
				last_id,
			};

			new.broadcast_event(&DecryptedSocketEvent::AuthSuccess);
			new
		}
	}

	impl ListenerManagerExt for DisconnectedListenerManager {
		fn add_listener<'a>(
			&mut self,
			callback: EventListenerCallback,
			cancel_receiver: tokio::sync::oneshot::Receiver<()>,
			id_sender: tokio::sync::oneshot::Sender<Result<u64, Error>>,
			event_types: Option<impl Iterator<Item = Cow<'a, str>>>,
		) {
			let id = ListenerManagerExtInner::add_listener(self, callback, event_types);
			self.new_ids.push(NewIdStruct {
				id,
				id_sender,
				cancel_receiver,
			});
		}

		fn remove_listener(&mut self, id: u64) {
			let callback = ListenerManagerExtInner::remove_listener(self, id);
			if let Some(callback) = callback {
				callback(&DecryptedSocketEvent::Unsubscribed);
			}
			self.new_ids.retain(|new_id_struct| new_id_struct.id != id);
		}
	}

	pub(crate) struct ConnectedListenerManager {
		callbacks: U64Map<EventListenerCallback>,
		callbacks_for_event: HashMap<String, Vec<u64>>,
		global_callbacks: Vec<u64>,
		last_id: u64,
	}

	impl ListenerManager for ConnectedListenerManager {
		fn callbacks(&self) -> &U64Map<EventListenerCallback> {
			&self.callbacks
		}

		fn callbacks_mut(&mut self) -> &mut U64Map<EventListenerCallback> {
			&mut self.callbacks
		}

		fn callbacks_for_event(&self) -> &HashMap<String, Vec<u64>> {
			&self.callbacks_for_event
		}

		fn callbacks_for_event_mut(&mut self) -> &mut HashMap<String, Vec<u64>> {
			&mut self.callbacks_for_event
		}

		fn global_callbacks(&self) -> &Vec<u64> {
			&self.global_callbacks
		}

		fn global_callbacks_mut(&mut self) -> &mut Vec<u64> {
			&mut self.global_callbacks
		}

		fn last_id(&mut self) -> &mut u64 {
			&mut self.last_id
		}
	}

	impl ConnectedListenerManager {
		pub(crate) fn broadcast_event(&self, event: &DecryptedSocketEvent<'_>) {
			ListenerManagerExtInner::broadcast_event(self, event);
		}

		pub(crate) fn into_disconnected(self) -> DisconnectedListenerManager {
			self.broadcast_event(&DecryptedSocketEvent::Reconnecting);
			let ConnectedListenerManager {
				callbacks,
				callbacks_for_event,
				global_callbacks,
				last_id,
			} = self;

			DisconnectedListenerManager {
				callbacks,
				callbacks_for_event,
				global_callbacks,
				last_id,
				new_ids: Vec::new(),
			}
		}

		pub(crate) fn should_decrypt_event(&self, event: &SocketEvent<'_>) -> bool {
			!self.global_callbacks().is_empty()
				|| self.callbacks_for_event().contains_key(event.event_type())
		}
	}

	impl ListenerManagerExt for ConnectedListenerManager {
		fn add_listener<'a>(
			&mut self,
			callback: EventListenerCallback,
			mut cancel_receiver: tokio::sync::oneshot::Receiver<()>,
			id_sender: tokio::sync::oneshot::Sender<Result<u64, Error>>,
			event_types: Option<impl Iterator<Item = Cow<'a, str>>>,
		) {
			let id = ListenerManagerExtInner::add_listener(self, callback, event_types);

			if id_sender.send(Ok(id)).is_err() {
				ListenerManagerExtInner::remove_listener(self, id);
			} else if let Ok(()) = cancel_receiver.try_recv() {
				ListenerManagerExtInner::remove_listener(self, id);
			}
		}

		fn remove_listener(&mut self, id: u64) {
			let callback = ListenerManagerExtInner::remove_listener(self, id);
			if let Some(callback) = callback {
				callback(&DecryptedSocketEvent::Unsubscribed);
			}
		}
	}
}

pub(super) use listener_manager::{
	ConnectedListenerManager, DisconnectedListenerManager, ListenerManagerExt,
};

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
	NewEvent(NewEvent<'a>),
	FileRename(FileRename<'a>), // rust never uses this, so no way to test it
	FileArchiveRestored(FileArchiveRestored), // not sure what this is for
	FileNew(FileNew),           // tested, needs size added
	FileRestore(FileRestore),   // tested, needs size added
	FileMove(FileMove),         // tested, needs size added
	FileTrash(FileTrash),       // tested, might want to add enough info to build a RemoteFile here
	FileArchived(FileArchived), // untested, not sure what this is for
	FolderRename(FolderRename<'a>), // rust never uses this, so no way to test it
	FolderTrash(FolderTrash),   // tested, might want to add enough info to build a RemoteFolder here
	FolderMove(FolderMove),     // tested, needs color added
	FolderSubCreated(FolderSubCreated), // tested, needs color added
	FolderRestore(FolderRestore), // tested, needs color added
	FolderColorChanged(FolderColorChanged<'a>), // tested
	TrashEmpty,
	PasswordChanged,
	ChatMessageNew(ChatMessageNew),
	ChatTyping(ChatTyping<'a>),
	ChatConversationsNew(ChatConversationsNew),
	ChatMessageDelete(ChatMessageDelete),
	NoteContentEdited(NoteContentEdited),
	NoteArchived(NoteArchived),
	NoteDeleted(NoteDeleted),
	NoteTitleEdited(NoteTitleEdited),
	NoteParticipantPermissions(NoteParticipantPermissions),
	NoteRestored(NoteRestored),
	NoteParticipantRemoved(NoteParticipantRemoved),
	NoteParticipantNew(NoteParticipantNew<'a>),
	NoteNew(NoteNew),
	ChatMessageEmbedDisabled(ChatMessageEmbedDisabled),
	ChatConversationParticipantLeft(ChatConversationParticipantLeft),
	ChatConversationDeleted(ChatConversationDeleted),
	ChatMessageEdited(ChatMessageEdited<'a>),
	ChatConversationNameEdited(ChatConversationNameEdited),
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
		event: SocketEvent<'a>,
	) -> Result<DecryptedSocketEvent<'a>, Error> {
		Ok(match event {
			SocketEvent::AuthSuccess => DecryptedSocketEvent::AuthSuccess,
			SocketEvent::AuthFailed => DecryptedSocketEvent::AuthFailed,
			SocketEvent::Reconnecting => DecryptedSocketEvent::Reconnecting,
			SocketEvent::Unsubscribed => DecryptedSocketEvent::Unsubscribed,
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
						ChatConversationsNew::blocking_from_encrypted(crypter, e),
					)
				})
				.await
			}
			SocketEvent::ChatMessageDelete(e) => DecryptedSocketEvent::ChatMessageDelete(e),
			SocketEvent::NoteContentEdited(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::NoteContentEdited(
						NoteContentEdited::blocking_from_encrypted(crypter, e),
					)
				})
				.await
			}
			SocketEvent::NoteArchived(e) => DecryptedSocketEvent::NoteArchived(e),
			SocketEvent::NoteDeleted(e) => DecryptedSocketEvent::NoteDeleted(e),
			SocketEvent::NoteTitleEdited(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::NoteTitleEdited(NoteTitleEdited::blocking_from_encrypted(
						crypter, e,
					))
				})
				.await
			}
			SocketEvent::NoteParticipantPermissions(e) => {
				DecryptedSocketEvent::NoteParticipantPermissions(e)
			}
			SocketEvent::NoteRestored(e) => DecryptedSocketEvent::NoteRestored(e),
			SocketEvent::NoteParticipantRemoved(e) => {
				DecryptedSocketEvent::NoteParticipantRemoved(e)
			}
			SocketEvent::NoteParticipantNew(e) => DecryptedSocketEvent::NoteParticipantNew(e),
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
					DecryptedSocketEvent::ChatConversationNameEdited(
						ChatConversationNameEdited::blocking_from_encrypted(crypter, e),
					)
				})
				.await
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
				size: 0, // TODO fix
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
			size: 0, // TODO fix
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
			size: 0, // TODO fix
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
			size: 0, // TODO fix
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
pub struct ChatConversationsNew;
impl<'a> ChatConversationsNew {
	fn blocking_from_encrypted(
		_crypter: &impl MetaCrypter,
		_event: filen_types::api::v3::socket::ChatConversationsNew<'a>,
	) -> Self {
		Self
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteContentEdited;
impl<'a> NoteContentEdited {
	fn blocking_from_encrypted(
		_crypter: &impl MetaCrypter,
		_event: filen_types::api::v3::socket::NoteContentEdited<'a>,
	) -> Self {
		Self
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteTitleEdited;
impl<'a> NoteTitleEdited {
	fn blocking_from_encrypted(
		_crypter: &impl MetaCrypter,
		_event: filen_types::api::v3::socket::NoteTitleEdited<'a>,
	) -> Self {
		Self
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatConversationNameEdited;
impl<'a> ChatConversationNameEdited {
	fn blocking_from_encrypted(
		_crypter: &impl MetaCrypter,
		_event: filen_types::api::v3::socket::ChatConversationNameEdited<'a>,
	) -> Self {
		Self
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
					// todo use actual chunk count once this is returned from backend
					chunks: if size == 0 {
						0
					} else {
						size / CHUNK_SIZE as u64 + 1
					},
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
