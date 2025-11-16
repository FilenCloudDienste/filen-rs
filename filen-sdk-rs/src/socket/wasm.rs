use std::{
	borrow::Cow,
	cell::Cell,
	collections::HashSet,
	fmt::Write,
	mem::ManuallyDrop,
	rc::Rc,
	sync::{Arc, Weak},
	time::Duration,
};
use tokio::sync::{Mutex, MutexGuard, oneshot};
use wasm_bindgen::{JsCast, UnwrapThrowExt};
use web_sys::{CloseEvent, WebSocket};

use filen_types::{
	api::v3::socket::{MessageType, PacketType, SocketEvent},
	crypto::EncryptedString,
};

use crate::{
	Error,
	auth::{
		Client,
		http::{AuthClient, AuthorizedClient},
	},
	crypto::shared::MetaCrypter,
	error::ResultExt,
	runtime::{self, do_cpu_intensive},
};

const SOCKET_HOST: &str = "socket.filen.io";
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const PING_INTERVAL: Duration = Duration::from_secs(15);

pub type EventListenerCallback = Box<dyn Fn(&SocketEvent<'_>) + Send + Sync + 'static>;
#[derive(Clone)]
pub(crate) struct SocketConnectionState(Arc<Mutex<SocketConnectionStateCarrier>>);

// submodule to make sure I don't accidentally take out the value manually
mod socket_state_carrier {
	use super::SocketConnectionStateEnum;

	pub(super) struct SocketConnectionStateCarrier(Option<SocketConnectionStateEnum>);

	impl SocketConnectionStateCarrier {
		pub(super) fn new(state: SocketConnectionStateEnum) -> Self {
			Self(Some(state))
		}

		pub(super) fn with_owned<F, R>(&mut self, f: F) -> R
		where
			F: FnOnce(SocketConnectionStateEnum) -> (R, SocketConnectionStateEnum),
		{
			let owned = self.0.take().expect("value was already taken");
			let (result, new_value) = f(owned);
			self.0 = Some(new_value);
			result
		}

		pub(super) async fn async_with_owned<F, R>(&mut self, f: F) -> R
		where
			F: AsyncFnOnce(SocketConnectionStateEnum) -> (R, SocketConnectionStateEnum),
		{
			let owned = self.0.take().expect("value was already taken");
			let (result, new_value) = f(owned).await;
			self.0 = Some(new_value);
			result
		}

		pub(super) fn borrow(&self) -> &SocketConnectionStateEnum {
			self.0.as_ref().expect("value was already taken")
		}
	}
}

use socket_state_carrier::SocketConnectionStateCarrier;

enum SocketConnectionStateEnum {
	// no listeners, no connection
	Uninintialized(UninitSocketConnection),
	// connected, authenticated and with listeners
	Initialized(InitSocketConnection),
	// connecting or authenticating, with listeners, will broadcast when authenticated
	Initializing(InitSocketConnection, tokio::sync::broadcast::Sender<bool>),
	// not connected, with listeners, trying to connect, will broadcast when authenticated
	Reconnecting(
		UninitSocketConnection,
		ListenerManager,
		tokio::sync::broadcast::Sender<bool>,
	),
}

enum AddListenerReturn<'a> {
	Success(EventListenerHandle),
	Fail(MutexGuard<'a, SocketConnectionStateEnum>, EventListener),
}

struct EventAndOptionalData<'a, T> {
	event: &'a str,
	data: Option<&'a T>,
}

impl<T> serde::Serialize for EventAndOptionalData<'_, T>
where
	T: serde::Serialize,
{
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		use serde::ser::SerializeSeq;
		let mut seq = serializer.serialize_seq(Some(1 + self.data.is_some() as usize))?;
		seq.serialize_element(&self.event)?;
		if let Some(data) = &self.data {
			seq.serialize_element(data)?;
		}
		seq.end()
	}
}

const MESSAGE_EVENT_PAYLOAD: &str =
	match str::from_utf8(&[PacketType::Message as u8, MessageType::Event as u8]) {
		Ok(s) => s,
		Err(_) => panic!("Failed to create handshake payload string"),
	};

fn send_str(ws: &WebSocket, msg: &str) -> Result<(), Error> {
	ws.send_with_str(msg).map_err(|e| {
		Error::custom(
			crate::ErrorKind::Server,
			format!("Failed to send WebSocket message: {:?}", e),
		)
	})?;
	Ok(())
}

