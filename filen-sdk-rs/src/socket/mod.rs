use crate::{Error, auth::Client};

use std::borrow::Cow;

mod consts;
mod events;
#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
mod js;
mod listener_manager;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) mod native;
mod thread_handling;
mod traits;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod wasm;

use traits::EventListenerCallback;

pub(crate) use thread_handling::WebSocketHandle;

pub use {events::*, thread_handling::ListenerHandle};

impl Client {
	pub async fn add_event_listener(
		&self,
		callback: EventListenerCallback,
		event_types: Option<Vec<Cow<'static, str>>>,
	) -> Result<ListenerHandle, Error> {
		let request_sender = {
			let mut socket_handle = self.socket_handle.lock().unwrap_or_else(|e| e.into_inner());
			socket_handle.get_request_sender(self.arc_client_ref(), self)
		};
		request_sender
			.add_event_listener(callback, event_types)
			.await
	}

	/// Register `callback` and return its handle WITHOUT waiting for the socket to connect.
	/// Unlike [`add_event_listener`](Self::add_event_listener) — which resolves only once the
	/// socket is connected (or auth fails) — this inserts the callback into the routing table
	/// synchronously, even while disconnected, so the caller never wedges on a connect ack.
	/// Registration failure still surfaces synchronously, and once the socket connects every event
	/// is delivered to the callback like any other listener.
	pub async fn add_event_listener_sync(
		&self,
		callback: EventListenerCallback,
		event_types: Option<Vec<Cow<'static, str>>>,
	) -> Result<ListenerHandle, Error> {
		let request_sender = {
			let mut socket_handle = self.socket_handle.lock().unwrap_or_else(|e| e.into_inner());
			socket_handle.get_request_sender(self.arc_client_ref(), self)
		};
		request_sender
			.add_event_listener_sync(callback, event_types)
			.await
	}

	pub fn is_socket_connected(&self) -> bool {
		self.socket_handle
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.is_connected()
	}

	pub async fn get_last_event_ids(
		&self,
	) -> Result<filen_types::api::v3::message_ids::Response, Error> {
		crate::api::v3::message_ids::get(self.client()).await
	}
}
