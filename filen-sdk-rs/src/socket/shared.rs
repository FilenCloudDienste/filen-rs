use std::{borrow::Cow, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::{
		dir::color::DirColor,
		socket::{MessageType, PacketType, SocketEvent},
	},
	auth::FileEncryptionVersion,
	crypto::MaybeEncrypted,
	fs::UuidStr,
	traits::CowHelpers,
};
use yoke::Yokeable;

pub use filen_types::api::v3::socket::{
	ChatConversationDeleted, ChatConversationParticipantLeft, ChatMessageDelete,
	ChatMessageEmbedDisabled, ChatTyping, ContactRequestReceived, FileArchived,
	FileDeletedPermanent, FileTrash, FolderColorChanged, FolderDeletedPermanent, FolderTrash,
	NewEvent, NoteArchived, NoteDeleted, NoteNew, NoteParticipantNew, NoteParticipantPermissions,
	NoteParticipantRemoved, NoteRestored,
};

use crate::{
	auth::http::AuthClient,
	crypto::shared::MetaCrypter,
	fs::{
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
	AuthSuccess,
	/// Sent after failed authentication, including on reconnect, after which the socket is closed and all listeners removed
	AuthFailed,
	/// Sent when the socket has unexpectedly closed and begins attempting to reconnect
	Reconnecting,
	/// Sent when the handle to the event listener has been dropped and the listener is removed
	Unsubscribed,
	NewEvent(NewEvent<'a>),
	FileRename(FileRename<'a>),
	FileArchiveRestored(FileArchiveRestored),
	FileNew(FileNew),
	FileRestore(FileRestore),
	FileMove(FileMove),
	FileTrash(FileTrash),
	FileArchived(FileArchived),
	FolderRename(FolderRename<'a>),
	FolderTrash(FolderTrash),
	FolderMove(FolderMove),
	FolderSubCreated(FolderSubCreated),
	FolderRestore(FolderRestore),
	FolderColorChanged(FolderColorChanged<'a>),
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
	ChatMessageEdited(ChatMessageEdited),
	ChatConversationNameEdited(ChatConversationNameEdited),
	ContactRequestReceived(ContactRequestReceived<'a>),
	ItemFavorite(ItemFavorite),
	ChatConversationParticipantNew(ChatConversationParticipantNew<'a>),
	FileDeletedPermanent(FileDeletedPermanent),
	FolderMetadataChanged(FolderMetadataChanged<'a>),
	FolderDeletedPermanent(FolderDeletedPermanent),
	FileMetadataChanged(FileMetadataChanged<'a>),
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

	pub(crate) async fn from_encrypted<'a>(
		crypter: &impl MetaCrypter,
		event: SocketEvent<'a>,
	) -> DecryptedSocketEvent<'a> {
		match event {
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
					DecryptedSocketEvent::ChatMessageNew(ChatMessageNew::blocking_from_encrypted(
						crypter, e,
					))
				})
				.await
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
					DecryptedSocketEvent::ChatMessageEdited(
						ChatMessageEdited::blocking_from_encrypted(crypter, e),
					)
				})
				.await
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
					DecryptedSocketEvent::ItemFavorite(ItemFavorite::blocking_from_encrypted(
						crypter, e,
					))
				})
				.await
			}
			SocketEvent::ChatConversationParticipantNew(e) => {
				runtime::do_cpu_intensive(|| {
					DecryptedSocketEvent::ChatConversationParticipantNew(
						ChatConversationParticipantNew::blocking_from_encrypted(crypter, e),
					)
				})
				.await
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
		}
	}
}

trait FromEncryptedSocketEvent<'a> {
	type Event;
	fn blocking_from_encrypted(crypter: &impl MetaCrypter, event: Self::Event) -> Self;
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct FileRename<'a> {
	pub uuid: UuidStr,
	pub metadata: FileMeta<'a>,
}