fn send_event(
	ws: &WebSocket,
	event: &str,
	data: Option<&impl serde::Serialize>,
) -> Result<(), Error> {
	let payload = EventAndOptionalData { event, data };
	let mut packet = String::new();
	packet.push_str(MESSAGE_EVENT_PAYLOAD);
	// SAFETY: we are only appending valid UTF-8 to a valid UTF-8 string
	serde_json::to_writer(unsafe { packet.as_mut_vec() }, &payload)
		.expect("string message serialization should not fail");

	send_str(ws, &packet).map_err(|e| {
		Error::custom(
			crate::ErrorKind::Server,
			format!("Failed to send WebSocket message: {:?}", e),
		)
	})?;
	Ok(())
}

fn handle_handshake(
	ws: &WebSocket,
	interval_change_sender: &tokio::sync::mpsc::UnboundedSender<Duration>,
	msg: &str,
) -> Result<(), Error> {
	use filen_types::api::v3::socket::HandShake;

	let handshake: HandShake = serde_json::from_str(msg).map_err(|e| {
		Error::custom(
			crate::ErrorKind::Server,
			format!("Failed to parse handshake message: {}", e),
		)
	})?;
	// don't care if the send fails, the connection will be closed soon if it does
	let _ = interval_change_sender.send(Duration::from_millis(handshake.ping_interval));
	send_str(ws, MESSAGE_EVENT_PAYLOAD).map_err(|e| {
		Error::custom(
			crate::ErrorKind::Server,
			format!("Failed to send WebSocket message: {:?}", e),
		)
	})?;
	Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthMessage<'a> {
	api_key: Cow<'a, str>,
}

fn try_handle_event(event_str: &str, listener_manager: &ListenerManager) -> Result<(), Error> {
	let socket_event: SocketEvent = serde_json::from_str(event_str).map_err(|e| {
		Error::custom(
			crate::ErrorKind::Server,
			format!("Failed to parse WebSocket event: {}", e),
		)
	})?;
	listener_manager.handle_event(&socket_event);
	Ok(())
}

fn parse_message(msg: &str) -> Result<Option<&str>, Error> {
	let message_type = msg.bytes().next().ok_or_else(|| {
		Error::custom(
			crate::ErrorKind::Server,
			"Empty message received over WebSocket",
		)
	})?;

	match MessageType::try_from(message_type) {
		Err(e) => {
			log::error!("Invalid message type: {}", e);
			Ok(None)
		}
		Ok(MessageType::Event) => {
			log::info!("Received event message: {}", &msg[1..]);
			Ok(Some(&msg[1..]))
		}
		Ok(_) => {
			// ignore other message types for now
			Ok(None)
		}
	}
}

const AUTHED_FALSE_MESSAGE: &str = r#"["authed",false]"#;
const AUTHED_TRUE_MESSAGE: &str = r#"["authed",true]"#;
const AUTH_SUCCESS_MESSAGE: &str = r#"["authSuccess"]"#;
const AUTH_FAILED_MESSAGE: &str = r#"["authFailed"]"#;

