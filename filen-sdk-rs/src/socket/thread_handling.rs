use std::{borrow::Cow, ops::Deref, sync::Arc, time::Duration};

use filen_types::api::v3::socket::{HandShake, PacketType};
use futures::{StreamExt, stream::FuturesOrdered};
use rsa::RsaPrivateKey;
use yoke::Yoke;

use crate::{
	Error, ErrorKind,
	auth::{Client, http::AuthClient},
	crypto::shared::MetaCrypter,
	error::ResultExt,
};

use super::{
	consts::{
		CONNECT_TIMEOUT, MESSAGE_CONNECT_PAYLOAD, MESSAGE_EVENT_PAYLOAD, PING_INTERVAL,
		RECONNECT_DELAY,
	},
	events::DecryptedSocketEvent,
	listener_manager::{ConnectedListenerManager, DisconnectedListenerManager, ListenerManagerExt},
	traits::*,
};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::{Instant, sleep, timeout};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::{
	std::Instant,
	tokio::{sleep, timeout},
};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use super::native;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use super::wasm;

#[derive(Default)]
pub(crate) struct WebSocketHandle {
	request_sender: Option<tokio::sync::mpsc::WeakSender<SocketRequest>>,
}

impl WebSocketHandle {
	/// Returns the cached request sender if the websocket thread is still usable,
	/// otherwise `None` — in which case the caller must spawn a fresh thread.
	///
	/// `WeakSender::upgrade` succeeds as long as any strong `Sender` (a live
	/// [`ListenerHandle`]) remains; it does not observe whether the receiver (the
	/// websocket task) is still alive. The task can exit for a non-channel reason —
	/// auth failure, a critical receive error, or a panic — and drop its receiver while
	/// a `ListenerHandle` is still held. We therefore also check `is_closed`, so we
	/// never hand out a sender whose next `send` would fail with "websocket thread has
	/// been closed".
	fn live_sender(&self) -> Option<tokio::sync::mpsc::Sender<SocketRequest>> {
		self.request_sender
			.as_ref()
			.and_then(|weak| weak.upgrade())
			.filter(|sender| !sender.is_closed())
	}

