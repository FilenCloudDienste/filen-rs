#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::pin::Pin;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::{borrow::Cow, collections::HashSet, mem::ManuallyDrop, sync::Arc, time::Duration};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use filen_types::api::v3::socket::SocketEvent;

use crate::{
	Error,
	auth::{Client, http::AuthClient},
	util::{MaybeArc, MaybeArcWeak},
};

const SOCKET_HOST: &str = "socket.filen.io";
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const PING_INTERVAL: Duration = Duration::from_secs(15);

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub type EventListenerCallback = Box<dyn Fn(&SocketEvent<'_>) + 'static>;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub type EventListenerCallback = Box<
	dyn for<'a> Fn(&'a SocketEvent<'a>) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
		+ Send
		+ Sync
		+ 'static,
>;
#[derive(Clone)]
pub(crate) struct SocketConnectionState(MaybeArc<RwLock<SocketConnectionStateEnum>>);

enum SocketConnectionStateEnum {
	Uninintialized(UninitSocketConnection),
	Initialized(InitSocketConnection),
	Tmp,
}

fn uninitialize(state: &mut SocketConnectionStateEnum) {
	let mut tmp_state = SocketConnectionStateEnum::Tmp;
	std::mem::swap(state, &mut tmp_state);
	let init = match tmp_state {
		SocketConnectionStateEnum::Initialized(init) => init,
		other => {
			*state = other;
			return;
		}
	};

	init.ws_handle.close();

	*state = SocketConnectionStateEnum::Uninintialized(UninitSocketConnection {
		client: init.client,
		ws_url: init.socket_url,
		reconnect_delay: init.reconnect_delay,
		max_reconnect_delay: init.max_reconnect_delay,
		ping_interval: init.ping_interval,
	});
}

enum AddListenerReturn<'a> {
	Success(EventListenerHandle),
	Fail(
		RwLockWriteGuard<'a, SocketConnectionStateEnum>,
		EventListener,
	),
}

impl SocketConnectionState {
	pub(crate) fn new(client: Arc<AuthClient>, config: SocketConfig) -> Self {
		Self(MaybeArc::new(RwLock::new(
			SocketConnectionStateEnum::Uninintialized(UninitSocketConnection {
				client,
				ws_url: format!(
					"{}://{}/socket.io/",
					if config.tls { "wss" } else { "ws" },
					&config.socket_url
				),
				reconnect_delay: RECONNECT_DELAY,
				max_reconnect_delay: MAX_RECONNECT_DELAY,
				ping_interval: PING_INTERVAL,
			}),
		)))
	}