fn handle_unauthed_message(
	ws: &std::rc::Weak<WebSocket>,
	msg: &str,
	connection: InitSocketConnection,
	broadcaster: tokio::sync::broadcast::Sender<bool>,
) -> Result<SocketConnectionStateEnum, (SocketConnectionStateEnum, Error)> {
	match parse_message(msg) {
		Ok(Some(AUTHED_FALSE_MESSAGE)) => {
			{
				let client = &*connection.config.client;
				let api_key = client.get_api_key();
				let msg = Some(&AuthMessage {
					api_key: Cow::Borrowed(&api_key.0),
				});
				let Some(ws) = ws.upgrade() else {
					std::mem::drop(api_key);
					log::error!("WebSocket was closed before authentication could be sent");
					return Err((
						SocketConnectionStateEnum::Uninintialized(UninitSocketConnection {
							config: connection.config,
						}),
						Error::custom(
							crate::ErrorKind::Server,
							"WebSocket was closed before authentication could be sent",
						),
					));
				};
				if let Err(e) = send_event(&ws, "auth", msg) {
					std::mem::drop(api_key);
					log::error!("Failed to send auth event: {}", e);
					return Err((
						SocketConnectionStateEnum::Initializing(connection, broadcaster),
						e,
					));
				}
			}

			Ok(SocketConnectionStateEnum::Initializing(
				connection,
				broadcaster,
			))
		}
		Ok(Some(AUTHED_TRUE_MESSAGE)) => Ok(SocketConnectionStateEnum::Initializing(
			connection,
			broadcaster,
		)),
		Ok(Some(AUTH_SUCCESS_MESSAGE)) => {
			let _ = broadcaster.send(true);
			match try_handle_event(AUTH_SUCCESS_MESSAGE, &connection.listener_manager) {
				Ok(()) => Ok(SocketConnectionStateEnum::Initialized(connection)),
				Err(e) => Err((
					SocketConnectionStateEnum::Uninintialized(UninitSocketConnection {
						config: connection.config,
					}),
					e,
				)),
			}
		}
		Ok(Some(AUTH_FAILED_MESSAGE)) => {
			log::error!("WebSocket authentication failed");
			// first notify listeners, then drop them
			let res = match try_handle_event(AUTH_FAILED_MESSAGE, &connection.listener_manager) {
				Ok(()) => Ok(SocketConnectionStateEnum::Uninintialized(
					UninitSocketConnection {
						config: connection.config,
					},
				)),
				Err(e) => Err((
					SocketConnectionStateEnum::Uninintialized(UninitSocketConnection {
						config: connection.config,
					}),
					e,
				)),
			};
			let _ = broadcaster.send(false);
			res
		}
		Ok(Some(other)) => match try_handle_event(other, &connection.listener_manager) {
			Ok(()) => Ok(SocketConnectionStateEnum::Initializing(
				connection,
				broadcaster,
			)),
			Err(e) => Err((
				SocketConnectionStateEnum::Initializing(connection, broadcaster),
				e,
			)),
		},
		Ok(None) => Ok(SocketConnectionStateEnum::Initializing(
			connection,
			broadcaster,
		)),
		Err(e) => Err((
			SocketConnectionStateEnum::Initializing(connection, broadcaster),
			e,
		)),
	}
}

fn handle_authed_message(
	ws: &std::rc::Weak<WebSocket>,
	msg: &str,
	connection: &InitSocketConnection,
) -> Result<(), Error> {
	match parse_message(msg) {
		Ok(Some(AUTHED_TRUE_MESSAGE)) => {
			let client = &*connection.config.client;
			let api_key = client.get_api_key();
			let msg = Some(&AuthMessage {
				api_key: Cow::Borrowed(&api_key.0),
			});

			if let Some(ws) = ws.upgrade() {
				send_event(&ws, "auth", msg)?;
			}
		}
		Ok(Some(AUTHED_FALSE_MESSAGE)) => {}
		Ok(Some(other)) => try_handle_event(other, &connection.listener_manager)?,
		Ok(None) => {}
		Err(e) => return Err(e),
	}
	Ok(())
}

const PING_MESSAGE: &str = match str::from_utf8(&[PacketType::Ping as u8]) {
	Ok(s) => s,
	Err(_) => panic!("Failed to create ping message string"),
};

fn start_ping_task(
	ws: std::rc::Weak<WebSocket>,
	mut interval: Duration,
	mut interval_change_receiver: tokio::sync::mpsc::UnboundedReceiver<Duration>,
) {
	let mut last_update = wasmtimer::std::Instant::now();
	runtime::spawn_local(async move {
		let mut timestamp_string = String::new();
		loop {
			tokio::select! {
				biased;
				Some(new_interval) = interval_change_receiver.recv() => {
					interval = new_interval;
				}
				_ = wasmtimer::tokio::sleep(interval.saturating_sub(last_update.elapsed())) => {
					let Some(ws) = ws.upgrade() else {
						// automatic cleanup when the WebSocket is dropped
						return;
					};

					if send_str(&ws, PING_MESSAGE).is_err() {
						log::warn!("Failed to send ping message, stopping ping task");
						return;
					}
					timestamp_string.clear();
					write!(
						&mut timestamp_string,
						"{}",
						chrono::Utc::now().timestamp_millis()
					)
					.expect("writing integer to string should not fail");
					send_event(&ws, "authed", Some(&timestamp_string)).unwrap_or_else(|e| {
						log::error!("Failed to send authed over WebSocket: {:?}", e);
					});
					last_update = wasmtimer::std::Instant::now();
				}
			}
		}
	});
}

