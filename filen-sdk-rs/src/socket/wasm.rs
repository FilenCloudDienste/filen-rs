use std::{borrow::Cow, cell::Cell, fmt::Write, ops::Deref, time::Duration};

use tokio::sync::mpsc::error::TrySendError;
use wasm_bindgen::JsCast;

use crate::{Error, ErrorKind};

use super::{
	consts::{PING_MESSAGE, WEBSOCKET_URL_CORE},
	traits::*,
};

/// Owns the browser socket and closes it when the last wrapper is dropped.
///
/// Dropping the raw `web_sys::WebSocket` only releases Rust's handle to the JS
/// object: with live event listeners the browser keeps the underlying
/// connection open, so every abandoned attempt (connect timeout, liveness
/// deadline) would leak a real connection until the per-origin limit blocks
/// all reconnects.
struct WebSocketGuard(web_sys::WebSocket);

impl Drop for WebSocketGuard {
	fn drop(&mut self) {
		// a bare close() (no close code) cannot throw, and closing an already
		// CLOSING/CLOSED socket is a no-op
		if let Err(e) = self.0.close() {
			tracing::error!("Failed to close WebSocket: {:?}", e.as_string());
		}
	}
}

impl Deref for WebSocketGuard {
	type Target = web_sys::WebSocket;

	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

pub(super) struct WasmSocket {
	socket: WebSocketGuard,
	msg_receiver: tokio::sync::mpsc::Receiver<String>,
	close_receiver: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl IntoStableDeref for String {
	type Output = String;

	fn into_stable_deref(self) -> Self::Output {
		self
	}
}

pub(super) struct WasmPingTask {
	stop_channel: tokio::sync::oneshot::Sender<()>,
}

impl PingTask<WasmSender> for WasmPingTask {
	fn abort(self) {
		let _ = self.stop_channel.send(());
	}

	fn new(mut sender: WasmSender, interval_duration: std::time::Duration) -> Self {
		let mut interval = wasmtimer::tokio::interval(interval_duration);

		let mut timestamp_string = String::new();

		let (stop_channel, mut stop_receiver) = tokio::sync::oneshot::channel();

		wasm_bindgen_futures::spawn_local(async move {
			loop {
				// Bail promptly on abort (send or sender drop): this sender owns
				// the WebSocketGuard, so the browser socket only closes once this
				// task exits — waiting out the next tick would keep the old
				// connection open into the next reconnect attempt.
				tokio::select! {
					biased;
					_ = &mut stop_receiver => {
						tracing::debug!("Stopping WebSocket ping task");
						break;
					}
					_ = interval.tick() => {}
				}

				timestamp_string.clear();
				if let Err(e) = write!(
					&mut timestamp_string,
					r#"42["authed", {}]"#,
					chrono::Utc::now().timestamp_millis()
				) {
					tracing::error!("Failed to format WebSocket authed ping: {e}");
					break;
				}

				match sender
					.send_multiple([
						Cow::Borrowed(PING_MESSAGE),
						Cow::Borrowed(&timestamp_string),
					])
					.await
				{
					Ok(Some(())) => {}
					Ok(None) => {
						tracing::debug!("WebSocket has been closed, stopping ping task");
						break;
					}
					Err(e) => {
						tracing::error!("Failed to send WebSocket ping: {e}");
						continue;
					}
				}
			}
		});

		WasmPingTask { stop_channel }
	}
}

pub(super) struct WasmSender(UnauthedWasmSender);

impl Sender for WasmSender {
	async fn send(&mut self, msg: Cow<'_, str>) -> Result<Option<()>, Error> {
		self.0.send(msg).await
	}

