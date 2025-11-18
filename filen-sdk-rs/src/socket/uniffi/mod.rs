use std::{borrow::Cow, sync::Arc};

use crate::{Error, auth::JsClient, runtime::do_on_commander, socket::native::ListenerHandle};

mod events;

use events::SocketEvent;
use filen_types::crypto::EncryptedStringStatic;

#[uniffi::export(callback_interface)]
pub trait SocketEventListener: Send + Sync {
	fn on_event(&self, event: SocketEvent);
}

#[uniffi::export]
impl JsClient {
	pub async fn add_event_listener(
		&self,
		listener: Box<dyn SocketEventListener>,
		events_types: Option<Vec<String>>,
	) -> Result<ListenerHandle, Error> {
		let this = self.inner();
		let listener: Arc<dyn SocketEventListener> = Arc::from(listener);
		do_on_commander(move || async move {
			this.add_event_listener(
				Box::new(
					move |event: &filen_types::api::v3::socket::SocketEvent<'_>| {
						let event = SocketEvent::from(event);
						let listener = Arc::clone(&listener);
						tokio::task::spawn_blocking(move || {
							listener.on_event(event);
						});
					},
				),
				events_types.map(|v| {
					v.into_iter()
						.map(Cow::Owned)
						.collect::<Vec<Cow<'static, str>>>()
				}),
			)
			.await
		})
		.await
	}

	pub fn is_socket_connected(&self) -> bool {
		self.inner_ref().is_socket_connected()
	}

	pub async fn decrypt_meta(&self, encrypted: EncryptedStringStatic) -> Result<String, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.decrypt_meta(&encrypted).await }).await
	}
}