fn start_reconnect_task(
	connection: Weak<Mutex<SocketConnectionStateCarrier>>,
	mut reconnect_delay: Duration,
	max_reconnect_delay: Duration,
) {
	let mut last_update = wasmtimer::std::Instant::now();
	runtime::spawn_local(async move {
		loop {
			// need this to be a separate block so that the guard is dropped before the await point
			// clippy still complains if I std::mem::drop it manually
			let should_break = {
				let Some(connection) = connection.upgrade() else {
					// automatic cleanup when the WebSocket is dropped
					return;
				};
				let mut guard = connection.lock().await;
				guard
					.async_with_owned(async |state| match state {
						SocketConnectionStateEnum::Reconnecting(
							init_socket_connection,
							listener_manger,
							broadcaster,
						) => {
							match setup_websocket(
								Arc::downgrade(&connection),
								SocketUninitOrReconnecting::Reconnecting(
									init_socket_connection,
									listener_manger,
								),
							)
							.await
							{
								Ok(connection) => (
									true,
									SocketConnectionStateEnum::Initializing(
										connection,
										broadcaster,
									),
								),
								Err((e, uninit, listener_manager)) => {
									log::warn!("Failed to recreate WebSocket: {}", e);
									(
										false,
										SocketConnectionStateEnum::Reconnecting(
											uninit,
											listener_manager,
											broadcaster,
										),
									)
								}
							}
						}
						other => (true, other),
					})
					.await
			};

			if should_break {
				break;
			}

			let sleep_duration = reconnect_delay.saturating_sub(last_update.elapsed());
			last_update = wasmtimer::std::Instant::now();
			reconnect_delay *= 2;
			if reconnect_delay > max_reconnect_delay {
				reconnect_delay = max_reconnect_delay;
			}
			wasmtimer::tokio::sleep(sleep_duration).await;
		}
	});
}

fn make_on_message_closure(
	ws: std::rc::Weak<WebSocket>,
	interval_change_sender: tokio::sync::mpsc::UnboundedSender<Duration>,
	connection: Weak<Mutex<SocketConnectionStateCarrier>>,
) -> wasm_bindgen::prelude::Closure<dyn Fn(web_sys::MessageEvent)> {
	let interval_change_sender = std::sync::Mutex::new(interval_change_sender);
	wasm_bindgen::prelude::Closure::<dyn Fn(web_sys::MessageEvent)>::new(
		move |msg: web_sys::MessageEvent| {
			let msg = msg.data();
			use filen_types::api::v3::socket::PacketType;
			let Some(msg) = msg.as_string() else {
				log::error!("Invalid message type");
				return;
			};

			let Some(packet_type) = msg.bytes().next() else {
				log::error!("Invalid packet type: {}", msg);
				return;
			};
			match PacketType::try_from(packet_type) {
				Err(e) => {
					log::error!("Invalid packet type: {}", e);
				}
				Ok(PacketType::Connect) => {
					let Some(connection) = connection.upgrade() else {
						return;
					};
					let guard = connection.blocking_lock();
					if let SocketConnectionStateEnum::Initializing(_, _) = guard.borrow() {
						let interval_change_sender = interval_change_sender
							.lock()
							.unwrap_or_else(|e| e.into_inner());
						if let Some(ws) = ws.upgrade() {
							handle_handshake(&ws, &interval_change_sender, &msg[1..])
								.unwrap_or_else(|e| {
									log::error!("Failed to handle handshake: {}", e);
								});
						}
					}
				}
				Ok(PacketType::Message) => {
					let Some(connection) = connection.upgrade() else {
						return;
					};
					let mut guard = connection.blocking_lock();
					match guard.borrow() {
						SocketConnectionStateEnum::Initialized(conn) => {
							handle_authed_message(&ws, &msg[1..], conn).unwrap_or_else(|e| {
								log::error!("Failed to handle message: {}", e);
							});
						}
						SocketConnectionStateEnum::Initializing(_, _) => {
							guard.with_owned(|state| match state {
								SocketConnectionStateEnum::Initializing(conn, broadcaster) => (
									(),
									handle_unauthed_message(&ws, &msg[1..], conn, broadcaster)
										.unwrap_or_else(|(new_state, e)| {
											log::error!("Failed to handle message: {}", e);
											new_state
										}),
								),
								SocketConnectionStateEnum::Initialized(init) => {
									handle_authed_message(&ws, &msg[1..], &init).unwrap_or_else(
										|e| {
											log::error!("Failed to handle message: {}", e);
										},
									);
									((), SocketConnectionStateEnum::Initialized(init))
								}
								other => ((), other),
							})
						}
						_ => {}
					}
				}
				Ok(_) => {}
			}
		},
	)
}

