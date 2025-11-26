use std::{borrow::Cow, time::Duration};

use futures::{
	StreamExt,
	stream::{SplitSink, SplitStream},
};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::Role};
use tungstenite::{Message, Utf8Bytes};

use crate::{Error, ErrorKind};

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

		Self { task_handle }
	}
}

pub(super) struct UnauthedNativeSender {
	write: SplitSink<WebSocketStream<TlsStream<TcpStream>>, Message>,
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
	read: SplitStream<WebSocketStream<TlsStream<TcpStream>>>,
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
	stream: WebSocketStream<TlsStream<TcpStream>>,
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

pub(super) struct NativeSocket {
	write: SplitSink<WebSocketStream<TlsStream<TcpStream>>, Message>,
	read: SplitStream<WebSocketStream<TlsStream<TcpStream>>>,
}

impl
	Socket<
		String,
		UnauthedNativeSocket,
		NativeSender,
		NativeReceiver,
		Utf8Bytes,
		UnauthedNativeSender,
		UnauthedNativeReceiver,
		NativePingTask,
	> for NativeSocket
{
	async fn build_request() -> Result<String, Error> {
		Ok(format!(
			"{}{}",
			WEBSOCKET_URL_CORE,
			chrono::Utc::now().timestamp_millis()
		))
	}

	async fn connect(request: String) -> Result<UnauthedNativeSocket, Error> {
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

		Ok(UnauthedNativeSocket {
			stream: WebSocketStream::from_raw_socket(tls_stream, Role::Client, None).await,
		})
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