impl<'a> FromEncryptedSocketEvent<'a> for FileRename<'a> {
	type Event = filen_types::api::v3::socket::FileRename<'a>;

	fn blocking_from_encrypted(crypter: &impl MetaCrypter, event: Self::Event) -> Self {
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
pub struct FileArchiveRestored(pub RemoteFile);

impl<'a> FromEncryptedSocketEvent<'a> for FileArchiveRestored {
	type Event = filen_types::api::v3::socket::FileArchiveRestored<'a>;

	fn blocking_from_encrypted(crypter: &impl MetaCrypter, event: Self::Event) -> Self {
		// todo, what is the difference between uuid and current_uuid here?
		Self(RemoteFile {
			uuid: event.current_uuid,
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
pub struct FileNew(pub RemoteFile);

impl<'a> FromEncryptedSocketEvent<'a> for FileNew {
	type Event = filen_types::api::v3::socket::FileNew<'a>;

	fn blocking_from_encrypted(crypter: &impl MetaCrypter, event: Self::Event) -> Self {
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

impl<'a> FromEncryptedSocketEvent<'a> for FileRestore {
	type Event = filen_types::api::v3::socket::FileRestore<'a>;

	fn blocking_from_encrypted(crypter: &impl MetaCrypter, event: Self::Event) -> Self {
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
impl<'a> FromEncryptedSocketEvent<'a> for FileMove {
	type Event = filen_types::api::v3::socket::FileMove<'a>;

	fn blocking_from_encrypted(crypter: &impl MetaCrypter, event: Self::Event) -> Self {
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

impl<'a> FromEncryptedSocketEvent<'a> for FolderRename<'a> {
	type Event = filen_types::api::v3::socket::FolderRename<'a>;

	fn blocking_from_encrypted(crypter: &impl MetaCrypter, event: Self::Event) -> Self {
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
impl<'a> FromEncryptedSocketEvent<'a> for FolderMove {
	type Event = filen_types::api::v3::socket::FolderMove<'a>;

	fn blocking_from_encrypted(crypter: &impl MetaCrypter, event: Self::Event) -> Self {
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
impl<'a> FromEncryptedSocketEvent<'a> for FolderSubCreated {
	type Event = filen_types::api::v3::socket::FolderSubCreated<'a>;

	fn blocking_from_encrypted(crypter: &impl MetaCrypter, event: Self::Event) -> Self {
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
impl<'a> FromEncryptedSocketEvent<'a> for FolderRestore {
	type Event = filen_types::api::v3::socket::FolderRestore<'a>;

	fn blocking_from_encrypted(crypter: &impl MetaCrypter, event: Self::Event) -> Self {
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
pub struct ChatMessageNew;
impl<'a> FromEncryptedSocketEvent<'a> for ChatMessageNew {
	type Event = filen_types::api::v3::socket::ChatMessageNew<'a>;

	fn blocking_from_encrypted(_crypter: &impl MetaCrypter, _event: Self::Event) -> Self {
		Self
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatConversationsNew;
impl<'a> FromEncryptedSocketEvent<'a> for ChatConversationsNew {
	type Event = filen_types::api::v3::socket::ChatConversationsNew<'a>;

	fn blocking_from_encrypted(_crypter: &impl MetaCrypter, _event: Self::Event) -> Self {
		Self
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteContentEdited;
impl<'a> FromEncryptedSocketEvent<'a> for NoteContentEdited {
	type Event = filen_types::api::v3::socket::NoteContentEdited<'a>;

	fn blocking_from_encrypted(_crypter: &impl MetaCrypter, _event: Self::Event) -> Self {
		Self
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteTitleEdited;
impl<'a> FromEncryptedSocketEvent<'a> for NoteTitleEdited {
	type Event = filen_types::api::v3::socket::NoteTitleEdited<'a>;

	fn blocking_from_encrypted(_crypter: &impl MetaCrypter, _event: Self::Event) -> Self {
		Self
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessageEdited;
impl<'a> FromEncryptedSocketEvent<'a> for ChatMessageEdited {
	type Event = filen_types::api::v3::socket::ChatMessageEdited<'a>;

	fn blocking_from_encrypted(_crypter: &impl MetaCrypter, _event: Self::Event) -> Self {
		Self
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatConversationNameEdited;
impl<'a> FromEncryptedSocketEvent<'a> for ChatConversationNameEdited {
	type Event = filen_types::api::v3::socket::ChatConversationNameEdited<'a>;

	fn blocking_from_encrypted(_crypter: &impl MetaCrypter, _event: Self::Event) -> Self {
		Self
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemFavorite;
impl<'a> FromEncryptedSocketEvent<'a> for ItemFavorite {
	type Event = filen_types::api::v3::socket::ItemFavorite<'a>;

	fn blocking_from_encrypted(_crypter: &impl MetaCrypter, _event: Self::Event) -> Self {
		Self
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct ChatConversationParticipantNew<'a> {
	pub chat: UuidStr,
	pub user_id: u64,
	pub email: Cow<'a, str>,
	pub avatar: Option<Cow<'a, str>>,
	pub nick_name: Option<Cow<'a, str>>,
	pub metadata: MaybeEncrypted<'a>,
	pub permissions_add: bool,
	pub added_timestamp: DateTime<Utc>,
}

impl<'a> FromEncryptedSocketEvent<'a> for ChatConversationParticipantNew<'a> {
	type Event = filen_types::api::v3::socket::ChatConversationParticipantNew<'a>;

	fn blocking_from_encrypted(crypter: &impl MetaCrypter, event: Self::Event) -> Self {
		Self {
			chat: event.chat,
			user_id: event.user_id,
			email: event.email,
			avatar: event.avatar,
			nick_name: event.nick_name,
			metadata: match crypter.blocking_decrypt_meta(&event.metadata) {
				Ok(decrypted_metadata) => MaybeEncrypted::Decrypted(Cow::Owned(decrypted_metadata)),
				Err(_) => MaybeEncrypted::Encrypted(event.metadata),
			},
			permissions_add: event.permissions_add,
			added_timestamp: event.added_timestamp,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct FolderMetadataChanged<'a> {
	pub uuid: UuidStr,
	pub meta: DirectoryMeta<'a>,
}

impl<'a> FromEncryptedSocketEvent<'a> for FolderMetadataChanged<'a> {
	type Event = filen_types::api::v3::socket::FolderMetadataChanged<'a>;

	fn blocking_from_encrypted(crypter: &impl MetaCrypter, event: Self::Event) -> Self {
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

impl<'a> FromEncryptedSocketEvent<'a> for FileMetadataChanged<'a> {
	type Event = filen_types::api::v3::socket::FileMetadataChanged<'a>;

	fn blocking_from_encrypted(crypter: &impl MetaCrypter, event: Self::Event) -> Self {
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