fn make_on_close_closure(
	connection: Weak<Mutex<SocketConnectionStateCarrier>>,
	handled_close_receiver: oneshot::Receiver<()>,
) -> wasm_bindgen::prelude::Closure<dyn Fn(CloseEvent)> {
	// needed to make the closure Fn instead of FnMut which is required for the Closure trait bound
	let handled_close_channel = std::sync::Mutex::new(handled_close_receiver);
	wasm_bindgen::prelude::Closure::<dyn Fn(CloseEvent)>::new(move |e: CloseEvent| {
		match handled_close_channel.lock().unwrap().try_recv() {
			Ok(()) | Err(oneshot::error::TryRecvError::Closed) => {
				// connection was closed due to a graceful state transition, do not attempt to reconnect
				// it is annoying that this is the best way I have of handling this
				return;
			}
			Err(oneshot::error::TryRecvError::Empty) => {}
		}
		let Some(connection) = connection.upgrade() else {
			return;
		};
		let mut guard = connection.blocking_lock();
		guard.with_owned(|state| {
			let (init, broadcaster) = match state {
				SocketConnectionStateEnum::Initialized(init) => {
					init.listener_manager
						.handle_event(&SocketEvent::Reconnecting);
					(init, tokio::sync::broadcast::channel(1).0)
				}
				SocketConnectionStateEnum::Initializing(init, broadcaster) => (init, broadcaster),
				other => {
					return ((), other);
				}
			};

			log::error!(
				"WebSocket closed: code {}, reason: {}",
				e.code(),
				e.reason()
			);

			start_reconnect_task(
				Arc::downgrade(&connection),
				init.config.reconnect_delay,
				init.config.max_reconnect_delay,
			);

			(
				(),
				SocketConnectionStateEnum::Reconnecting(
					UninitSocketConnection {
						config: init.config,
					},
					init.listener_manager,
					broadcaster,
				),
			)
		});
	})
}

fn make_on_open_closure(
	ws: std::rc::Weak<WebSocket>,
	ping_interval: Duration,
	interval_change_receiver: tokio::sync::mpsc::UnboundedReceiver<Duration>,
) -> wasm_bindgen::prelude::Closure<dyn Fn(web_sys::Event)> {
	// workaround for Closure needing Fn instead of FnMut/FnOnce
	let fn_once = Cell::new(Some(move || {
		start_ping_task(ws, ping_interval, interval_change_receiver)
	}));

	wasm_bindgen::prelude::Closure::<dyn Fn(web_sys::Event)>::new(move |_e: web_sys::Event| {
		if let Some(f) = fn_once.take() {
			f();
		} else {
			log::error!("WebSocket onopen called multiple times");
		}
	})
}

enum SocketUninitOrReconnecting {
	Reconnecting(UninitSocketConnection, ListenerManager),
	Uninit(UninitSocketConnection),
}

fn spawn_close_task(close_receiver: oneshot::Receiver<()>, websocket: Rc<WebSocket>) {
	runtime::spawn_local(async move {
		// don't care about errors or other edge cases, if we get an error, we try to close anyway
		let _ = close_receiver.await;
		let _ = websocket.close();
	});
}

fn setup_websocket_thread(
	config: WebsocketConfig,
	listener_manager: ListenerManager,
	state: Weak<Mutex<SocketConnectionStateCarrier>>,
	receivers: WSReceivers,
) -> Result<(ListenerManager, WebsocketConfig), (Error, UninitSocketConnection, ListenerManager)> {
	let websocket_url = format!(
		"{}?EIO=3&transport=websocket&t={}",
		&config.ws_url,
		chrono::Utc::now().timestamp_millis()
	);

	let ws = match web_sys::WebSocket::new(&websocket_url).map_err(|e| {
		Error::custom(
			crate::ErrorKind::Server,
			format!("Failed to create WebSocket connection: {:?}", e),
		)
	}) {
		Ok(ws) => ws,
		Err(e) => {
			return Err((e, UninitSocketConnection { config }, listener_manager));
		}
	};
	let (interval_change_sender, interval_change_receiver) = tokio::sync::mpsc::unbounded_channel();

	let ws = Rc::new(ws);

	ws.set_onmessage(Some(
		make_on_message_closure(Rc::downgrade(&ws), interval_change_sender, state.clone())
			.into_js_value()
			.unchecked_ref(),
	));

	ws.set_onclose(Some(
		make_on_close_closure(state, receivers.handled_close_receiver)
			.into_js_value()
			.unchecked_ref(),
	));

	ws.set_onopen(Some(
		make_on_open_closure(
			Rc::downgrade(&ws),
			config.ping_interval,
			interval_change_receiver,
		)
		.into_js_value()
		.unchecked_ref(),
	));

	spawn_close_task(receivers.close_receiver, ws);

	Ok((listener_manager, config))
}

