use std::{sync::Arc, time::Duration};

use filen_types::api::v3::socket::{MessageType, PacketType, SocketEvent};

use crate::auth::http::AuthClient;

pub type EventListenerCallback = Box<dyn Fn(&SocketEvent<'_>) + Send + 'static>;

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

	use crate::Error;

	use super::{EventListenerCallback, SocketEvent};

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
		fn broadcast_event(&self, event: &SocketEvent<'_>) {
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
			ListenerManagerExtInner::broadcast_event(self, &SocketEvent::AuthFailed);
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

			new.broadcast_event(&SocketEvent::AuthSuccess);
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
				callback(&SocketEvent::Unsubscribed);
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
		pub(crate) fn broadcast_event(&self, event: &SocketEvent<'_>) {
			ListenerManagerExtInner::broadcast_event(self, event);
		}

		pub(crate) fn into_disconnected(self) -> DisconnectedListenerManager {
			self.broadcast_event(&SocketEvent::Reconnecting);
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
				callback(&SocketEvent::Unsubscribed);
			}
		}
	}
}

pub(super) use listener_manager::{
	ConnectedListenerManager, DisconnectedListenerManager, ListenerManagerExt,
};
