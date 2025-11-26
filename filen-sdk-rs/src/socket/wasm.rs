use std::{borrow::Cow, cell::Cell, fmt::Write, time::Duration};

use tokio::sync::mpsc::error::TrySendError;
use wasm_bindgen::JsCast;

use crate::{Error, ErrorKind};

use super::{
	consts::{PING_MESSAGE, WEBSOCKET_URL_CORE},
	traits::*,
};

pub(super) struct WasmSocket {
	socket: web_sys::WebSocket,
	msg_receiver: tokio::sync::mpsc::Receiver<Result<String, Error>>,
	close_receiver: tokio::sync::oneshot::Receiver<()>,
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
				interval.tick().await;
				match stop_receiver.try_recv() {
					Ok(()) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
						log::debug!("Stopping WebSocket ping task");
						break;
					}
					Err(tokio::sync::oneshot::error::TryRecvError::Empty) => { /* continue */ }
				}

				timestamp_string.clear();
				if let Err(e) = write!(
					&mut timestamp_string,
					r#"42["authed", {}]"#,
					chrono::Utc::now().timestamp_millis()
				) {
					log::error!("Failed to format WebSocket authed ping: {e}");
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
						log::debug!("WebSocket has been closed, stopping ping task");
						break;
					}
					Err(e) => {
						log::error!("Failed to send WebSocket ping: {e}");
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
	socket: web_sys::WebSocket,
	msg_receiver: tokio::sync::mpsc::Receiver<Result<String, Error>>,
	close_receiver: tokio::sync::oneshot::Receiver<()>,
}

impl UnauthedSocket<UnauthedWasmSender, UnauthedWasmReceiver, String> for UnauthedWasmSocket {
	fn split(self) -> (UnauthedWasmSender, UnauthedWasmReceiver) {
		let sender = UnauthedWasmSender {
			socket: self.socket,
		};
		let receiver = UnauthedWasmReceiver {
			msg_receiver: self.msg_receiver,
			close_receiver: self.close_receiver,
		};
		(sender, receiver)
	}
}

pub(super) struct UnauthedWasmSender {
	socket: web_sys::WebSocket,
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
	msg_receiver: tokio::sync::mpsc::Receiver<Result<String, Error>>,
	close_receiver: tokio::sync::oneshot::Receiver<()>,
}

impl Receiver<String> for UnauthedWasmReceiver {
	async fn receive(&mut self) -> Option<Result<String, Error>> {
		match self.close_receiver.try_recv() {
			Ok(()) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
				// socket closed
				None
			}
			Err(tokio::sync::oneshot::error::TryRecvError::Empty) => self.msg_receiver.recv().await,
		}
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
				log::info!("WebSocket connection opened");
				if let Some(f) = fn_once.take() {
					f();
				} else {
					log::error!("WebSocket onopen called multiple times");
				}
			},
		);

		// onmessage
		let (msg_sender, msg_receiver) = tokio::sync::mpsc::channel::<Result<String, Error>>(16);
		let on_msg_closure = wasm_bindgen::prelude::Closure::<dyn Fn(web_sys::MessageEvent)>::new(
			move |e: web_sys::MessageEvent| {
				log::info!("WebSocket message received");
				let result = e.data().as_string().ok_or_else(|| {
					Error::custom(
						ErrorKind::Server,
						"expected text message while handling websocket message, received non-string",
					)
				});
				if let Err(TrySendError::Full(msg)) = msg_sender.try_send(result) {
					log::error!(
						"WebSocket message channel full, dropping message '{:?}'",
						msg
					);
				}
			},
		);

		// onclose
		let (close_sender, close_receiver) = tokio::sync::oneshot::channel();
		let fn_once = Cell::new(Some(move || {
			let _ = close_sender.send(());
		}));

		let on_close_closure = wasm_bindgen::prelude::Closure::<dyn Fn(web_sys::CloseEvent)>::new(
			move |_e: web_sys::CloseEvent| {
				log::info!("WebSocket connection closed");
				if let Some(f) = fn_once.take() {
					f();
				} else {
					log::error!("WebSocket onclose called multiple times");
				}
			},
		);

		// websocket creation
		let ws = match web_sys::WebSocket::new(&request) {
			Ok(ws) => ws,
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

		open_receiver.await.map_err(|e| {
			Error::custom(
				ErrorKind::Server,
				format!("failed to receive WebSocket open event: {}", e),
			)
		})?;

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