async fn spawn_websocket_thread(
	config: WebsocketConfig,
	listener_manager: ListenerManager,
	state: Weak<Mutex<SocketConnectionStateCarrier>>,
	receivers: WSReceivers,
) -> Result<(ListenerManager, WebsocketConfig), (Error, UninitSocketConnection, ListenerManager)> {
	let (result_sender, result_receiver) = oneshot::channel();
	runtime::spawn(|| {
		let result = setup_websocket_thread(config, listener_manager, state, receivers);

		let _ = result_sender.send(result);
	});
	result_receiver.await.unwrap_throw()
}

struct WSReceivers {
	handled_close_receiver: oneshot::Receiver<()>,
	close_receiver: oneshot::Receiver<()>,
}

/// Sets up the WebSocket connection and returns a receiver that will be notified when the connection is authenticated
/// or dropped if auth fails
async fn setup_websocket(
	state: Weak<Mutex<SocketConnectionStateCarrier>>,
	maybe_init_state: SocketUninitOrReconnecting,
) -> Result<InitSocketConnection, (Error, UninitSocketConnection, ListenerManager)> {
	let (config, listener_manager) = match maybe_init_state {
		SocketUninitOrReconnecting::Uninit(uninit) => (
			uninit.config,
			ListenerManager {
				listeners: Vec::new(),
			},
		),
		SocketUninitOrReconnecting::Reconnecting(uninit, listener_manager) => {
			(uninit.config, listener_manager)
		}
	};

	let (handled_close_sender, handled_close_receiver) = oneshot::channel();
	let (close_sender, close_receiver) = oneshot::channel();

	let (listener_manager, config) = spawn_websocket_thread(
		config,
		listener_manager,
		state,
		WSReceivers {
			handled_close_receiver,
			close_receiver,
		},
	)
	.await?;

	let ws_handle = Arc::new(WebSocketHandle {
		handled_close_sender,
		close_sender,
	});

	Ok(InitSocketConnection {
		ws_handle,
		listener_manager,
		config,
	})
}

impl SocketConnectionState {
	pub(crate) fn new(client: Arc<AuthClient>, config: SocketConfig) -> Self {
		Self(Arc::new(Mutex::new(SocketConnectionStateCarrier::new(
			SocketConnectionStateEnum::Uninintialized(UninitSocketConnection {
				config: WebsocketConfig {
					client,
					ws_url: format!(
						"{}://{}/socket.io/",
						if config.tls { "wss" } else { "ws" },
						&config.socket_url
					),
					reconnect_delay: RECONNECT_DELAY,
					max_reconnect_delay: MAX_RECONNECT_DELAY,
					ping_interval: PING_INTERVAL,
				},
			}),
		))))
	}

