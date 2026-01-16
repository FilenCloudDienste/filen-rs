use std::{borrow::Cow, ops::Deref, sync::Arc, time::Duration};

use filen_types::{
	api::v3::socket::{HandShake, PacketType},
	traits::CowHelpers,
};
use futures::{StreamExt, stream::FuturesOrdered};
use rsa::RsaPrivateKey;

use crate::{
	Error, ErrorKind,
	auth::{Client, http::AuthClient},
	crypto::shared::MetaCrypter,
	error::ResultExt,
};

use super::{
	consts::{
		MAX_RECONNECT_DELAY, MESSAGE_CONNECT_PAYLOAD, MESSAGE_EVENT_PAYLOAD, PING_INTERVAL,
		RECONNECT_DELAY,
	},
	events::DecryptedSocketEvent,
	listener_manager::{ConnectedListenerManager, DisconnectedListenerManager, ListenerManagerExt},
	traits::*,
};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::sleep;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::tokio::sleep;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use super::native;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use super::wasm;

#[derive(Default)]
pub(crate) struct WebSocketHandle {
	request_sender: Option<tokio::sync::mpsc::WeakSender<SocketRequest>>,
}

impl WebSocketHandle {
	pub(super) fn get_request_sender(
		&mut self,
		auth_client: &Arc<AuthClient>,
		client: &Client,
	) -> RequestSender {
		if let Some(weak_sender) = &self.request_sender
			&& let Some(sender) = weak_sender.upgrade()
		{
			return RequestSender(sender);
		}

		let sender = spawn_websocket_thread(
			Arc::clone(auth_client),
			client.crypter(),
			client.arc_private_key(),
			client.user_id,
		);
		self.request_sender = Some(sender.downgrade());
		RequestSender(sender)
	}

	// todo rework this because it only shows connected status not authenticated status
	pub(super) fn is_connected(&self) -> bool {
		self.request_sender
			.as_ref()
			.is_some_and(|weak| weak.strong_count() > 0)
	}
}

pub(super) struct RequestSender(tokio::sync::mpsc::Sender<SocketRequest>);