	fn inner_add_listener<'a>(
		&self,
		mut write_guard: RwLockWriteGuard<'a, SocketConnectionStateEnum>,
		listener: EventListener,
	) -> AddListenerReturn<'a> {
		match &mut *write_guard {
			SocketConnectionStateEnum::Initialized(conn) => AddListenerReturn::Success(
				conn.listener_manager.add_listener(listener, self.clone()),
			),
			_ => AddListenerReturn::Fail(write_guard, listener),
		}
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	fn get_write_guard(&self) -> RwLockWriteGuard<'_, SocketConnectionStateEnum> {
		self.0.write().unwrap_or_else(|e| e.into_inner())
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	async fn get_write_guard(&self) -> RwLockWriteGuard<'_, SocketConnectionStateEnum> {
		self.0.write().await
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	fn get_read_guard(&self) -> RwLockReadGuard<'_, SocketConnectionStateEnum> {
		self.0.read().unwrap_or_else(|e| e.into_inner())
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	async fn get_read_guard(&self) -> RwLockReadGuard<'_, SocketConnectionStateEnum> {
		self.0.read().await
	}

	async fn add_listener(
		&self,
		event_types: Option<HashSet<String>>,
		callback: EventListenerCallback,
	) -> Result<EventListenerHandle, Error> {
		let (mut write_guard, listener) = match self.inner_add_listener(
			{
				#[cfg(all(target_family = "wasm", target_os = "unknown"))]
				{
					self.get_write_guard()
				}
				#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
				{
					self.get_write_guard().await
				}
			},
			EventListener {
				event_types,
				callback,
			},
		) {
			AddListenerReturn::Success(handle) => return Ok(handle),
			AddListenerReturn::Fail(wg, l) => (wg, l),
		};
		let uninit = match &mut *write_guard {
			SocketConnectionStateEnum::Uninintialized(uninit_socket_connection) => {
				uninit_socket_connection
			}
			_ => unreachable!("Should never be Tmp here, and we just checked for Initialized "),
		};

		let listener_manager = ListenerManager {
			listeners: Vec::new(),
		};

		let websocket_url = format!(
			"{}?EIO=3&transport=websocket&t={}",
			&uninit.ws_url,
			chrono::Utc::now().timestamp_millis()
		);

		// initialize WebSocket connection
		let ws_handle = {
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				use wasm_bindgen::JsCast;

				let ws = web_sys::WebSocket::new(&websocket_url).map_err(|e| {
					Error::custom(
						crate::ErrorKind::Server,
						format!("Failed to create WebSocket connection: {:?}", e),
					)
				})?;

				let closure_state = self.clone();
				let closure =
					wasm_bindgen::closure::Closure::<dyn Fn(String)>::new(move |msg: String| {
						log::info!("WebSocket message: {:?}", msg);
						let read_guard = closure_state.get_read_guard();
						if let SocketConnectionStateEnum::Initialized(conn) = &*read_guard {
							conn.listener_manager.handle_message(&msg)
						}
					})
					.into_js_value();

				ws.set_onmessage(Some(closure.unchecked_ref()));
				WebSocketHandle { wasm: ws }
			}
			// #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			// {
			// 	use tokio_tungstenite::tungstenite::client::IntoClientRequest;
			// 	log::info!("Connecting to WebSocket: {}", websocket_url);

			// 	let (mut socket, _) = tokio_tungstenite::connect_async(
			// 		websocket_url.into_client_request().map_err(|e| {
			// 			Error::custom(
			// 				crate::ErrorKind::Server,
			// 				format!("Failed to create WebSocket request: {}", e),
			// 			)
			// 		})?,
			// 	)
			// 	.await
			// 	.map_err(|e| {
			// 		Error::custom(
			// 			crate::ErrorKind::Server,
			// 			format!("Failed to connect to WebSocket: {}", e),
			// 		)
			// 	})?;
			// 	let closure_state = self.clone();

			// 	let (close_sender, mut close_recv) = tokio::sync::oneshot::channel::<()>();
			// 	let (message_sender, mut msg_recv) =
			// 		tokio::sync::mpsc::unbounded_channel::<String>();

			// 	let handle = tokio::spawn(async move {
			// 		use filen_types::api::v3::socket::PacketType;
			// 		use futures::{SinkExt, StreamExt};
			// 		let msg = socket.next().await.unwrap().unwrap();
			// 		let msg = msg.into_data();
			// 		let Ok(packet_type) = PacketType::try_from(msg[0]) else {
			// 			log::error!("Invalid packet type: {}", msg[0]);
			// 			return false;
			// 		};

			// 		match packet_type {
			// 			PacketType::Connect => {
			// 				use filen_types::api::v3::socket::{HandShake, MessageType};

			// 				log::info!("WebSocket connected");
			// 				let Ok(handshake) = serde_json::from_slice::<HandShake>(&msg[1..])
			// 				else {
			// 					log::error!("Invalid message type: {}", msg[1]);
			// 					return false;
			// 				};
			// 				match socket
			// 					.send(tungstenite::Message::Text(
			// 						format!(
			// 							"{}{}",
			// 							PacketType::Message as u8,
			// 							MessageType::Connect as u8
			// 						)
			// 						.into(),
			// 					))
			// 					.await
			// 				{
			// 					Ok(_) => {}
			// 					Err(e) => {
			// 						log::error!("Failed to send WebSocket message: {}", e);
			// 						return false;
			// 					}
			// 				}
			// 			}
			// 			other => {
			// 				log::error!("Expected Connect packet, got: {:?}", other);
			// 				return false;
			// 			}
			// 		}

			// 		loop {
			// 			tokio::select! {
			// 				biased;
			// 				_ = &mut close_recv => {
			// 					if let Err(e) = socket.close(None).await {
			// 						log::error!("Error when trying to close websocket: {}", e);
			// 						return false;
			// 					} else {
			// 						log::info!("WebSocket receive loop terminated by sender");
			// 						return true;
			// 					}
			// 				}
			// 				msg_to_send = msg_recv.recv() => {
			// 					if let Some(msg) = msg_to_send {
			// 						if let Err(e) = socket.send(tungstenite::Message::Text(msg.into())).await {
			// 							log::error!("Failed to send WebSocket message: {}", e);
			// 							return false;
			// 						}
			// 					} else {
			// 						log::info!("WebSocket send channel closed");
			// 						return true;
			// 					}
			// 				}
			// 				msg = socket.next() => {
			// 					match msg {
			// 					Some(Ok(tungstenite::Message::Text(txt))) => {
			// 						log::info!("WebSocket message: {:?}", txt);
			// 						let read_guard = closure_state.get_read_guard().await;
			// 						if let SocketConnectionStateEnum::Initialized(conn) = &*read_guard {
			// 							conn.listener_manager.handle_message(&txt).await;
			// 						}
			// 					}
			// 					Some(Ok(tungstenite::Message::Close(_))) => {
			// 						log::info!("WebSocket closed");
			// 						return true;
			// 					}
			// 					Some(Ok(_)) => {}
			// 					Some(Err(e)) => {
			// 						log::error!("WebSocket error: {}", e);
			// 						return false;
			// 					}
			// 					None => {
			// 						log::info!("WebSocket stream ended");
			// 						return true;
			// 					}
			// 				}
			// 				}
			// 			}
			// 		}
			// 	});
			// 	WebSocketHandle {
			// 		handle,
			// 		close_sender,
			// 		message_sender,
			// 	}
			// }
		};

		let mut tmp_state = SocketConnectionStateEnum::Tmp;

		std::mem::swap(&mut *write_guard, &mut tmp_state);

		let uninit = match tmp_state {
			SocketConnectionStateEnum::Uninintialized(cfg) => cfg,
			_ => unreachable!("we know it was Uninitialized above"),
		};

		*write_guard = SocketConnectionStateEnum::Initialized(InitSocketConnection {
			client: uninit.client,
			socket_url: uninit.ws_url,
			reconnect_delay: uninit.reconnect_delay,
			max_reconnect_delay: uninit.max_reconnect_delay,
			ping_interval: uninit.ping_interval,
			ws_handle,
			listener_manager,
		});

		match self.inner_add_listener(write_guard, listener) {
			AddListenerReturn::Success(handle) => Ok(handle),
			AddListenerReturn::Fail(_, _) => unreachable!("we just set self to Initialized"),
		}
	}

	// Only called when there is a listener being dropped and therefore a cleanup is needed
	fn cleanup(&mut self) {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			let mut write_guard = self.get_write_guard();
			if let SocketConnectionStateEnum::Initialized(conn) = &mut *write_guard {
				conn.listener_manager.cleanup();
				if conn.listener_manager.listeners.is_empty() {
					uninitialize(&mut write_guard);
				}
			}
		}
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			let state = self.clone();
			tokio::spawn(async move {
				let mut write_guard = state.get_write_guard().await;
				if let SocketConnectionStateEnum::Initialized(conn) = &mut *write_guard {
					conn.listener_manager.cleanup();
					if conn.listener_manager.listeners.is_empty() {
						uninitialize(&mut write_guard);
					}
				}
			});
		}
	}
}