	fn inner_add_listener<'a>(
		&self,
		mut guard: MutexGuard<'a, SocketConnectionStateEnum>,
		listener: EventListener,
	) -> AddListenerReturn<'a> {
		match &mut *guard {
			SocketConnectionStateEnum::Initialized(conn) => AddListenerReturn::Success(
				conn.listener_manager.add_listener(listener, self.clone()),
			),
			_ => AddListenerReturn::Fail(guard, listener),
		}
	}

	async fn add_listener(
		&self,
		event_types: Option<HashSet<String>>,
		callback: EventListenerCallback,
	) -> Result<EventListenerHandle, Error> {
		// clippy thinks I'm holding the lock through the await point if I don't wrap it in a block
		let (handle, receiver) = {
			let mut guard = self.0.lock().await;
			guard
				.async_with_owned(async |state| match state {
					SocketConnectionStateEnum::Initialized(mut conn) => (
						Ok((
							conn.listener_manager.add_listener(
								EventListener {
									event_types,
									callback,
								},
								self.clone(),
							),
							None,
						)),
						SocketConnectionStateEnum::Initialized(conn),
					),
					SocketConnectionStateEnum::Uninintialized(uninit) => match setup_websocket(
						Arc::downgrade(&self.0),
						SocketUninitOrReconnecting::Uninit(uninit),
					)
					.await
					{
						Ok(mut init) => {
							let broadcaster = tokio::sync::broadcast::channel(1).0;
							(
								Ok((
									init.listener_manager.add_listener(
										EventListener {
											event_types,
											callback,
										},
										self.clone(),
									),
									Some(broadcaster.subscribe()),
								)),
								SocketConnectionStateEnum::Initializing(init, broadcaster),
							)
						}
						Err((e, uninit, _)) => {
							(Err(e), SocketConnectionStateEnum::Uninintialized(uninit))
						}
					},
					SocketConnectionStateEnum::Reconnecting(
						uninit,
						mut listener_manager,
						broadcaster,
					) => (
						Ok((
							listener_manager.add_listener(
								EventListener {
									event_types,
									callback,
								},
								self.clone(),
							),
							Some(broadcaster.subscribe()),
						)),
						SocketConnectionStateEnum::Reconnecting(
							uninit,
							listener_manager,
							broadcaster,
						),
					),
					SocketConnectionStateEnum::Initializing(
						mut init_socket_connection,
						broadcaster,
					) => (
						Ok((
							init_socket_connection.listener_manager.add_listener(
								EventListener {
									event_types,
									callback,
								},
								self.clone(),
							),
							Some(broadcaster.subscribe()),
						)),
						SocketConnectionStateEnum::Initializing(
							init_socket_connection,
							broadcaster,
						),
					),
				})
				.await?
		};

		let fut_result = match receiver {
			None => return Ok(handle),
			Some(mut receiver) => receiver.recv().await.map_err(|_| ()),
		};

		// wait for auth to succeed
		match fut_result {
			Ok(true) => Ok(handle),
			Ok(false) | Err(_) => Err(Error::custom(
				crate::ErrorKind::Unauthenticated,
				"WebSocket authentication failed",
			)),
		}
	}

	// Only called when there is a listener being dropped and therefore a cleanup is needed
	async fn cleanup(&mut self) {
		let mut guard = self.0.lock().await;
		guard.with_owned(|state| match state {
			SocketConnectionStateEnum::Initialized(mut init) => {
				init.listener_manager.cleanup();
				if init.listener_manager.listeners.is_empty() {
					(
						(),
						SocketConnectionStateEnum::Uninintialized(UninitSocketConnection {
							config: init.config,
						}),
					)
				} else {
					((), SocketConnectionStateEnum::Initialized(init))
				}
			}
			SocketConnectionStateEnum::Initializing(mut init, broadcaster) => {
				init.listener_manager.cleanup();
				if init.listener_manager.listeners.is_empty() {
					(
						(),
						SocketConnectionStateEnum::Uninintialized(UninitSocketConnection {
							config: init.config,
						}),
					)
				} else {
					(
						(),
						SocketConnectionStateEnum::Initializing(init, broadcaster),
					)
				}
			}
			SocketConnectionStateEnum::Reconnecting(uninit, mut listener_manager, broadcaster) => {
				listener_manager.cleanup();
				if listener_manager.listeners.is_empty() {
					((), SocketConnectionStateEnum::Uninintialized(uninit))
				} else {
					(
						(),
						SocketConnectionStateEnum::Reconnecting(
							uninit,
							listener_manager,
							broadcaster,
						),
					)
				}
			}
			SocketConnectionStateEnum::Uninintialized(uninit) => {
				((), SocketConnectionStateEnum::Uninintialized(uninit))
			}
		})
	}
}

struct WebsocketConfig {
	client: Arc<AuthClient>,
	ws_url: String,
	reconnect_delay: Duration,
	max_reconnect_delay: Duration,
	ping_interval: Duration,
}

struct UninitSocketConnection {
	config: WebsocketConfig,
}

struct InitSocketConnection {
	config: WebsocketConfig,

	ws_handle: Arc<WebSocketHandle>,
	listener_manager: ListenerManager,
}