	async fn send_multiple(
		&mut self,
		msgs: impl IntoIterator<Item = Cow<'_, str>>,
	) -> Result<Option<()>, Error> {
		self.0.send_multiple(msgs).await
	}
}

pub(super) struct WasmReceiver(UnauthedWasmReceiver);

impl Receiver<String> for WasmReceiver {
	async fn receive(&mut self) -> Option<Result<String, Error>> {
		self.0.receive().await
	}
}

pub(super) struct UnauthedWasmSocket {
	socket: WebSocketGuard,
	msg_receiver: tokio::sync::mpsc::Receiver<String>,
	close_receiver: tokio::sync::oneshot::Receiver<()>,
}

impl UnauthedSocket<UnauthedWasmSender, UnauthedWasmReceiver, String> for UnauthedWasmSocket {
	fn split(self) -> (UnauthedWasmSender, UnauthedWasmReceiver) {
		let sender = UnauthedWasmSender {
			socket: self.socket,
		};
		let receiver = UnauthedWasmReceiver {
			msg_receiver: self.msg_receiver,
			close_receiver: Some(self.close_receiver),
		};
		(sender, receiver)
	}
}

pub(super) struct UnauthedWasmSender {
	socket: WebSocketGuard,
}

impl UnauthedSender for UnauthedWasmSender {
	type AuthedType = WasmSender;
}

impl Sender for UnauthedWasmSender {
	async fn send(&mut self, msg: Cow<'_, str>) -> Result<Option<()>, Error> {
		self.socket.send_with_str(&msg).map_err(|e| {
			Error::custom(
				ErrorKind::Server,
				format!("failed to send message over WebSocket: {:?}", e.as_string()),
			)
		})?;
		Ok(Some(()))
	}