struct UninitSocketConnection {
	client: Arc<AuthClient>,
	ws_url: String,
	reconnect_delay: Duration,
	max_reconnect_delay: Duration,
	ping_interval: Duration,
}

struct InitSocketConnection {
	client: Arc<AuthClient>,
	socket_url: String,
	reconnect_delay: Duration,
	max_reconnect_delay: Duration,
	ping_interval: Duration,

	ws_handle: WebSocketHandle,
	listener_manager: ListenerManager,
}

struct WebSocketHandle {
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	wasm: web_sys::WebSocket,
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	handle: tokio::task::JoinHandle<bool>,
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	close_sender: tokio::sync::oneshot::Sender<()>,
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	message_sender: tokio::sync::mpsc::UnboundedSender<String>,
}

impl WebSocketHandle {
	fn close(&self) {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			let _ = self.wasm.close();
		}
		// #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		// {
		// 	// closing with tokio is handled automatically when the Handle is dropped
		// }
	}

	fn send(&self, _msg: &str) -> Result<(), Error> {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			self.wasm.send_with_str(_msg).map_err(|e| {
				Error::custom(
					crate::ErrorKind::Server,
					format!("Failed to send WebSocket message: {:?}", e),
				)
			})
		}
		// #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		// {
		// 	unimplemented!(
		// 		"Sending messages over WebSocket is not implemented yet for non-wasm targets"
		// 	);
		// }
	}
}

// fn f() {
// 	fn g<T: Send>() {}
// 	g::<Client>();
// }