impl RequestSender {
	pub(super) async fn add_event_listener(
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

pub(super) enum SocketRequest {
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
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			{
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
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				wasm_bindgen_futures::spawn_local(async move {
					let _ = sender.send(request).await;
				});
			}
		}
		Err(_) => {
			// channel closed, nothing we can do
		}
	}
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen
)]
/// Handle to a registered WebSocket event listener.
/// When this handle is dropped, the listener is automatically unregistered.
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
	pub(super) struct ListenerRegisterGuard {
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

pub(super) struct WebSocketConfig {
	pub(super) client: Arc<AuthClient>,
	pub(super) reconnect_delay: Duration,
	pub(super) max_reconnect_delay: Duration,
	pub(super) ping_interval: Duration,
	pub(super) user_id: u64,
}

fn spawn_websocket_thread(
	client: Arc<AuthClient>,
	crypter: Arc<impl MetaCrypter + 'static>,
	private_key: Arc<RsaPrivateKey>,
	user_id: u64,
) -> tokio::sync::mpsc::Sender<SocketRequest> {
	let (request_sender, request_receiver) = tokio::sync::mpsc::channel::<SocketRequest>(16);

	let config = WebSocketConfig {
		client,
		reconnect_delay: RECONNECT_DELAY,
		max_reconnect_delay: MAX_RECONNECT_DELAY,
		ping_interval: PING_INTERVAL,
		user_id,
	};

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	let f = || {
		run_async_websocket_task::<native::NativeSocket, _, _, _, _, _, _, _, _>(
			config,
			request_receiver,
			crypter,
			private_key,
		)
	};

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	let f = || {
		run_async_websocket_task::<wasm::WasmSocket, _, _, _, _, _, _, _, _>(
			config,
			request_receiver,
			crypter,
			private_key,
		)
	};

	crate::runtime::spawn_async(f);

	request_sender
}

/// Handles the initialized websocket connection, processing incoming messages and managing listeners.
async fn handle_initialized_websocket<W, S, R, RV, US, UR, T, UW, PT>(
	config: &WebSocketConfig,
	web_socket: W,
	request_receiver: &mut tokio::sync::mpsc::Receiver<SocketRequest>,
	listeners: &mut ConnectedListenerManager,
	crypter: &impl MetaCrypter,
	private_key: &RsaPrivateKey,
) -> bool
where
	W: Socket<T, UW, S, R, RV, US, UR, PT>,
	S: Sender,
	R: Receiver<RV>,
	RV: IntoStableDeref + AsRef<str>,
	<RV::Output as Deref>::Target: AsRef<str>,
	US: UnauthedSender<AuthedType = S>,
	UR: UnauthedReceiver<RV, AuthedType = R>,
	UW: UnauthedSocket<US, UR, RV>,
	PT: PingTask<S>,
{
	let (mut receiver, ping_task) =
		web_socket.split_into_receiver_and_ping_task(config.ping_interval);

	let mut should_retry = true;

	let mut decryption_futures = FuturesOrdered::new();

	loop {
		tokio::select! {
			biased;
			request = request_receiver.recv() => {
				let Some(request) = request else {
					// request channel closed, shutting down websocket task
					should_retry = false;
					break;
				};
				handle_request(request, listeners);
			},
			// we generally want to prioritize decrypting messages over reading new ones
			// and we need to make sure we don't try to call next here if there are no futures
			// as it would resolve immediately and starve the read side
			decrypted = decryption_futures.next(), if !decryption_futures.is_empty() => {
				if let Some(Some(decrypted)) = decrypted {
					listeners.broadcast_event(&decrypted);
				}
			},
			message_result = receiver.receive() => {
				let message = match message_result {
					None => {
						// websocket closed
						break;
					}
					Some(Ok(msg)) => msg,
					Some(Err(e)) => {
						log::error!(
							"Critical error handling WebSocket message: {}, shutting down WebSocket task",
							e
						);
						should_retry = false;
						break;
					}
				};

				match super::events::try_parse_message_from_str(message.into_stable_deref()) {
					Ok(Some(event_yoke)) => {
						if listeners.should_decrypt_event(event_yoke.get()) {
							decryption_futures.push_back(async move {
								// this performs unnecessary cloning, ideally we would use an async
								// yoke try_map_project_async but this does not currently exist
								// https://github.com/unicode-org/icu4x/issues/7253
								match DecryptedSocketEvent::try_from_encrypted(crypter, private_key, config.user_id, event_yoke.get().as_borrowed_cow()).await {
									Ok(v) => Some(v.into_owned_cow()),
									Err(e) => {
										log::error!(
											"Error decrypting WebSocket event: {}, skipping event",
											e
										);
										None
									}
								}
							});
						}
					},
					// ignore non-event messages
					Ok(None) => {},
					Err(e) => {
						log::error!(
							"Error parsing WebSocket message: {}, continuing...",
							e
						);
					}
				}
			},
		}
	}
	ping_task.abort();
	should_retry
}

async fn initialize_websocket<W, S, R, RV, US, UR, T, UW, PT>(
	config: &mut WebSocketConfig,
	request_receiver: &mut tokio::sync::mpsc::Receiver<SocketRequest>,
	listeners: &mut DisconnectedListenerManager,
) -> Option<W>
where
	W: Socket<T, UW, S, R, RV, US, UR, PT>,
	S: Sender,
	R: Receiver<RV>,
	RV: IntoStableDeref + AsRef<str>,
	<RV::Output as Deref>::Target: AsRef<str>,
	US: UnauthedSender<AuthedType = S>,
	UR: UnauthedReceiver<RV, AuthedType = R>,
	UW: UnauthedSocket<US, UR, RV>,
	PT: PingTask<S>,
{
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
			result = connect_and_build_socket(&api_key, listeners) => {
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
						sleep(config.reconnect_delay).await;
					}
				}
			}
		}
	}
}

async fn run_async_websocket_task<W, S, R, RV, US, UR, T, UW, PT>(
	mut config: WebSocketConfig,
	mut request_receiver: tokio::sync::mpsc::Receiver<SocketRequest>,
	crypter: Arc<impl MetaCrypter + 'static>,
	private_key: Arc<RsaPrivateKey>,
) where
	W: Socket<T, UW, S, R, RV, US, UR, PT>,
	S: Sender,
	R: Receiver<RV>,
	RV: IntoStableDeref + AsRef<str>,
	<RV::Output as Deref>::Target: AsRef<str>,
	US: UnauthedSender<AuthedType = S>,
	UR: UnauthedReceiver<RV, AuthedType = R>,
	UW: UnauthedSocket<US, UR, RV>,
	PT: PingTask<S>,
{
	let mut disconnected_listeners = DisconnectedListenerManager::new();

	loop {
		let streams = match initialize_websocket::<W, _, _, _, _, _, _, _, _>(
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
			&*crypter,
			&private_key,
		)
		.await;

		if !should_retry {
			break;
		}

		// Demote
		disconnected_listeners = connected_listeners.into_disconnected();

		sleep(config.reconnect_delay).await;
	}
}

