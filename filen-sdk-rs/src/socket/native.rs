use std::{borrow::Cow, time::Duration};

use futures::{
	StreamExt,
	stream::{SplitSink, SplitStream},
};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tungstenite::{Message, Utf8Bytes};

use crate::{Error, ErrorKind, auth::http::AuthClient};

use super::{
	consts::{PING_MESSAGE, WEBSOCKET_URL_CORE},
	traits::*,
};

impl IntoStableDeref for Utf8Bytes {
	type Output = Box<Utf8Bytes>;

	fn into_stable_deref(self) -> Self::Output {
		// todo, hopefully the box can eventually be removed with
		// https://github.com/Storyyeller/stable_deref_trait/issues/21
		Box::new(self)
	}
}

pub(super) struct NativePingTask {
	task_handle: tokio::task::JoinHandle<()>,
}

impl PingTask<NativeSender> for NativePingTask {
	fn abort(self) {
		self.task_handle.abort();
	}

	fn new(mut sender: NativeSender, interval_duration: Duration) -> Self {
		let task_handle = tokio::spawn(async move {
			let mut interval = tokio::time::interval(interval_duration);

			loop {
				interval.tick().await;

				match sender
					.send_multiple([
						Cow::Borrowed(PING_MESSAGE),
						Cow::Owned(format!(
							r#"42["authed", {}]"#,
							chrono::Utc::now().timestamp_millis()
						)),
					])
					.await
				{
					Ok(Some(())) => {}
					Ok(None) => {
						tracing::debug!("WebSocket has been closed, stopping ping task");
						break;
					}
					Err(e) => {
						tracing::warn!("Failed to send WebSocket ping: {e}");
						continue;
					}
				}
			}
		});

		Self { task_handle }
	}
}

pub(super) struct UnauthedNativeSender {
	write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
}

impl Sender for UnauthedNativeSender {
	async fn send(&mut self, msg: Cow<'_, str>) -> Result<Option<()>, Error> {
		match futures::SinkExt::send(
			&mut self.write,
			Message::Text(Utf8Bytes::from(msg.into_owned())),
		)
		.await
		{
			Ok(()) => Ok(Some(())),
			Err(tungstenite::Error::ConnectionClosed) => Ok(None),
			Err(e) => Err(Error::custom(
				ErrorKind::Server,
				format!("failed to send message over websocket: {}", e),
			)),
		}
	}

	async fn send_multiple(
		&mut self,
		msgs: impl IntoIterator<Item = Cow<'_, str>>,
	) -> Result<Option<()>, Error> {
		for msg in msgs {
			match futures::SinkExt::feed(
				&mut self.write,
				Message::Text(Utf8Bytes::from(msg.into_owned())),
			)
			.await
			{
				Ok(()) => {}
				Err(tungstenite::Error::ConnectionClosed) => return Ok(None),
				Err(e) => {
					return Err(Error::custom(
						ErrorKind::Server,
						format!("failed to send message over websocket: {}", e),
					));
				}
			}
		}
		futures::SinkExt::flush(&mut self.write)
			.await
			.map_err(|e| {
				Error::custom(
					ErrorKind::Server,
					format!("failed to flush messages over websocket: {}", e),
				)
			})?;
		Ok(Some(()))
	}
}

impl UnauthedSender for UnauthedNativeSender {
	type AuthedType = NativeSender;
}

pub(super) struct NativeSender(UnauthedNativeSender);