	pub(super) fn get_request_sender(
		&mut self,
		auth_client: &Arc<AuthClient>,
		client: &Client,
	) -> RequestSender {
		if let Some(sender) = self.live_sender() {
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
		self.live_sender().is_some()
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
	RV: IntoStableDeref + AsRef<str> + Send,
	<RV::Output as Deref>::Target: AsRef<str> + Send,
	US: UnauthedSender<AuthedType = S>,
	UR: UnauthedReceiver<RV, AuthedType = R>,
	UW: UnauthedSocket<US, UR, RV>,
	PT: PingTask<S>,
{
	let (mut receiver, ping_task) =
		web_socket.split_into_receiver_and_ping_task(config.ping_interval);

	let mut should_retry = true;

	let mut decryption_futures = FuturesOrdered::new();

	// A black-holed connection (NAT rebind, dead path) delivers neither an error
	// nor a close frame — receive() would pend forever while is_connected() stays
	// true. Any inbound frame resets this deadline; if it fires, the connection is
	// declared dead and the reconnect loop takes over.
	let liveness_timeout = liveness_timeout_from(config.ping_interval);
	let mut liveness = std::pin::pin!(sleep(liveness_timeout));

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
				let decrypted: Option<Option<Yoke<_, <RV as IntoStableDeref>::Output>>> = decrypted;
				if let Some(Some(decrypted)) = decrypted {
					listeners.broadcast_event(decrypted.get());
				}
			},
			message_result = receiver.receive() => {
				liveness.as_mut().reset(Instant::now() + liveness_timeout);
				let message = match message_result {
					None => {
						// websocket closed
						break;
					}
					Some(Ok(msg)) => msg,
					Some(Err(e)) => {
						// Post-handshake receive errors are transport-level (a reset
						// without a close frame, a protocol hiccup) — never auth: the
						// reconnect path re-runs the handshake, where genuine auth
						// failures are classified and terminate the task. Killing the
						// task here turned one WiFi blip into a permanent, silent loss
						// of all socket events.
						tracing::warn!(
							"Error receiving WebSocket message: {}, reconnecting",
							e
						);
						break;
					}
				};
				// Do not log the raw payload: it carries plaintext PII (sender emails)
			// and encrypted metadata blobs even at debug level.
			tracing::debug!(
				"Received WebSocket message ({} bytes)",
				message.as_ref().len()
			);

				match super::events::try_parse_message_from_str(message.into_stable_deref()) {
					Ok(Some(event_yoke)) => {
						if listeners.should_decrypt_event(event_yoke.get()) {
							decryption_futures.push_back(async move {
								// this performs unnecessary cloning, ideally we would use an async
								// yoke try_map_project_async but this does not currently exist
								// https://github.com/unicode-org/icu4x/issues/7253
								match DecryptedSocketEvent::try_from_encrypted(crypter, private_key, config.user_id, event_yoke).await {
									Ok(v) => Some(v),
									Err(e) => {
										tracing::warn!(
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
						tracing::warn!(
							"Error parsing WebSocket message: {}, continuing...",
							e
						);
					}
				}
			},
			_ = liveness.as_mut() => {
				tracing::warn!(
					"No WebSocket traffic within {:?}, reconnecting",
					liveness_timeout
				);
				break;
			},
		}
	}
	// Events that already arrived must survive the teardown: broadcast anything
	// still decrypting instead of dropping it with the queue — reconnects are
	// routine (transport errors, liveness) and must not silently lose events.
	while let Some(decrypted) = decryption_futures.next().await {
		let decrypted: Option<Yoke<_, <RV as IntoStableDeref>::Output>> = decrypted;
		if let Some(decrypted) = decrypted {
			listeners.broadcast_event(decrypted.get());
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
	loop {
		// Re-read the key on every attempt: a password change rotates it in
		// place, and a pre-loop snapshot would keep authenticating with the
		// stale key until authFailed permanently killed the task.
		let api_key = config
			.client
			.api_key()
			.read()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
			.0
			.to_string();

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
						tracing::error!("WebSocket authentication failed: {}, not retrying", e);
						return None;
					}
					Err(e) => {
						tracing::warn!("Error initializing WebSocket connection: {}, retrying...", e);
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
	RV: IntoStableDeref + AsRef<str> + Send,
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
	// One deadline over the whole setup: connect_async and every handshake read
	// are otherwise unbounded, so a dead peer would park the task forever.
	let setup = async {
		tracing::debug!("Connecting to WebSocket server...");
		let request = W::build_request().await?;

		tracing::debug!("WebSocket request built, connecting...");

		let unauthed_ws = W::connect(request).await?;
		tracing::debug!("WebSocket connected, performing handshake...");

		perform_handshake(unauthed_ws, listeners, api_key).await
	};
	timeout(CONNECT_TIMEOUT, setup).await.map_err(|_| {
		Error::custom(
			ErrorKind::Server,
			"timed out establishing websocket connection",
		)
	})?
}

// `skip_all` is load-bearing here: `api_key` must never be recorded as a span field. `err` logs
// handshake failures (which never contain the key) with the span attached; at `debug` level so
// transient reconnects — which the caller's retry loop already reports at WARN — don't double-log
// benign network churn at ERROR.
#[tracing::instrument(name = "websocket_handshake", skip_all, err(level = "debug"))]
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
		.context("event subscribe message")?;

	Ok((
		W::from_unauthed_parts(write, read),
		ping_interval_from_millis(handshake.ping_interval),
	))
}

/// The handshake's `pingInterval` is server-controlled and in milliseconds. On a
/// zero period, `tokio::time::interval` panics, while `wasmtimer::tokio::interval`
/// yields a permanently-past deadline that resolves on every poll — a busy-loop
/// ping flood. A huge value is just as hostile in the other direction: it stops
/// our pings and stretches the liveness deadline toward infinity, resurrecting
/// the undetectable black-holed connection. Clamp instead of trusting it.
fn ping_interval_from_millis(millis: u64) -> Duration {
	const MIN_PING_INTERVAL: Duration = Duration::from_secs(1);
	const MAX_PING_INTERVAL: Duration = Duration::from_secs(60);
	Duration::from_millis(millis).clamp(MIN_PING_INTERVAL, MAX_PING_INTERVAL)
}

/// How long without ANY inbound frame before the connection is declared dead and
/// reconnected. The server acknowledges each client ping within its ping window,
/// so a healthy connection receives something at least every `ping_interval`; two
/// missed windows plus grace means the path is gone (NAT rebind, black hole) even
/// though the socket never errored or closed.
fn liveness_timeout_from(ping_interval: Duration) -> Duration {
	ping_interval
		.saturating_mul(2)
		.saturating_add(Duration::from_secs(5))
}

#[cfg(test)]
mod tests {
	use super::*;

	// Regression test for the "websocket thread has been closed" error.
	//
	// `WebSocketHandle` caches a `WeakSender` to the websocket task and decides whether
	// the task is still alive via `WeakSender::upgrade`. But `upgrade` only checks the
	// strong-sender count — not whether the receiver (the task) is still alive — so it
	// succeeds as long as any strong `Sender` (a live `ListenerHandle`) remains. When
	// the task exits for a non-channel reason (auth failure, critical receive error,
	// panic) while a `ListenerHandle` is still held, the cached weak still upgrades and
	// the next `add_event_listener` sends into a dropped receiver, failing with
	// `InvalidState: websocket thread has been closed`.
	//
	// `live_sender` must therefore reject a channel whose receiver has been dropped, so
	// `get_request_sender` re-spawns a fresh thread instead of handing out a dead one.
	#[test]
	fn live_sender_rejects_channel_whose_receiver_was_dropped() {
		let (sender, receiver) = tokio::sync::mpsc::channel::<SocketRequest>(16);
		// A still-held `ListenerHandle` keeps the strong-sender count above zero.
		let _zombie_handle = sender.clone();
		let handle = WebSocketHandle {
			request_sender: Some(sender.downgrade()),
		};

		// While the websocket task (the receiver) is alive, the cached sender is usable.
		assert!(handle.live_sender().is_some());
		assert!(handle.is_connected());

		// The websocket task dies for a non-channel reason: its receiver is dropped
		// while a strong sender (the `ListenerHandle`) is still alive.
		drop(receiver);

		assert!(
			handle.live_sender().is_none(),
			"a cached sender to a dropped-receiver channel must be treated as dead"
		);
		assert!(
			!handle.is_connected(),
			"a websocket thread whose receiver was dropped must not report connected"
		);
	}

	#[test]
	fn ping_interval_from_millis_floors_zero() {
		assert_eq!(ping_interval_from_millis(0), Duration::from_secs(1));
	}

	/// Hermetic tests of the full websocket pipeline (connect → handshake → event
	/// loop → reconnect). Everything under [`run_async_websocket_task`] is generic
	/// over the transport traits, so a scripted in-memory fake exercises the REAL
	/// production control flow — reconnect classification, retry policy, listener
	/// broadcasts — with no network and a paused clock.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	mod pipeline {
		use std::{
			cell::RefCell,
			collections::VecDeque,
			sync::{Arc, Mutex, OnceLock, RwLock},
		};

		use filen_types::{auth::APIKey, crypto::EncryptedString};

		use super::*;
		use crate::{
			auth::{http::ClientConfig, unauth::UnauthClient},
			crypto::error::ConversionError,
		};

		// ----- scripted fake transport -----

		pub(super) enum ServerAction {
			/// deliver this frame to the client
			Msg(String),
			/// deliver this frame after the delay elapses (the paused clock
			/// auto-advances to it); NOTE: the item is consumed when `receive`
			/// is first polled, so a script must not rely on redelivery if
			/// another select arm wins during the delay
			Delayed(Duration, String),
			/// surface a transport-level receive error
			Err(&'static str),
			/// clean close (receive yields None)
			Close,
			/// never respond (black-holed connection)
			Hang,
		}

		pub(super) struct FakeConnection {
			script: VecDeque<ServerAction>,
			/// runs when the connection is established — e.g. rotate the api key
			/// or drop the request sender to end the task
			on_connect: Option<Box<dyn FnOnce()>>,
		}

		impl FakeConnection {
			fn new(script: Vec<ServerAction>) -> Self {
				Self {
					script: script.into(),
					on_connect: None,
				}
			}

			fn with_on_connect(mut self, f: impl FnOnce() + 'static) -> Self {
				self.on_connect = Some(Box::new(f));
				self
			}
		}

		#[derive(Default)]
		struct FakeState {
			connections: VecDeque<FakeConnection>,
			connect_count: usize,
			sent: Vec<String>,
		}

		thread_local! {
			static STATE: RefCell<FakeState> = RefCell::default();
		}

		fn prime(connections: Vec<FakeConnection>) {
			STATE.with(|s| {
				*s.borrow_mut() = FakeState {
					connections: connections.into(),
					..Default::default()
				}
			});
		}

		fn connect_count() -> usize {
			STATE.with(|s| s.borrow().connect_count)
		}

		fn sent_messages() -> Vec<String> {
			STATE.with(|s| s.borrow().sent.clone())
		}

		pub(super) struct FakeMsg(String);

		impl AsRef<str> for FakeMsg {
			fn as_ref(&self) -> &str {
				&self.0
			}
		}

		impl IntoStableDeref for FakeMsg {
			type Output = String;

			fn into_stable_deref(self) -> String {
				self.0
			}
		}

		pub(super) struct FakeSender;

		impl Sender for FakeSender {
			async fn send(&mut self, msg: Cow<'_, str>) -> Result<Option<()>, Error> {
				STATE.with(|s| s.borrow_mut().sent.push(msg.into_owned()));
				Ok(Some(()))
			}

			async fn send_multiple(
				&mut self,
				msgs: impl IntoIterator<Item = Cow<'_, str>>,
			) -> Result<Option<()>, Error> {
				for msg in msgs {
					self.send(msg).await?;
				}
				Ok(Some(()))
			}
		}

		impl UnauthedSender for FakeSender {
			type AuthedType = FakeSender;
		}

		pub(super) struct FakeReceiver {
			script: VecDeque<ServerAction>,
		}

		impl Receiver<FakeMsg> for FakeReceiver {
			async fn receive(&mut self) -> Option<Result<FakeMsg, Error>> {
				match self.script.pop_front() {
					Some(ServerAction::Msg(s)) => Some(Ok(FakeMsg(s))),
					Some(ServerAction::Delayed(delay, s)) => {
						tokio::time::sleep(delay).await;
						Some(Ok(FakeMsg(s)))
					}
					Some(ServerAction::Err(s)) => Some(Err(Error::custom(ErrorKind::Server, s))),
					Some(ServerAction::Close) => None,
					// an exhausted script behaves like a black-holed connection
					Some(ServerAction::Hang) | None => std::future::pending().await,
				}
			}
		}

		impl UnauthedReceiver<FakeMsg> for FakeReceiver {
			type AuthedType = FakeReceiver;
		}

		pub(super) struct FakeUnauthedSocket {
			script: VecDeque<ServerAction>,
		}

		impl UnauthedSocket<FakeSender, FakeReceiver, FakeMsg> for FakeUnauthedSocket {
			fn split(self) -> (FakeSender, FakeReceiver) {
				(
					FakeSender,
					FakeReceiver {
						script: self.script,
					},
				)
			}
		}

		pub(super) struct FakeNoopPingTask;

		impl PingTask<FakeSender> for FakeNoopPingTask {
			fn new(_: FakeSender, _: Duration) -> Self {
				Self
			}

			fn abort(self) {}
		}

		pub(super) struct FakeSocket {
			script: VecDeque<ServerAction>,
		}

		impl
			Socket<
				(),
				FakeUnauthedSocket,
				FakeSender,
				FakeReceiver,
				FakeMsg,
				FakeSender,
				FakeReceiver,
				FakeNoopPingTask,
			> for FakeSocket
		{
			async fn build_request() -> Result<(), Error> {
				Ok(())
			}

			async fn connect(_request: ()) -> Result<FakeUnauthedSocket, Error> {
				let conn = STATE.with(|s| {
					let mut s = s.borrow_mut();
					s.connect_count += 1;
					s.connections.pop_front()
				});
				match conn {
					Some(conn) => {
						if let Some(f) = conn.on_connect {
							f();
						}
						Ok(FakeUnauthedSocket {
							script: conn.script,
						})
					}
					None => Err(Error::custom(
						ErrorKind::Server,
						"no more scripted connections",
					)),
				}
			}

			fn from_unauthed_parts(_: FakeSender, receiver: FakeReceiver) -> Self {
				Self {
					script: receiver.script,
				}
			}

			fn split_into_receiver_and_ping_task(
				self,
				_ping_interval: Duration,
			) -> (FakeReceiver, FakeNoopPingTask) {
				(
					FakeReceiver {
						script: self.script,
					},
					FakeNoopPingTask,
				)
			}
		}

		// ----- helpers -----

		struct NoopCrypter;

		impl MetaCrypter for NoopCrypter {
			fn blocking_encrypt_meta_into(
				&self,
				_meta: &str,
				_out: String,
			) -> EncryptedString<'static> {
				unreachable!("pipeline tests never encrypt metadata")
			}

			fn blocking_decrypt_meta_into(
				&self,
				_meta: &EncryptedString,
				_out: Vec<u8>,
			) -> Result<String, (ConversionError, Vec<u8>)> {
				unreachable!("pipeline tests never decrypt metadata")
			}
		}

		fn test_private_key() -> Arc<RsaPrivateKey> {
			static KEY: OnceLock<Arc<RsaPrivateKey>> = OnceLock::new();
			Arc::clone(KEY.get_or_init(|| {
				// tiny key: never used for real crypto, only to satisfy the signature
				Arc::new(RsaPrivateKey::new(&mut old_rng::thread_rng(), 512).unwrap())
			}))
		}

		fn test_config() -> WebSocketConfig {
			let unauthed = UnauthClient::from_config(ClientConfig::default()).unwrap();
			WebSocketConfig {
				client: Arc::new(AuthClient::from_unauthed(
					unauthed,
					Arc::new(RwLock::new(APIKey(Cow::Borrowed("test-api-key")))),
				)),
				reconnect_delay: RECONNECT_DELAY,
				ping_interval: PING_INTERVAL,
				user_id: 0,
			}
		}

		/// The scripted handshake exchange, followed by `then` as the in-session script.
		fn handshake_then(then: Vec<ServerAction>) -> Vec<ServerAction> {
			let mut script = vec![
				ServerAction::Msg(format!(
					"0{}",
					r#"{"sid":"fake","upgrades":[],"pingInterval":25000,"pingTimeout":20000}"#
				)),
				ServerAction::Msg(MESSAGE_CONNECT_PAYLOAD.to_string()),
				ServerAction::Msg(r#"42["authed",false]"#.to_string()),
				ServerAction::Msg(r#"42["authSuccess"]"#.to_string()),
			];
			script.extend(then);
			script
		}

		fn recording_listener() -> (EventListenerCallback, Arc<Mutex<Vec<String>>>) {
			let events = Arc::new(Mutex::new(Vec::new()));
			let events_clone = Arc::clone(&events);
			let callback: EventListenerCallback = Box::new(move |event| {
				events_clone
					.lock()
					.unwrap()
					.push(event.event_type().to_string());
			});
			(callback, events)
		}

		// ----- tests -----

		#[tokio::test(start_paused = true)]
		async fn transport_error_triggers_reconnect_with_broadcasts() {
			let (request_sender, request_receiver) = tokio::sync::mpsc::channel(16);

			// conn1 dies with a transport error mid-session (WiFi drop), conn2
			// closes cleanly, conn3 stays healthy and shuts the task down by
			// dropping the final request sender
			let final_sender = request_sender.clone();
			prime(vec![
				FakeConnection::new(handshake_then(vec![ServerAction::Err(
					"connection reset without closing handshake",
				)])),
				FakeConnection::new(handshake_then(vec![ServerAction::Close])),
				FakeConnection::new(handshake_then(vec![ServerAction::Hang]))
					.with_on_connect(move || drop(final_sender)),
			]);

			let (callback, events) = recording_listener();
			let (id_sender, _id_receiver) = tokio::sync::oneshot::channel();
			let (_canceller, cancel_receiver) = tokio::sync::oneshot::channel();
			request_sender
				.send(SocketRequest::AddListener {
					id_sender,
					cancel_receiver,
					callback,
					event_types: None,
				})
				.await
				.unwrap();
			// the clones inside the script now hold the only remaining senders
			drop(request_sender);

			run_async_websocket_task::<FakeSocket, _, _, _, _, _, _, _, _>(
				test_config(),
				request_receiver,
				Arc::new(NoopCrypter),
				test_private_key(),
			)
			.await;

			// a transport error must demote (broadcasting `reconnecting`) and
			// reconnect — not kill the task
			assert_eq!(connect_count(), 3);
			assert_eq!(
				events.lock().unwrap().as_slice(),
				[
					"authSuccess",
					"reconnecting",
					"authSuccess",
					"reconnecting",
					"authSuccess"
				]
			);
		}

		#[tokio::test(start_paused = true)]
		async fn events_received_before_a_transport_error_are_not_lost() {
			let (request_sender, request_receiver) = tokio::sync::mpsc::channel(16);

			// a decrypt-free drive event arrives, then the transport dies: the
			// event may still be sitting in the decryption queue at break time
			// and must be broadcast before the reconnect demotes the listeners
			let final_sender = request_sender.clone();
			prime(vec![
				FakeConnection::new(handshake_then(vec![
					ServerAction::Msg(
						r#"42["file-trash",{"uuid":"00000000-0000-0000-0000-000000000000","driveMessageId":1}]"#
							.to_string(),
					),
					ServerAction::Err("connection reset"),
				])),
				FakeConnection::new(handshake_then(vec![ServerAction::Hang]))
					.with_on_connect(move || drop(final_sender)),
			]);

			let (callback, events) = recording_listener();
			let (id_sender, _id_receiver) = tokio::sync::oneshot::channel();
			let (_canceller, cancel_receiver) = tokio::sync::oneshot::channel();
			request_sender
				.send(SocketRequest::AddListener {
					id_sender,
					cancel_receiver,
					callback,
					event_types: None,
				})
				.await
				.unwrap();
			drop(request_sender);

			run_async_websocket_task::<FakeSocket, _, _, _, _, _, _, _, _>(
				test_config(),
				request_receiver,
				Arc::new(NoopCrypter),
				test_private_key(),
			)
			.await;

			assert_eq!(
				events.lock().unwrap().as_slice(),
				["authSuccess", "fileTrash", "reconnecting", "authSuccess"]
			);
		}

		#[tokio::test(start_paused = true)]
		async fn api_key_is_reread_on_each_connect_attempt() {
			let mut config = test_config();
			let client = Arc::clone(&config.client);

			// conn1 dies during the handshake (retryable, before auth is sent);
			// the key rotates while the retry loop runs, so attempt 2 must
			// authenticate with the NEW key, not a pre-loop snapshot
			prime(vec![
				FakeConnection::new(vec![ServerAction::Close]).with_on_connect(move || {
					*client
						.api_key()
						.write()
						.unwrap_or_else(|poisoned| poisoned.into_inner()) = APIKey(Cow::Borrowed("rotated-key"));
				}),
				FakeConnection::new(handshake_then(vec![])),
			]);

			let (_request_sender, mut request_receiver) = tokio::sync::mpsc::channel(16);
			let mut listeners = DisconnectedListenerManager::new();
			let socket = initialize_websocket::<FakeSocket, _, _, _, _, _, _, _, _>(
				&mut config,
				&mut request_receiver,
				&mut listeners,
			)
			.await;
			assert!(socket.is_some(), "second attempt should connect");

			let auth_messages: Vec<String> = sent_messages()
				.into_iter()
				.filter(|m| m.contains("apiKey"))
				.collect();
			assert_eq!(auth_messages, [r#"42["auth",{"apiKey":"rotated-key"}]"#]);
		}

		#[tokio::test(start_paused = true)]
		async fn black_holed_connection_triggers_reconnect_via_liveness_deadline() {
			let begin = tokio::time::Instant::now();
			let config = test_config();

			// a NAT rebind / network black hole: the connection stops delivering
			// anything, without an error or a close frame
			let socket = FakeSocket::from_unauthed_parts(
				FakeSender,
				FakeReceiver {
					script: vec![ServerAction::Hang].into(),
				},
			);

			let (_request_sender, mut request_receiver) = tokio::sync::mpsc::channel(16);
			let mut listeners = DisconnectedListenerManager::new().into_connected();

			let should_retry = tokio::time::timeout(
				Duration::from_secs(3600),
				handle_initialized_websocket(
					&config,
					socket,
					&mut request_receiver,
					&mut listeners,
					&NoopCrypter,
					&test_private_key(),
				),
			)
			.await
			.expect("a silent connection must be torn down by the liveness deadline");

			assert!(
				should_retry,
				"a dead connection reconnects, it does not kill the task"
			);
			assert_eq!(begin.elapsed(), liveness_timeout_from(config.ping_interval));
		}

		#[tokio::test(start_paused = true)]
		async fn inbound_traffic_resets_the_liveness_deadline() {
			let begin = tokio::time::Instant::now();
			let config = test_config();
			let liveness = liveness_timeout_from(config.ping_interval);

			// one (ignored, non-event) frame arrives shortly before the deadline
			// would fire; the deadline must restart from that frame
			let socket = FakeSocket::from_unauthed_parts(
				FakeSender,
				FakeReceiver {
					script: vec![
						ServerAction::Delayed(liveness - Duration::from_secs(1), "3".to_string()),
						ServerAction::Hang,
					]
					.into(),
				},
			);

			let (_request_sender, mut request_receiver) = tokio::sync::mpsc::channel(16);
			let mut listeners = DisconnectedListenerManager::new().into_connected();

			let should_retry = tokio::time::timeout(
				Duration::from_secs(3600),
				handle_initialized_websocket(
					&config,
					socket,
					&mut request_receiver,
					&mut listeners,
					&NoopCrypter,
					&test_private_key(),
				),
			)
			.await
			.expect("a silent connection must be torn down by the liveness deadline");

			assert!(should_retry);
			assert_eq!(
				begin.elapsed(),
				(liveness - Duration::from_secs(1)) + liveness
			);
		}

		#[tokio::test(start_paused = true)]
		async fn hung_handshake_times_out_and_stays_retryable() {
			let begin = tokio::time::Instant::now();

			// server accepts the connection but never sends the handshake packet
			prime(vec![FakeConnection::new(vec![ServerAction::Hang])]);

			let mut listeners = DisconnectedListenerManager::new();
			let result = tokio::time::timeout(
				Duration::from_secs(3600),
				connect_and_build_socket::<
					FakeUnauthedSocket,
					FakeSender,
					FakeReceiver,
					FakeMsg,
					FakeSocket,
					FakeSender,
					FakeReceiver,
					(),
					FakeNoopPingTask,
				>("key", &mut listeners),
			)
			.await
			.expect("a hung handshake must time out on its own");

			let err = match result {
				Ok(_) => panic!("the hung handshake must fail"),
				Err(e) => e,
			};
			assert!(
				!matches!(err.kind(), ErrorKind::Unauthenticated),
				"a handshake timeout must stay retryable, got: {err}"
			);
			assert_eq!(begin.elapsed(), CONNECT_TIMEOUT);
		}
	}

	#[test]
	fn ping_interval_from_millis_floors_tiny_values() {
		assert_eq!(ping_interval_from_millis(5), Duration::from_secs(1));
		assert_eq!(ping_interval_from_millis(999), Duration::from_secs(1));
	}

	#[test]
	fn ping_interval_from_millis_passes_through_sane_values() {
		assert_eq!(ping_interval_from_millis(1000), Duration::from_secs(1));
		assert_eq!(
			ping_interval_from_millis(25_000),
			Duration::from_millis(25_000)
		);
	}

	// A huge server-sent pingInterval would stop our pings and inflate the
	// liveness deadline toward infinity — the black-hole wedge by other means.
	#[test]
	fn ping_interval_from_millis_caps_huge_values() {
		assert_eq!(
			ping_interval_from_millis(25_000_000_000),
			Duration::from_secs(60)
		);
		assert_eq!(ping_interval_from_millis(u64::MAX), Duration::from_secs(60));
	}
}