	async fn send_multiple(
		&mut self,
		msgs: impl IntoIterator<Item = Cow<'_, str>>,
	) -> Result<Option<()>, Error> {
		for msg in msgs {
			self.socket.send_with_str(&msg).map_err(|e| {
				Error::custom(
					ErrorKind::Server,
					format!("failed to send message over WebSocket: {:?}", e.as_string()),
				)
			})?;
		}
		Ok(Some(()))
	}
}

pub(super) struct UnauthedWasmReceiver {
	msg_receiver: tokio::sync::mpsc::Receiver<String>,
	close_receiver: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl Receiver<String> for UnauthedWasmReceiver {
	async fn receive(&mut self) -> Option<Result<String, Error>> {
		if let Some(close_receiver) = &mut self.close_receiver {
			// The old try_recv-at-entry-then-plain-await never woke for a close
			// that fired mid-await: a dead socket went undetected until unrelated
			// request traffic happened to re-enter this function. Racing the two
			// (message-first, so frames that arrived before the close are still
			// drained in order) keeps this cancellation-safe: nothing is consumed
			// unless an arm actually completes.
			tokio::select! {
				biased;
				msg = self.msg_receiver.recv() => return msg.map(Ok),
				_ = close_receiver => {}
			}
			// the close fired: never poll the finished oneshot again
			self.close_receiver = None;
		}
		// closed: no new frames can arrive — deliver anything still buffered,
		// then report end-of-stream
		self.msg_receiver.try_recv().ok().map(Ok)
	}
}

impl UnauthedReceiver<String> for UnauthedWasmReceiver {
	type AuthedType = WasmReceiver;
}

impl
	Socket<
		String,
		UnauthedWasmSocket,
		WasmSender,
		WasmReceiver,
		String,
		UnauthedWasmSender,
		UnauthedWasmReceiver,
		WasmPingTask,
	> for WasmSocket
{
	async fn build_request() -> Result<String, Error> {
		Ok(format!(
			"{}{}",
			WEBSOCKET_URL_CORE,
			chrono::Utc::now().timestamp_millis()
		))
	}

	async fn connect(request: String) -> Result<UnauthedWasmSocket, Error> {
		// onopen
		let (open_sender, open_receiver) = tokio::sync::oneshot::channel();
		let fn_once = Cell::new(Some(move || {
			let _ = open_sender.send(());
		}));

		let on_open_closure = wasm_bindgen::prelude::Closure::<dyn Fn(web_sys::Event)>::new(
			move |_e: web_sys::Event| {
				tracing::debug!("WebSocket connection opened");
				if let Some(f) = fn_once.take() {
					f();
				} else {
					tracing::error!("WebSocket onopen called multiple times");
				}
			},
		);

		// onmessage. The JS callback cannot be backpressured (unlike native's
		// lazily-pulled stream), so the channel bound is the only buffer between
		// an event burst and a consumer busy decrypting — keep it roomy; overflow
		// still only drops the excess with a warning.
		let (msg_sender, msg_receiver) = tokio::sync::mpsc::channel::<String>(64);
		let on_msg_closure = wasm_bindgen::prelude::Closure::<dyn Fn(web_sys::MessageEvent)>::new(
			move |e: web_sys::MessageEvent| {
				// skip non-text frames (Blob/ArrayBuffer) like native does —
				// erroring here used to permanently kill the websocket task
				let Some(text) = e.data().as_string() else {
					tracing::warn!("received non-text WebSocket message, ignoring");
					return;
				};
				if let Err(TrySendError::Full(msg)) = msg_sender.try_send(text) {
					tracing::warn!(
						"WebSocket message channel full, dropping message '{:?}'",
						msg
					);
				}
			},
		);

		// onclose
		let (close_sender, mut close_receiver) = tokio::sync::oneshot::channel();
		let fn_once = Cell::new(Some(move || {
			let _ = close_sender.send(());
		}));

		let on_close_closure = wasm_bindgen::prelude::Closure::<dyn Fn(web_sys::CloseEvent)>::new(
			move |_e: web_sys::CloseEvent| {
				tracing::debug!("WebSocket connection closed");
				if let Some(f) = fn_once.take() {
					f();
				} else {
					tracing::error!("WebSocket onclose called multiple times");
				}
			},
		);

		// websocket creation — wrap in the guard before the first await so a
		// cancelled connect (CONNECT_TIMEOUT dropping this future) still closes
		// the browser socket
		let ws = match web_sys::WebSocket::new(&request) {
			Ok(ws) => WebSocketGuard(ws),
			Err(e) => {
				return Err(Error::custom(
					ErrorKind::Server,
					format!("failed to create WebSocket: {:?}", e.as_string()),
				));
			}
		};

		ws.set_onopen(Some(on_open_closure.into_js_value().unchecked_ref()));
		ws.set_onmessage(Some(on_msg_closure.into_js_value().unchecked_ref()));
		ws.set_onclose(Some(on_close_closure.into_js_value().unchecked_ref()));

		// A failed connection attempt fires error/close, never open — awaiting
		// only onopen hung forever on an unreachable server (the leaked closure
		// keeps the open sender alive, so not even a channel error arrives).
		tokio::select! {
			biased;
			result = open_receiver => {
				result.map_err(|e| {
					Error::custom(
						ErrorKind::Server,
						format!("failed to receive WebSocket open event: {}", e),
					)
				})?;
			}
			_ = &mut close_receiver => {
				return Err(Error::custom(
					ErrorKind::Server,
					"WebSocket closed before opening",
				));
			}
		}

		Ok(UnauthedWasmSocket {
			socket: ws,
			msg_receiver,
			close_receiver,
		})
	}

	fn from_unauthed_parts(
		unauthed_sender: UnauthedWasmSender,
		unauthed_receiver: UnauthedWasmReceiver,
	) -> Self {
		Self {
			socket: unauthed_sender.socket,
			msg_receiver: unauthed_receiver.msg_receiver,
			close_receiver: unauthed_receiver.close_receiver,
		}
	}

	fn split_into_receiver_and_ping_task(
		self,
		ping_interval: Duration,
	) -> (WasmReceiver, WasmPingTask) {
		let sender = WasmSender(UnauthedWasmSender {
			socket: self.socket,
		});
		let receiver = WasmReceiver(UnauthedWasmReceiver {
			msg_receiver: self.msg_receiver,
			close_receiver: self.close_receiver,
		});
		(receiver, WasmPingTask::new(sender, ping_interval))
	}
}
