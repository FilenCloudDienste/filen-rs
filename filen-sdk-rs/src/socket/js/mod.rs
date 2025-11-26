mod events;

#[cfg(feature = "uniffi")]
mod uniffi {
	use std::{borrow::Cow, sync::Arc};

	use crate::{
		Error,
		auth::JsClient,
		runtime::do_on_commander,
		socket::{DecryptedSocketEvent, thread_handling::ListenerHandle},
	};

	use super::events::SocketEvent;

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
				let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
				tokio::task::spawn_blocking(move || {
					while let Some(event) = receiver.blocking_recv() {
						listener.on_event(event);
					}
				});

				this.add_event_listener(
					Box::new(move |event: &DecryptedSocketEvent<'_>| {
						let _ = sender.send(event.into());
					}),
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
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod wasm {
	use std::borrow::Cow;

	use wasm_bindgen::JsValue;
	use web_sys::js_sys;

	use crate::{
		Error,
		auth::JsClient,
		runtime,
		socket::{DecryptedSocketEvent, thread_handling::ListenerHandle},
	};

	use super::events::SocketEvent;

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
	)]
	impl JsClient {
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "addEventListener")
		)]
		pub async fn add_event_listener(
			&self,
			#[wasm_bindgen(unchecked_param_type = "(event: SocketEvent) => void")]
			listener: js_sys::Function,
			#[wasm_bindgen(unchecked_param_type = "SocketEventType[] | null")] event_types: Option<
				Vec<String>,
			>,
		) -> Result<ListenerHandle, Error> {
			let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<SocketEvent>();

			runtime::spawn_local(async move {
				while let Some(event) = receiver.recv().await {
					let serializer = serde_wasm_bindgen::Serializer::new()
						.serialize_maps_as_objects(true)
						.serialize_large_number_types_as_bigints(true);

					let _ = listener.call1(
						&JsValue::UNDEFINED,
						&serde::Serialize::serialize(&event, &serializer)
							.expect("failed to serialize event to JsValue (should be impossible)"),
					);
				}
			});

			let callback = Box::new(move |event: &DecryptedSocketEvent<'_>| {
				let _ = sender.send(event.into());
			});

			let this = self.inner();
			runtime::do_on_commander(move || async move {
				this.add_event_listener(
					callback,
					event_types.map(|v| v.into_iter().map(Cow::Owned).collect()),
				)
				.await
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "isSocketConnected")
		)]
		pub async fn is_socket_connected(&self) -> bool {
			let this = self.inner();
			runtime::do_on_commander(move || async move { this.is_socket_connected() }).await
		}
	}
}