struct WebSocketHandle {
	handled_close_sender: oneshot::Sender<()>,
	close_sender: oneshot::Sender<()>,
}

struct EventListener {
	event_types: Option<HashSet<String>>,
	callback: EventListenerCallback,
}

#[wasm_bindgen::prelude::wasm_bindgen]
pub struct EventListenerHandle {
	// we use ManuallyDrop here so that we can drop the Arc before we call cleanup on state
	my_listener: ManuallyDrop<Arc<EventListener>>,
	state: ManuallyDrop<SocketConnectionState>,
}

impl Drop for EventListenerHandle {
	fn drop(&mut self) {
		// SAFETY: we are the only ones with access to my_listener
		// this function is called exactly once when the handle is dropped
		std::mem::drop(unsafe { ManuallyDrop::take(&mut self.my_listener) });

		// SAFETY: we are the only ones with access to my_listener
		// this function is called exactly once when the handle is dropped
		let mut state = unsafe { ManuallyDrop::take(&mut self.state) };
		runtime::spawn_local(async move {
			state.cleanup().await;
		});
	}
}

struct ListenerManager {
	listeners: Vec<Weak<EventListener>>,
}

impl ListenerManager {
	fn add_listener(
		&mut self,
		listener: EventListener,
		state: SocketConnectionState,
	) -> EventListenerHandle {
		let my_listener = Arc::new(listener);
		self.listeners.push(Arc::downgrade(&my_listener));
		EventListenerHandle {
			my_listener: ManuallyDrop::new(my_listener),
			state: ManuallyDrop::new(state),
		}
	}

	fn handle_event(&self, event: &SocketEvent<'_>) {
		for weak_ref in self.listeners.iter() {
			if let Some(listener) = weak_ref.upgrade()
				&& listener
					.event_types
					.as_ref()
					.is_none_or(|set| set.contains(event.event_type()))
			{
				(listener.callback)(event);
			}
		}
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

	pub async fn is_socket_connected(&self) -> bool {
		let guard = self.socket_connection.0.lock().await;
		matches!(*guard.borrow(), SocketConnectionStateEnum::Initialized(_))
	}

	// we need to expose this for v3 because most of the returned events are encrypted
	// and we need to decrypt them, and we do not have enough information to do that purely in the rust sdk
	pub async fn decrypt_meta(&self, encrypted: &EncryptedString<'_>) -> Result<String, Error> {
		do_cpu_intensive(|| {
			self.crypter()
				.blocking_decrypt_meta(encrypted)
				.context("public decrypt_meta")
		})
		.await
	}
}

#[cfg(feature = "wasm-full")]
mod js_impl {
	use std::borrow::Cow;

	use filen_types::{api::v3::socket::SocketEvent, crypto::EncryptedString, traits::CowHelpers};
	use wasm_bindgen::JsValue;
	use web_sys::js_sys;

	use crate::{
		Error,
		auth::JsClient,
		runtime::{self, do_on_commander},
		socket::wasm::EventListenerHandle,
	};

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
	)]
	impl JsClient {
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "addSocketListener")
		)]
		pub async fn add_socket_listener(
			&self,
			#[wasm_bindgen(unchecked_param_type = "SocketEventType[] | null")] event_types: Option<
				Vec<String>,
			>,
			#[wasm_bindgen(unchecked_param_type = "(event: SocketEvent) => void")]
			listener: js_sys::Function,
		) -> Result<EventListenerHandle, Error> {
			let (sender, mut receiver) =
				tokio::sync::mpsc::unbounded_channel::<SocketEvent<'static>>();

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

			let callback = Box::new(move |event: &SocketEvent<'_>| {
				// let event = event.to_owned();
				let _ = sender.send(event.as_borrowed_cow().into_owned_cow());
			});

			let this = self.inner();
			do_on_commander(move || async move {
				this.add_socket_listener(event_types.map(|v| v.into_iter().collect()), callback)
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
			do_on_commander(move || async move { this.is_socket_connected().await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "decryptMeta")
		)]
		pub async fn decrypt_meta(
			&self,
			#[wasm_bindgen(unchecked_param_type = "EncryptedString")] encrypted: String,
		) -> Result<String, Error> {
			let this = self.inner();

			do_on_commander(move || async move {
				let encrypted = EncryptedString(Cow::Owned(encrypted));
				this.decrypt_meta(&encrypted).await
			})
			.await
		}
	}
}
