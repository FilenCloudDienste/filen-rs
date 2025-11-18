use std::{borrow::Cow, sync::Arc, time::Duration};

use filen_types::{
	api::v3::socket::{HandShake, MessageType, PacketType, SocketEvent},
	crypto::EncryptedString,
};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::WebSocketStream;
use tungstenite::{ClientRequestBuilder, Message, Utf8Bytes};

use crate::{
	Error, ErrorKind,
	auth::{Client, http::AuthClient},
	crypto::shared::MetaCrypter,
	error::ResultExt,
	runtime::do_cpu_intensive,
};

use super::shared::*;

impl Client {
	pub async fn add_event_listener(
		&self,
		callback: EventListenerCallback,
		event_types: Option<Vec<Cow<'static, str>>>,
	) -> Result<ListenerHandle, Error> {
		let request_sender = {
			let mut socket_handle = self.socket_handle.lock().unwrap();
			socket_handle.get_request_sender(self.arc_client())
		};
		request_sender
			.add_event_listener(callback, event_types)
			.await
	}

	pub fn is_socket_connected(&self) -> bool {
		self.socket_handle.lock().unwrap().request_sender.is_some()
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

#[derive(Default)]
pub(crate) struct WebSocketHandle {
	// closes websocket thread on drop
	request_sender: Option<tokio::sync::mpsc::Sender<SocketRequest>>,
}

struct RequestSender(tokio::sync::mpsc::Sender<SocketRequest>);

impl RequestSender {
	async fn add_event_listener(
		self,
		callback: EventListenerCallback,
		event_types: Option<Vec<Cow<'static, str>>>,
	) -> Result<ListenerHandle, Error> {
		let (id_sender, id_receiver) = tokio::sync::oneshot::channel();
		let (canceller, cancel_receiver) = tokio::sync::oneshot::channel();
		let request = SocketRequest::AddListener {
			callback,
			event_types,
			id_sender,
			cancel_receiver,
		};

		let guard = ListenerRegisterGuard {
			receiver: id_receiver,
			request_sender: Some(self.0),
			canceller: Some(canceller),
		};
		guard
			.request_sender
			.as_ref()
			.expect("we set this above")
			.send(request)
			.await
			.map_err(|_| {
				Error::custom(ErrorKind::InvalidState, "websocket thread has been closed")
			})?;
		guard.await
	}
}

impl WebSocketHandle {
	fn get_request_sender(&mut self, client: Arc<AuthClient>) -> RequestSender {
		RequestSender(match &self.request_sender {
			Some(s) => s.clone(),
			None => {
				let request_sender = spawn_websocket_thread(client);
				self.request_sender = Some(request_sender.clone());
				request_sender
			}
		})
	}
}

enum SocketRequest {
	AddListener {
		id_sender: tokio::sync::oneshot::Sender<Result<u64, Error>>,
		cancel_receiver: tokio::sync::oneshot::Receiver<()>,
		callback: EventListenerCallback,
		event_types: Option<Vec<Cow<'static, str>>>,
	},
	RemoveListener(u64),
}

fn guarantee_send_remove_listener(sender: tokio::sync::mpsc::Sender<SocketRequest>, idx: u64) {
	match sender.try_send(SocketRequest::RemoveListener(idx)) {
		Ok(_) => {}
		Err(e) if matches!(e, tokio::sync::mpsc::error::TrySendError::Full(_)) => {
			// channel is full, spawn a task to handle the removal
			let request = e.into_inner();
			match tokio::runtime::Handle::try_current() {
				Ok(runtime_handle) => {
					runtime_handle.spawn(async move {
						let _ = sender.send(request).await;
					});
				}
				Err(_) => {
					// No runtime available, spawn a thread to handle the removal
					std::thread::spawn(move || {
						let _ = sender.blocking_send(request);
					});
				}
			}
		}
		Err(_) => {
			// channel closed, nothing we can do
		}
	}
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct ListenerHandle {
	idx: u64,
	remove_listener_sender: Option<tokio::sync::mpsc::Sender<SocketRequest>>,
}

impl Drop for ListenerHandle {
	fn drop(&mut self) {
		if let Some(sender) = self.remove_listener_sender.take() {
			guarantee_send_remove_listener(sender, self.idx);
		}
	}
}

pin_project_lite::pin_project! {
	struct ListenerRegisterGuard {
		#[pin]
		receiver: tokio::sync::oneshot::Receiver<Result<u64, Error>>,
		request_sender: Option<tokio::sync::mpsc::Sender<SocketRequest>>,
		canceller: Option<tokio::sync::oneshot::Sender<()>>,
	}

	impl PinnedDrop for ListenerRegisterGuard {
		fn drop(this: Pin<&mut Self>) {
			let mut this = this.project();

			let Some(request_sender) = this.request_sender.take() else {
				// future completed and we were converted into a ListenerHandle
				return;
			};

			if let Some(canceller) = this.canceller.take() {
				let _ = canceller.send(());
			}

			if let Ok(Ok(id)) = this.receiver.try_recv()
			{
				guarantee_send_remove_listener(request_sender, id);
			}
		}
	}
}

impl Future for ListenerRegisterGuard {
	type Output = Result<ListenerHandle, Error>;

	fn poll(
		self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Self::Output> {
		let this = self.project();
		match this.receiver.poll(cx) {
			std::task::Poll::Ready(Ok(Ok(id))) => std::task::Poll::Ready(Ok(ListenerHandle {
				idx: id,
				remove_listener_sender: this.request_sender.take(),
			})),
			std::task::Poll::Ready(Ok(Err(e))) => std::task::Poll::Ready(Err(e)),
			std::task::Poll::Ready(Err(_)) => std::task::Poll::Ready(Err(Error::custom(
				ErrorKind::InvalidState,
				"websocket thread has been closed",
			))),
			std::task::Poll::Pending => std::task::Poll::Pending,
		}
	}
}

fn handle_message(
	maybe_message: Option<Result<Message, tungstenite::Error>>,
	listeners: &mut ConnectedListenerManager,
) -> Result<(), (bool, Error)> {
	let msg = maybe_message
		.ok_or_else(|| {
			(
				false,
				Error::custom(
					ErrorKind::Server,
					"websocket closed unexpectedly while handling message",
				),
			)
		})?
		.map_err(|e| {
			(
				false,
				Error::custom_with_source(
					ErrorKind::Server,
					e,
					Some("failed to read websocket message"),
				),
			)
		})?;

	let Message::Text(text) = msg else {
		return Err((
			true,
			Error::custom(
				ErrorKind::Server,
				format!(
					"expected text message while handling websocket message, received: {:?}",
					msg
				),
			),
		));
	};

	let mut text_bytes = text.bytes();
	let Some(packet_type) = text_bytes.next() else {
		return Err((
			true,
			Error::custom(ErrorKind::Server, "Empty message received over WebSocket"),
		));
	};
	match PacketType::try_from(packet_type) {
		Err(e) => {
			return Err((
				true,
				Error::custom(ErrorKind::Server, format!("Invalid packet type: {}", e)),
			));
		}
		Ok(PacketType::Message) => {}
		Ok(PacketType::Connect) => {
			return Err((
				true,
				Error::custom(
					ErrorKind::InvalidState,
					"Received unexpected connect packet after initialization",
				),
			));
		}
		Ok(_) => {
			return Ok(());
		}
	}

	let Some(message_type) = text_bytes.next() else {
		return Err((
			true,
			Error::custom(
				ErrorKind::Server,
				"PacketType::Message received with no MessageType",
			),
		));
	};

	match MessageType::try_from(message_type) {
		Err(e) => {
			return Err((
				true,
				Error::custom(ErrorKind::Server, format!("Invalid message type: {}", e)),
			));
		}
		Ok(MessageType::Event) => {
			// continue
		}
		Ok(_) => {
			// ignore other message types for now
			return Ok(());
		}
	}

	let event_str = &text.as_str()[2..];
	if event_str == r#"["authed",true]"# {
		// ignore authed true messages
		return Ok(());
	}
	let event: SocketEvent = serde_json::from_str(event_str).map_err(|e| {
		(
			true,
			Error::custom_with_source(ErrorKind::Conversion, e, Some("deserializing SocketEvent")),
		)
	})?;

	listeners.broadcast_event(&event);
	Ok(())
}

/// Handles a socket request, modifying the listener manager as needed.
///
/// Returns true if there are no more listeners after handling the request.
fn handle_request(request: SocketRequest, listeners: &mut impl ListenerManagerExt) -> bool {
	match request {
		SocketRequest::AddListener {
			id_sender,
			callback,
			cancel_receiver,
			event_types,
		} => {
			listeners.add_listener(
				callback,
				cancel_receiver,
				id_sender,
				event_types.map(|v| v.into_iter()),
			);
		}
		SocketRequest::RemoveListener(idx) => {
			listeners.remove_listener(idx);
		}
	}
	listeners.is_empty()
}

/// Handles the initialized websocket connection, processing incoming messages and managing listeners.
async fn handle_initialized_websocket(
	config: &WebSocketConfig,
	mut streams: WebSocketStreams,
	request_receiver: &mut tokio::sync::mpsc::Receiver<SocketRequest>,
	listeners: &mut ConnectedListenerManager,
) -> bool {
	let ping_task = spawn_ping_task(streams.write, config.ping_interval);
	let mut should_retry = true;

	loop {
		tokio::select! {
			biased;
			request = request_receiver.recv() => {
				let Some(request) = request else {
					// request channel closed, shutting down websocket task
					should_retry = false;
					break;
				};
				should_retry = !handle_request(request, listeners);
				if !should_retry {
					// no more listeners, shutting down websocket task
					break;
				}
			}
			message_result = streams.read.next() => {
				if let Err((should_continue, error)) = handle_message(message_result, listeners) {
					if should_continue {
						log::error!("Error handling WebSocket message: {}", error);
					} else {
						log::error!("Critical error handling WebSocket message: {}, shutting down WebSocket task, will retry", error);
						break;
					}
				}
			}
		}
	}
	ping_task.abort();
	should_retry
}

async fn initialize_websocket(
	config: &mut WebSocketConfig,
	request_receiver: &mut tokio::sync::mpsc::Receiver<SocketRequest>,
	listeners: &mut DisconnectedListenerManager,
) -> Option<WebSocketStreams> {
	let api_key = config
		.client
		.api_key
		.read()
		.unwrap_or_else(|poisoned| poisoned.into_inner())
		.0
		.to_string();

	loop {
		tokio::select! {
			biased;
			request = request_receiver.recv() => {
				let Some(request) = request else {
					// request channel closed, shutting down websocket task
					return None;
				};
				handle_request(request, listeners);
			}
			result = WebSocketStreams::connect(listeners, &api_key) => {
				match result {
					Ok((streams, interval)) => {
						config.ping_interval = interval;
						return Some(streams);
					}
					Err(e) if matches!(e.kind(), ErrorKind::Unauthenticated) => {
						log::error!("WebSocket authentication failed: {}, not retrying", e);
						return None;
					}
					Err(e) => {
						log::error!("Error initializing WebSocket connection: {}, retrying...", e);
						tokio::time::sleep(config.reconnect_delay).await;
					}
				}
			}
		}
	}
}

fn spawn_websocket_thread(client: Arc<AuthClient>) -> tokio::sync::mpsc::Sender<SocketRequest> {
	let (request_sender, mut request_receiver) = tokio::sync::mpsc::channel::<SocketRequest>(16);

	let mut config = WebSocketConfig {
		client,
		reconnect_delay: RECONNECT_DELAY,
		max_reconnect_delay: MAX_RECONNECT_DELAY,
		ping_interval: PING_INTERVAL,
	};

	std::thread::spawn(move || {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.expect("failed to create websocket runtime");

		runtime.block_on(async move {
			let mut disconnected_listeners = DisconnectedListenerManager::new();

			loop {
				let streams = match initialize_websocket(
					&mut config,
					&mut request_receiver,
					&mut disconnected_listeners,
				)
				.await
				{
					Some(s) => s,
					None => break,
				};

				// Promote
				let mut connected_listeners = disconnected_listeners.into_connected();

				let should_retry = handle_initialized_websocket(
					&config,
					streams,
					&mut request_receiver,
					&mut connected_listeners,
				)
				.await;

				if !should_retry {
					break;
				}

				// Demote
				disconnected_listeners = connected_listeners.into_disconnected();

				tokio::time::sleep(config.reconnect_delay).await;
			}
		})
	});

	request_sender
}

struct WebSocketStreams {
	write: futures::stream::SplitSink<
		WebSocketStream<TlsStream<TcpStream>>,
		tokio_tungstenite::tungstenite::Message,
	>,
	read: futures::stream::SplitStream<WebSocketStream<TlsStream<TcpStream>>>,
}

fn build_request(api_key: &str) -> tungstenite::ClientRequestBuilder {
	let uri: tungstenite::http::Uri = format!(
		"{}{}",
		WEBSOCKET_URL_CORE,
		chrono::Utc::now().timestamp_millis()
	)
	.parse()
	.expect("failed to parse websocket URI");

	tungstenite::ClientRequestBuilder::new(uri)
		.with_header("Authorization", format!("Bearer {api_key}"))
}

async fn await_next_message(
	read: &mut futures::stream::SplitStream<WebSocketStream<TlsStream<TcpStream>>>,
) -> Result<Utf8Bytes, Error> {
	let msg = read
		.next()
		.await
		.ok_or_else(|| {
			Error::custom(
				ErrorKind::Server,
				"websocket closed unexpectedly while awaiting message",
			)
		})?
		.map_err(|e| {
			Error::custom(
				ErrorKind::Server,
				format!("failed to read websocket message: {}", e),
			)
		})?;
	match msg {
		Message::Text(text) => Ok(text),
		other => Err(Error::custom(
			ErrorKind::Server,
			format!("expected text message, got {:?} instead", other),
		)),
	}
}

async fn send_next_message(
	write: &mut futures::stream::SplitSink<
		WebSocketStream<TlsStream<TcpStream>>,
		tokio_tungstenite::tungstenite::Message,
	>,
	msg: Utf8Bytes,
) -> Result<(), Error> {
	write.send(Message::Text(msg)).await.map_err(|e| {
		Error::custom(
			ErrorKind::Server,
			format!("failed to send websocket message: {}", e),
		)
	})
}

fn spawn_ping_task(
	mut write: futures::stream::SplitSink<
		WebSocketStream<TlsStream<TcpStream>>,
		tokio_tungstenite::tungstenite::Message,
	>,
	interval_duration: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
	let mut interval = tokio::time::interval(interval_duration);
	tokio::spawn(async move {
		loop {
			interval.tick().await;

			if let Err(e) = write
				.feed(Message::Text(Utf8Bytes::from_static(PING_MESSAGE)))
				.await
			{
				log::error!("Failed to send WebSocket ping: {e}");
				break;
			}
			// is this necessary?
			if let Err(e) = write
				.send(Message::text(format!(
					r#"42["authed", {}]"#,
					chrono::Utc::now().timestamp_millis()
				)))
				.await
			{
				log::error!("Failed to send WebSocket authed ping: {e}");
				break;
			}
		}
	})
}

impl WebSocketStreams {
	async fn connect_into_tls(
		request: ClientRequestBuilder,
	) -> Result<WebSocketStream<TlsStream<TcpStream>>, Error> {
		let (ws_stream, _) = tokio_tungstenite::connect_async(request)
			.await
			.map_err(|e| {
				Error::custom(
					ErrorKind::Server,
					format!("failed to connect to websocket: {}", e),
				)
			})?;

		// make sure we have a TLS stream
		let inner_stream = ws_stream.into_inner();
		let tls_stream = match inner_stream {
			tokio_tungstenite::MaybeTlsStream::Plain(_) => {
				return Err(Error::custom(
					ErrorKind::InvalidState,
					"expected TLS stream, got plain stream",
				));
			}
			tokio_tungstenite::MaybeTlsStream::Rustls(tls_stream) => tls_stream,
			other => {
				return Err(Error::custom(
					ErrorKind::InvalidState,
					format!("expected Rustls TLS stream, got {:?}", other),
				));
			}
		};
		Ok(WebSocketStream::from_raw_socket(
			tls_stream,
			tokio_tungstenite::tungstenite::protocol::Role::Client,
			None,
		)
		.await)
	}

	async fn perform_handshake(
		ws_stream: WebSocketStream<TlsStream<TcpStream>>,
		disconnected_listeners: &mut DisconnectedListenerManager,
		api_key: &str,
	) -> Result<(WebSocketStreams, Duration), Error> {
		let (mut write, mut read) = ws_stream.split();

		let handshake_msg = await_next_message(&mut read)
			.await
			.context("receiving handshake message")?;

		let Some(packet_type) = handshake_msg.bytes().next() else {
			return Err(Error::custom(
				ErrorKind::Server,
				"Empty handshake message received over WebSocket",
			));
		};

		if PacketType::try_from(packet_type) != Ok(PacketType::Connect) {
			return Err(Error::custom(
				ErrorKind::Server,
				format!(
					"Did not receive connect packet in handshake message, got packet type {}",
					packet_type
				),
			));
		}

		let handshake: HandShake = serde_json::from_str(&handshake_msg[1..]).map_err(|e| {
			Error::custom(
				ErrorKind::Server,
				format!("Failed to parse handshake message: {}", e),
			)
		})?;

		let read_msg = await_next_message(&mut read)
			.await
			.context("receiving connect message")?;

		if read_msg != Utf8Bytes::from_static(MESSAGE_CONNECT_PAYLOAD) {
			return Err(Error::custom(
				ErrorKind::Server,
				format!(
					"expected connect message after handshake payload, got {:?} instead",
					read_msg
				),
			));
		}

		send_next_message(
			&mut write,
			Utf8Bytes::from(format!(
				r#"42["authed","{}"]"#,
				chrono::Utc::now().timestamp_millis()
			)),
		)
		.await
		.context("authed message")?;

		let authed_status_msg = await_next_message(&mut read)
			.await
			.context("receiving authed message")?;

		if authed_status_msg != Utf8Bytes::from_static(r#"42["authed",false]"#) {
			return Err(Error::custom(
				ErrorKind::Server,
				format!(
					"expected authed false message after authed payload, got {:?} instead",
					authed_status_msg
				),
			));
		}

		send_next_message(
			&mut write,
			Utf8Bytes::from(format!(r#"42["auth",{{"apiKey":"{api_key}"}}]"#)),
		)
		.await
		.context("auth message")?;

		let auth_success_msg = await_next_message(&mut read)
			.await
			.context("receiving auth success message")?;

		let auth_success_msg = auth_success_msg.as_str();

		match auth_success_msg {
			r#"42["authFailed"]"# => {
				disconnected_listeners.broadcast_auth_failed();
				return Err(Error::custom(
					ErrorKind::Unauthenticated,
					"WebSocket authentication failed: invalid API key",
				));
			}
			r#"42["authSuccess"]"# => {}
			other => {
				return Err(Error::custom(
					ErrorKind::Server,
					format!(
						"expected authSuccess or authFailed message after auth payload, got {:?} instead",
						other
					),
				));
			}
		}

		send_next_message(&mut write, Utf8Bytes::from_static(MESSAGE_EVENT_PAYLOAD))
			.await
			.context("auth message")?;

		Ok((
			WebSocketStreams { write, read },
			Duration::from_millis(handshake.ping_interval),
		))
	}

	async fn connect(
		disconnected_listeners: &mut DisconnectedListenerManager,
		api_key: &str,
	) -> Result<(Self, Duration), Error> {
		let request = build_request(api_key);

		let ws_stream = Self::connect_into_tls(request).await?;

		Self::perform_handshake(ws_stream, disconnected_listeners, api_key).await
	}
}
