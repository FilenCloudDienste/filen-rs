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

use thread_handling::ListenerHandle;
use traits::EventListenerCallback;

pub(crate) use thread_handling::WebSocketHandle;

pub use events::DecryptedSocketEvent;

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

	pub fn is_socket_connected(&self) -> bool {
		self.socket_handle
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.is_connected()
	}
}