struct EventListener {
	event_types: Option<HashSet<String>>,
	callback: EventListenerCallback,
}

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen
)]
pub struct EventListenerHandle {
	// we use ManuallyDrop here so that we can drop the Arc before we call cleanup on state
	my_listener: ManuallyDrop<MaybeArc<EventListener>>,
	state: SocketConnectionState,
}

impl Drop for EventListenerHandle {
	fn drop(&mut self) {
		// SAFETY: we are the only ones with access to my_listener
		// this function is called exactly once when the handle is dropped
		std::mem::drop(unsafe { ManuallyDrop::take(&mut self.my_listener) });
		self.state.cleanup();
	}
}

struct ListenerManager {
	listeners: Vec<MaybeArcWeak<EventListener>>,
}

impl ListenerManager {
	fn add_listener(
		&mut self,
		listener: EventListener,
		state: SocketConnectionState,
	) -> EventListenerHandle {
		let my_listener = MaybeArc::new(listener);
		self.listeners.push(MaybeArc::downgrade(&my_listener));
		EventListenerHandle {
			my_listener: ManuallyDrop::new(my_listener),
			state,
		}
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	fn handle_message(&self, msg: &str) {
		let event: SocketEvent<'_> = match serde_json::from_str(msg) {
			Ok(event) => event,
			Err(e) => {
				log::error!("Failed to parse WebSocket message: {}", e);
				return;
			}
		};

		for weak_ref in self.listeners.iter() {
			if let Some(listener) = weak_ref.upgrade()
				&& listener
					.event_types
					.as_ref()
					.is_none_or(|set| set.contains(event.event_type()))
			{
				(listener.callback)(&event);
			}
		}
	}
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	async fn handle_message(&self, msg: &str) {
		let event: SocketEvent<'_> = match serde_json::from_str(msg) {
			Ok(event) => event,
			Err(e) => {
				log::error!("Failed to parse WebSocket message: {}", e);
				return;
			}
		};

		use futures::{StreamExt, stream::FuturesUnordered};

		let mut futures: FuturesUnordered<_> = self
			.listeners
			.iter()
			.filter_map(|weak_ref| {
				if let Some(listener) = weak_ref.upgrade() {
					if listener
						.event_types
						.as_ref()
						.is_none_or(|set| set.contains(event.event_type()))
					{
						Some((listener.callback)(&event))
					} else {
						None
					}
				} else {
					None
				}
			})
			.collect();
		while let Some(()) = futures.next().await {}
	}

	fn cleanup(&mut self) {
		let mut garbage_ids = Vec::with_capacity(self.listeners.len());
		for (i, weak_ref) in self.listeners.iter().enumerate() {
			if weak_ref.upgrade().is_none() {
				garbage_ids.push(i);
			}
		}
		for i in garbage_ids.into_iter() {
			self.listeners.swap_remove(i);
		}
	}
}

pub(crate) struct SocketConfig {
	socket_url: Cow<'static, str>,
	tls: bool,
}

impl Default for SocketConfig {
	fn default() -> Self {
		Self {
			socket_url: Cow::Borrowed(SOCKET_HOST),
			tls: true,
		}
	}
}

impl SocketConfig {
	pub fn new(socket_url: Option<String>, tls: Option<bool>) -> Self {
		Self {
			socket_url: socket_url
				.map(Cow::Owned)
				.unwrap_or(Cow::Borrowed(SOCKET_HOST)),
			tls: tls.unwrap_or(true),
		}
	}
}

impl Client {
	pub async fn add_socket_listener(
		&self,
		event_types: Option<HashSet<String>>,
		callback: EventListenerCallback,
	) -> Result<EventListenerHandle, Error> {
		self.socket_connection
			.add_listener(event_types, callback)
			.await
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod js_impl {
	use filen_types::api::v3::socket::SocketEvent;
	use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
	use web_sys::js_sys;

	use crate::{Error, auth::Client, sockets::EventListenerHandle};

	#[wasm_bindgen]
	impl Client {
		#[wasm_bindgen(js_name = "addSocketListener")]
		pub async fn js_add_socket_listener(
			&self,
			event_types: Option<Vec<String>>,
			listener: js_sys::Function,
		) -> Result<EventListenerHandle, Error> {
			let callback = Box::new(move |event: &SocketEvent<'_>| {
				let _ = listener.call1(
					&JsValue::UNDEFINED,
					&serde_wasm_bindgen::to_value(event)
						.expect("failed to serialize event to JsValue (should be impossible)"),
				);
			});
			self.add_socket_listener(event_types.map(|v| v.into_iter().collect()), callback)
				.await
		}
	}
}