/// Handles a socket request, modifying the listener manager as needed.
///
/// Returns true if there are no more listeners after handling the request.
fn handle_request(request: SocketRequest, listeners: &mut impl ListenerManagerExt) {
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
}

async fn connect_and_build_socket<UW, US, UR, RV, W, S, R, T, PT>(
	api_key: &str,
	listeners: &mut DisconnectedListenerManager,
) -> Result<(W, Duration), Error>
where
	UW: UnauthedSocket<US, UR, RV>,
	US: UnauthedSender<AuthedType = S>,
	UR: UnauthedReceiver<RV, AuthedType = R>,
	W: Socket<T, UW, S, R, RV, US, UR, PT>,
	S: Sender,
	R: Receiver<RV>,
	RV: IntoStableDeref + AsRef<str>,
	<RV::Output as Deref>::Target: AsRef<str>,
	PT: PingTask<S>,
{
	log::info!("Connecting to WebSocket server...");
	let request = W::build_request().await?;

	log::info!("WebSocket request built, connecting...");

	let unauthed_ws = W::connect(request).await?;
	log::info!("WebSocket connected, performing handshake...");

	perform_handshake(unauthed_ws, listeners, api_key).await
}

async fn perform_handshake<UW, US, UR, RV, W, S, R, T, PT>(
	unauthed_ws: UW,
	disconnected_listeners: &mut DisconnectedListenerManager,
	api_key: &str,
) -> Result<(W, Duration), Error>
where
	UW: UnauthedSocket<US, UR, RV>,
	US: UnauthedSender<AuthedType = S>,
	UR: UnauthedReceiver<RV, AuthedType = R>,
	W: Socket<T, UW, S, R, RV, US, UR, PT>,
	S: Sender,
	R: Receiver<RV>,
	RV: IntoStableDeref + AsRef<str>,
	<RV::Output as Deref>::Target: AsRef<str>,
	PT: PingTask<S>,
{
	let (mut write, mut read) = unauthed_ws.split();

	let handshake_msg = read.receive().await.ok_or_else(|| {
		Error::custom(
			ErrorKind::Server,
			"websocket closed unexpectedly while awaiting handshake message",
		)
	})??;
	let handshake_msg = handshake_msg.as_ref();

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

	let read_msg = read.receive().await.ok_or_else(|| {
		Error::custom(
			ErrorKind::Server,
			"websocket closed unexpectedly while awaiting connect message",
		)
	})??;
	let read_msg = read_msg.as_ref();

	if read_msg != MESSAGE_CONNECT_PAYLOAD {
		return Err(Error::custom(
			ErrorKind::Server,
			format!(
				"expected connect message after handshake payload, got '{}' instead",
				read_msg
			),
		));
	}

	write
		.send(Cow::Owned(format!(
			r#"42["authed","{}"]"#,
			chrono::Utc::now().timestamp_millis()
		)))
		.await
		.context("authed message")?;

	let authed_status_msg = read.receive().await.ok_or_else(|| {
		Error::custom(
			ErrorKind::Server,
			"websocket closed unexpectedly while awaiting authed message",
		)
	})??;
	let authed_status_msg = authed_status_msg.as_ref();

	if authed_status_msg != r#"42["authed",false]"# {
		return Err(Error::custom(
			ErrorKind::Server,
			format!(
				"expected authed false message after authed payload, got '{}' instead",
				authed_status_msg
			),
		));
	}

	write
		.send(Cow::Owned(format!(
			r#"42["auth",{{"apiKey":"{api_key}"}}]"#
		)))
		.await
		.context("auth message")?;

	let auth_success_msg = read.receive().await.ok_or_else(|| {
		Error::custom(
			ErrorKind::Server,
			"websocket closed unexpectedly while awaiting auth success message",
		)
	})??;

	let auth_success_msg = auth_success_msg.as_ref();

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
					"expected authSuccess or authFailed message after auth payload, got '{}' instead",
					other
				),
			));
		}
	}

	write
		.send(Cow::Borrowed(MESSAGE_EVENT_PAYLOAD))
		.await
		.context("auth message")?;

	Ok((
		W::from_unauthed_parts(write, read),
		Duration::from_millis(handshake.ping_interval),
	))
}