impl Sender for NativeSender {
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

pub(super) struct UnauthedNativeReceiver {
	read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
}

impl Receiver<Utf8Bytes> for UnauthedNativeReceiver {
	async fn receive(&mut self) -> Option<Result<Utf8Bytes, Error>> {
		while let Some(msg) = self.read.next().await {
			match msg {
				Ok(Message::Text(txt)) => return Some(Ok(txt)),
				Ok(Message::Close(_)) => return None,
				Ok(_) => continue,
				Err(e) => {
					return Some(Err(Error::custom(
						ErrorKind::Server,
						format!("failed to receive message over websocket: {}", e),
					)));
				}
			}
		}
		None
	}
}

impl UnauthedReceiver<Utf8Bytes> for UnauthedNativeReceiver {
	type AuthedType = NativeReceiver;
}

pub(super) struct NativeReceiver(UnauthedNativeReceiver);

impl Receiver<Utf8Bytes> for NativeReceiver {
	async fn receive(&mut self) -> Option<Result<Utf8Bytes, Error>> {
		self.0.receive().await
	}
}

pub(super) struct UnauthedNativeSocket {
	stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl UnauthedSocket<UnauthedNativeSender, UnauthedNativeReceiver, Utf8Bytes>
	for UnauthedNativeSocket
{
	fn split(self) -> (UnauthedNativeSender, UnauthedNativeReceiver) {
		let (write, read) = self.stream.split();
		(
			UnauthedNativeSender { write },
			UnauthedNativeReceiver { read },
		)
	}
}

/// What [`NativeSocket::connect`] opens: the URL or, on the `platform_net` targets, how to reach it.
pub(super) struct NativeRequest {
	#[cfg(not(platform_net))]
	url: String,
	#[cfg(platform_net)]
	transport: std::sync::Arc<crate::platform_net::SocketTransport>,
	#[cfg(platform_net)]
	route: crate::platform_net::SocketRoute,
}

pub(super) struct NativeSocket {
	write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
	read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
}

impl
	Socket<
		NativeRequest,
		UnauthedNativeSocket,
		NativeSender,
		NativeReceiver,
		Utf8Bytes,
		UnauthedNativeSender,
		UnauthedNativeReceiver,
		NativePingTask,
	> for NativeSocket
{
	#[cfg_attr(not(platform_net), allow(unused_variables))]
	async fn build_request(client: &AuthClient) -> Result<NativeRequest, Error> {
		let url = format!(
			"{}{}",
			WEBSOCKET_URL_CORE,
			chrono::Utc::now().timestamp_millis()
		);
		#[cfg(platform_net)]
		{
			let transport = client.unauthed.socket_transport.clone();
			let route = transport.route(&url)?;
			Ok(NativeRequest { transport, route })
		}
		#[cfg(not(platform_net))]
		Ok(NativeRequest { url })
	}

	#[cfg(platform_net)]
	fn proxied(request: &NativeRequest) -> bool {
		request.route.proxied()
	}

	async fn connect(request: NativeRequest) -> Result<UnauthedNativeSocket, Error> {
		// Bounded buffers: tungstenite's defaults (64 MiB message, 16 MiB frame) are sized for
		// servers, and a socket event is a few KiB of JSON — while one consumer of this socket
		// is an iOS file-provider extension living under a ~20 MB jetsam limit, where a single
		// default-sized buffer is the whole process budget. 4 MiB caps the worst case at
		// tolerable; NOT lower, because this socket also carries note/chat events whose
		// payloads embed full encrypted content, and an over-cap frame is a connection
		// teardown (a reconnect loop on every delivery), not a dropped message.
		let config = tungstenite::protocol::WebSocketConfig::default()
			.max_frame_size(Some(4 << 20))
			.max_message_size(Some(4 << 20));
		// The `platform_net` targets verify with the OS and go through the proxy; the others keep
		// tungstenite's webpki roots and connect directly.
		#[cfg(platform_net)]
		let ws_stream = request.transport.connect(request.route, config).await?;
		#[cfg(not(platform_net))]
		let (ws_stream, _) =
			tokio_tungstenite::connect_async_with_config(request.url, Some(config), false)
				.await
				.map_err(|e| {
					Error::custom(
						ErrorKind::Server,
						format!("failed to connect to websocket: {}", e),
					)
				})?;

		// Enforce TLS without unwrapping the stream: into_inner()/re-wrap would
		// discard tungstenite's read buffer, so a server first frame coalesced
		// with the 101 upgrade response would be lost and the handshake would
		// wait for a message that never arrives.
		match ws_stream.get_ref() {
			MaybeTlsStream::Rustls(_) => {}
			MaybeTlsStream::Plain(_) => {
				return Err(Error::custom(
					ErrorKind::InvalidState,
					"expected TLS stream, got plain stream",
				));
			}
			other => {
				return Err(Error::custom(
					ErrorKind::InvalidState,
					format!("expected Rustls TLS stream, got {:?}", other),
				));
			}
		}

		Ok(UnauthedNativeSocket { stream: ws_stream })
	}

	fn from_unauthed_parts(
		unauthed_sender: UnauthedNativeSender,
		unauthed_receiver: UnauthedNativeReceiver,
	) -> Self {
		Self {
			write: unauthed_sender.write,
			read: unauthed_receiver.read,
		}
	}

	fn split_into_receiver_and_ping_task(
		self,
		ping_interval: Duration,
	) -> (NativeReceiver, NativePingTask) {
		let sender = NativeSender(UnauthedNativeSender { write: self.write });
		let receiver = NativeReceiver(UnauthedNativeReceiver { read: self.read });
		(receiver, NativePingTask::new(sender, ping_interval))
	}
}
