use serde::Deserialize;
use std::{
	borrow::Cow,
	collections::HashSet,
	fmt::Write,
	mem::ManuallyDrop,
	rc::Rc,
	sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
	time::Duration,
};
use tokio::sync::oneshot;
use wasm_bindgen::JsCast;
use web_sys::CloseEvent;

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
	util::{MaybeArc, MaybeArcWeak},
};

const SOCKET_HOST: &str = "socket.filen.io";
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const PING_INTERVAL: Duration = Duration::from_secs(15);

pub type EventListenerCallback = Box<dyn Fn(&SocketEvent<'_>) + 'static>;
#[derive(Clone)]
pub(crate) struct SocketConnectionState(MaybeArc<RwLock<SocketConnectionStateCarrier>>);

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
	Fail(
		RwLockWriteGuard<'a, SocketConnectionStateEnum>,
		EventListener,
	),
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

fn send_event(
	ws: &WebSocketHandle,
	event: &str,
	data: Option<&impl serde::Serialize>,
) -> Result<(), Error> {
	let payload = EventAndOptionalData { event, data };
	let mut packet = Vec::new();
	packet.push(PacketType::Message as u8);
	packet.push(MessageType::Event as u8);
	serde_json::to_writer(&mut packet, &payload)
		.expect("string message serialization should not fail");

	// SAFETY: we just serialized valid UTF-8
	// and both PacketType and MessageType are valid ASCII
	// so the resulting byte array is valid UTF-8
	let packet = unsafe { std::str::from_utf8_unchecked(&packet) };
	ws.send_and_log_error(packet);
	Ok(())
}

fn handle_handshake(ws: &WebSocketHandle, msg: &str) -> Result<(), Error> {
	use filen_types::api::v3::socket::{HandShake, MessageType, PacketType};

	let handshake: HandShake = serde_json::from_str(msg).map_err(|e| {
		Error::custom(
			crate::ErrorKind::Server,
			format!("Failed to parse handshake message: {}", e),
		)
	})?;
	// don't care if the send fails, the connection will be closed soon if it does
	let _ = ws
		.interval_change_sender
		.send(Duration::from_millis(handshake.ping_interval));
	let raw_payload = [PacketType::Message as u8, MessageType::Event as u8];
	ws.send_and_log_error(str::from_utf8(&raw_payload).unwrap());
	Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthMessage<'a> {
	api_key: Cow<'a, str>,
}

fn parse_message(msg: &str) -> Result<Option<(String, Option<serde_json::Value>)>, Error> {
	let message_type = msg.bytes().next().ok_or_else(|| {
		Error::custom(
			crate::ErrorKind::Server,
			"Empty message received over WebSocket",
		)
	})?;

	match MessageType::try_from(message_type) {
		Err(e) => {
			log::error!("Invalid message type: {}", e);
			return Ok(None);
		}
		Ok(MessageType::Event) => {
			// continue
		}
		Ok(_) => {
			// ignore other message types for now
			return Ok(None);
		}
	}

	let json_value: serde_json::Value = serde_json::from_str(&msg[1..]).map_err(|e| {
		Error::custom(
			crate::ErrorKind::Server,
			format!("Failed to parse WebSocket message: {}", e),
		)
	})?;

	if let serde_json::Value::Array(arr) = json_value
		&& let mut arr_iter = arr.into_iter()
		&& let Some(event_name) = arr_iter.next()
	{
		let serde_json::Value::String(event_name) = event_name else {
			return Err(Error::custom(
				crate::ErrorKind::Server,
				format!("Invalid event name in WebSocket message: {:?}", event_name),
			));
		};

		Ok(Some((event_name, arr_iter.next())))
	} else {
		Ok(None)
	}
}

fn try_handle_event(
	event_name: &str,
	data: Option<serde_json::Value>,
	listener_manager: &ListenerManager,
) -> Result<(), Error> {
	let event_name = normalize_event_name(event_name);
	let mut serialized_value = serde_json::Map::with_capacity(2);
	serialized_value.insert(
		"type".to_string(),
		serde_json::Value::String(event_name.into_owned()),
	);
	if let Some(data) = data {
		serialized_value.insert("data".to_string(), data);
	}
	let serialized_value = serde_json::Value::Object(serialized_value);
	let socket_event = SocketEvent::deserialize(&serialized_value).map_err(|e| {
		Error::custom(
			crate::ErrorKind::Server,
			format!("Failed to parse WebSocket event: {}", e),
		)
	})?;

	listener_manager.handle_event(&socket_event);
	Ok(())
}

fn handle_unauthed_message(
	msg: &str,
	connection: InitSocketConnection,
	broadcaster: tokio::sync::broadcast::Sender<bool>,
) -> Result<SocketConnectionStateEnum, (SocketConnectionStateEnum, Error)> {
	match parse_message(msg) {
		Ok(Some((event_name, data))) => match event_name.as_str() {
			"authed" => {
				if data.as_ref().and_then(|d| d.as_bool()) == Some(false) {
					let client = &*connection.config.client;
					let api_key = client.get_api_key();
					let msg = Some(&AuthMessage {
						api_key: Cow::Borrowed(&api_key.0),
					});
					if let Err(e) = send_event(&connection.ws_handle, "auth", msg) {
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
			other => match other {
				"authFailed" => {
					log::error!("WebSocket authentication failed");
					let _ = broadcaster.send(false);
					// first notify listeners, then drop them
					match try_handle_event(other, data, &connection.listener_manager) {
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
					}
				}
				"authSuccess" => {
					let _ = broadcaster.send(true);
					match try_handle_event(other, data, &connection.listener_manager) {
						Ok(()) => Ok(SocketConnectionStateEnum::Initialized(connection)),
						Err(e) => Err((
							SocketConnectionStateEnum::Uninintialized(UninitSocketConnection {
								config: connection.config,
							}),
							e,
						)),
					}
				}
				other => match try_handle_event(other, data, &connection.listener_manager) {
					Ok(()) => Ok(SocketConnectionStateEnum::Initializing(
						connection,
						broadcaster,
					)),
					Err(e) => Err((
						SocketConnectionStateEnum::Initializing(connection, broadcaster),
						e,
					)),
				},
			},
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

fn handle_authed_message(msg: &str, connection: &InitSocketConnection) -> Result<(), Error> {
	if let Some((event_name, data)) = parse_message(msg)? {
		match event_name.as_str() {
			"authed" => {
				if data.as_ref().and_then(|d| d.as_bool()) == Some(false) {
					let client = &*connection.config.client;
					let api_key = client.get_api_key();
					let msg = Some(&AuthMessage {
						api_key: Cow::Borrowed(&api_key.0),
					});

					send_event(&connection.ws_handle, "auth", msg)?;
				}
			}
			other => {
				try_handle_event(other, data, &connection.listener_manager)?;
			}
		}
	}
	Ok(())
}

fn normalize_event_name(name: &str) -> Cow<'_, str> {
	let dashes = name
		.bytes()
		.enumerate()
		.rev()
		.filter_map(|(i, c)| if c == b'-' { Some(i) } else { None });
	let mut cow = Cow::Borrowed(name);
	for i in dashes {
		let mut_string = cow.to_mut();

		mut_string.remove(i);
		// SAFETY: we potentially convert a single byte ASCII lowercase character to uppercase
		// which is still valid UTF-8 and we are not changing the length of the string
		let mut_vec = unsafe { mut_string.as_mut_vec() };
		if let Some(c) = mut_vec.get(i)
			&& c.is_ascii_lowercase()
		{
			mut_vec[i] = c.to_ascii_uppercase();
		}
	}
	cow
}

fn start_ping_task(
	ws: &MaybeArc<WebSocketHandle>,
	mut interval: Duration,
	mut interval_change_receiver: tokio::sync::mpsc::UnboundedReceiver<Duration>,
) {
	let ws = MaybeArc::downgrade(ws);
	let mut last_update = wasmtimer::std::Instant::now();
	wasm_bindgen_futures::spawn_local(async move {
		let mut timestamp_string = String::new();
		let ping_packet = [PacketType::Ping as u8];
		loop {
			tokio::select! {
				biased;
				Some(new_interval) = interval_change_receiver.recv() => {
					interval = new_interval;
				}
				_ = wasmtimer::tokio::sleep(interval.saturating_sub(last_update.elapsed())) => {
					if let Some(ws) = ws.upgrade() {
						ws.send(str::from_utf8(&ping_packet).unwrap())
							.unwrap_or_else(|e| {
								log::error!("Failed to send ping over WebSocket: {:?}", e);
							});
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
					} else {
						// automatic cleanup when the WebSocket is dropped
						break;
					}
				}
			}
		}
	});
}

fn start_reconnect_task(
	connection: SocketConnectionState,
	mut reconnect_delay: Duration,
	max_reconnect_delay: Duration,
) {
	let mut last_update = wasmtimer::std::Instant::now();
	wasm_bindgen_futures::spawn_local(async move {
		loop {
			// need this to be a separate block so that the write_guard is dropped before the await point
			// clippy still complains if I std::mem::drop it manually
			let should_break = {
				let mut write_guard = connection.get_write_guard();
				write_guard.with_owned(|state| match state {
					SocketConnectionStateEnum::Reconnecting(
						init_socket_connection,
						listener_manger,
						broadcaster,
					) => {
						match setup_websocket(
							&connection,
							SocketUninitOrReconnecting::Reconnecting(
								init_socket_connection,
								listener_manger,
							),
						) {
							Ok(connection) => (
								true,
								SocketConnectionStateEnum::Initializing(connection, broadcaster),
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
	connection: SocketConnectionState,
) -> wasm_bindgen::prelude::Closure<dyn Fn(web_sys::MessageEvent)> {
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
					let read_guard = connection.get_read_guard();
					if let SocketConnectionStateEnum::Initializing(conn, _) = read_guard.borrow() {
						handle_handshake(&conn.ws_handle, &msg[1..]).unwrap_or_else(|e| {
							log::error!("Failed to handle handshake: {}", e);
						});
					}
				}
				Ok(PacketType::Message) => {
					let read_guard = connection.get_read_guard();
					match read_guard.borrow() {
						SocketConnectionStateEnum::Initialized(conn) => {
							handle_authed_message(&msg[1..], conn).unwrap_or_else(|e| {
								log::error!("Failed to handle message: {}", e);
							});
						}
						SocketConnectionStateEnum::Initializing(_, _) => {
							std::mem::drop(read_guard);
							connection
								.get_write_guard()
								.with_owned(|state| match state {
									SocketConnectionStateEnum::Initializing(conn, broadcaster) => (
										(),
										handle_unauthed_message(&msg[1..], conn, broadcaster)
											.unwrap_or_else(|(new_state, e)| {
												log::error!("Failed to handle message: {}", e);
												new_state
											}),
									),
									SocketConnectionStateEnum::Initialized(init) => {
										handle_authed_message(&msg[1..], &init).unwrap_or_else(
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
	connection: SocketConnectionState,
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
		let mut write_guard = connection.get_write_guard();
		write_guard.with_owned(|state| {
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
				connection.clone(),
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

enum SocketUninitOrReconnecting {
	Reconnecting(UninitSocketConnection, ListenerManager),
	Uninit(UninitSocketConnection),
}

/// Sets up the WebSocket connection and returns a receiver that will be notified when the connection is authenticated
/// or dropped if auth fails
fn setup_websocket(
	state: &SocketConnectionState,
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

	ws.set_onmessage(Some(
		make_on_message_closure(state.clone())
			.into_js_value()
			.unchecked_ref(),
	));

	let (handled_close_sender, handled_close_receiver) = oneshot::channel();

	ws.set_onclose(Some(
		make_on_close_closure(state.clone(), handled_close_receiver)
			.into_js_value()
			.unchecked_ref(),
	));

	let (interval_change_sender, interval_change_receiver) = tokio::sync::mpsc::unbounded_channel();

	let ws_handle = Rc::new(WebSocketHandle {
		wasm: ws,
		handled_close_sender,
		interval_change_sender,
	});

	start_ping_task(&ws_handle, config.ping_interval, interval_change_receiver);

	Ok(InitSocketConnection {
		ws_handle,
		listener_manager,
		config,
	})
}

impl SocketConnectionState {
	pub(crate) fn new(client: Arc<AuthClient>, config: SocketConfig) -> Self {
		Self(MaybeArc::new(RwLock::new(
			SocketConnectionStateCarrier::new(SocketConnectionStateEnum::Uninintialized(
				UninitSocketConnection {
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
				},
			)),
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

	fn get_write_guard(&self) -> RwLockWriteGuard<'_, SocketConnectionStateCarrier> {
		self.0.write().unwrap_or_else(|e| e.into_inner())
	}

	fn get_read_guard(&self) -> RwLockReadGuard<'_, SocketConnectionStateCarrier> {
		self.0.read().unwrap_or_else(|e| e.into_inner())
	}

	async fn add_listener(
		&self,
		event_types: Option<HashSet<String>>,
		callback: EventListenerCallback,
	) -> Result<EventListenerHandle, Error> {
		// clippy thinks I'm holding the lock through the await point if I don't wrap it in a block
		let (handle, receiver) = {
			let mut write_guard = self.get_write_guard();
			write_guard.with_owned(|state| match state {
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
				SocketConnectionStateEnum::Uninintialized(uninit) => {
					match setup_websocket(self, SocketUninitOrReconnecting::Uninit(uninit)) {
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
					}
				}
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
					SocketConnectionStateEnum::Reconnecting(uninit, listener_manager, broadcaster),
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
					SocketConnectionStateEnum::Initializing(init_socket_connection, broadcaster),
				),
			})?
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
	fn cleanup(&mut self) {
		let mut write_guard = self.get_write_guard();
		write_guard.with_owned(|state| match state {
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

	ws_handle: MaybeArc<WebSocketHandle>,
	listener_manager: ListenerManager,
}

struct WebSocketHandle {
	interval_change_sender: tokio::sync::mpsc::UnboundedSender<Duration>,
	handled_close_sender: oneshot::Sender<()>,
	wasm: web_sys::WebSocket,
}

impl Drop for WebSocketHandle {
	fn drop(&mut self) {
		let _ = self.wasm.close();
	}
}

impl WebSocketHandle {
	fn send_and_log_error(&self, msg: &str) {
		if let Err(e) = self.send(msg) {
			log::error!("Failed to send WebSocket message: {}", e);
		}
	}

	fn send(&self, msg: &str) -> Result<(), Error> {
		self.wasm.send_with_str(msg).map_err(|e| {
			Error::custom(
				crate::ErrorKind::Server,
				format!("Failed to send WebSocket message: {:?}", e),
			)
		})
	}
}

struct EventListener {
	event_types: Option<HashSet<String>>,
	callback: EventListenerCallback,
}

#[wasm_bindgen::prelude::wasm_bindgen]
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

	pub fn is_socket_connected(&self) -> bool {
		let read_guard = self.socket_connection.get_read_guard();
		matches!(
			*read_guard.borrow(),
			SocketConnectionStateEnum::Initialized(_)
		)
	}

	// we need to expose this for v3 because most of the returned events are encrypted
	// and we need to decrypt them, and we do not have enough information to do that purely in the rust sdk
	pub fn decrypt_meta(&self, encrypted: &EncryptedString) -> Result<String, Error> {
		self.crypter()
			.decrypt_meta(encrypted)
			.context("public decrypt_meta")
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod js_impl {
	use filen_types::{api::v3::socket::SocketEvent, crypto::EncryptedString};
	use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
	use web_sys::js_sys;

	use crate::{Error, auth::Client, sockets::EventListenerHandle};

	#[wasm_bindgen]
	impl Client {
		#[wasm_bindgen(js_name = "addSocketListener")]
		pub async fn js_add_socket_listener(
			&self,
			#[wasm_bindgen(unchecked_param_type = "SocketEventType[] | null")] event_types: Option<
				Vec<String>,
			>,
			#[wasm_bindgen(unchecked_param_type = "(event: SocketEvent) => void")]
			listener: js_sys::Function,
		) -> Result<EventListenerHandle, Error> {
			let callback = Box::new(move |event: &SocketEvent<'_>| {
				let serializer = serde_wasm_bindgen::Serializer::new()
					.serialize_maps_as_objects(true)
					.serialize_large_number_types_as_bigints(true);

				let _ = listener.call1(
					&JsValue::UNDEFINED,
					&serde::Serialize::serialize(&event, &serializer)
						.expect("failed to serialize event to JsValue (should be impossible)"),
				);
			});
			self.add_socket_listener(event_types.map(|v| v.into_iter().collect()), callback)
				.await
		}

		#[wasm_bindgen(js_name = "isSocketConnected")]
		pub fn js_is_socket_connected(&self) -> bool {
			self.is_socket_connected()
		}

		#[wasm_bindgen(js_name = "decryptMeta")]
		pub fn js_decrypt_meta(
			&self,
			#[wasm_bindgen(unchecked_param_type = "EncryptedString")] encrypted: EncryptedString,
		) -> Result<String, Error> {
			self.decrypt_meta(&encrypted)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn camelify_name_from_kebab() {
		assert_eq!(normalize_event_name("file-rename"), "fileRename");
		assert_eq!(
			normalize_event_name("file-archive-restored"),
			"fileArchiveRestored"
		);
		assert_eq!(normalize_event_name("auth-success"), "authSuccess");
		assert_eq!(normalize_event_name("simpleevent"), "simpleevent");
		assert_eq!(normalize_event_name("simpleEvent"), "simpleEvent");
		assert_eq!(normalize_event_name("-----"), "");
		assert_eq!(normalize_event_name("-----a"), "A");
		assert_eq!(normalize_event_name("-----aaa"), "Aaa");
	}
}
