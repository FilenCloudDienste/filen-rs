//! A scripted local HTTP server for tests: the test reads each request off a `TcpListener` it
//! bound itself and writes the answer it wants the client to get.

use std::{
	borrow::Cow,
	net::SocketAddr,
	sync::{Arc, RwLock},
};

use bytes::Bytes;
use filen_types::{auth::APIKey, fs::Uuid};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{TcpListener, TcpStream},
	task::JoinHandle,
};

use super::{AuthClient, ClientConfig, UnauthClient};
use crate::{api::download::download_file_chunk_by_uuid, consts::random_ingest_url};

/// The placeholder API key test clients send.
fn test_key() -> Arc<RwLock<APIKey<'static>>> {
	Arc::new(RwLock::new(APIKey(Cow::Borrowed("test-key"))))
}

/// A client with a placeholder API key, for requests to a scripted server.
pub(crate) fn client(config: ClientConfig) -> AuthClient {
	AuthClient::from_unauthed(
		UnauthClient::from_config(config).expect("a test config is valid"),
		test_key(),
	)
}

/// A [`client`] whose every request goes through the scripted HTTP proxy at `proxy`: an `https`
/// request arrives there as `CONNECT <host>:443`, an `http` one as an absolute-form request the
/// proxy answers as the server would. Reading a head proves its request got past the client's
/// limits. The proxied client has no connect timeout, so a CONNECT the proxy never answers keeps
/// its request in flight, holding its places, until the test drops it.
pub(crate) fn client_via_proxy(config: ClientConfig, proxy: SocketAddr) -> AuthClient {
	AuthClient::from_unauthed(
		unauthed_via_proxy(config, &format!("http://{proxy}")),
		test_key(),
	)
}

/// An [`UnauthClient`] built from `config` that sends every request through the HTTP proxy at
/// `proxy_url`.
pub(crate) fn unauthed_via_proxy(config: ClientConfig, proxy_url: &str) -> UnauthClient {
	let mut unauthed = UnauthClient::from_config(config).expect("a test config is valid");
	unauthed.reqwest_client = reqwest::Client::builder()
		.proxy(reqwest::Proxy::all(proxy_url).expect("a valid proxy URL"))
		.build()
		.expect("a client with a proxy builds");
	unauthed
}

/// The head a proxied request to the gateway starts with.
pub(crate) const GATEWAY_CONNECT: &str = "CONNECT gateway.filen.io:443";

/// A chunk transfer a test sends through [`client_via_proxy`] and never answers.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Stall {
	/// `post_raw_bytes_auth` to the ingest host, as `api::v3::upload::upload_file_chunk` sends it.
	Upload,
	/// `api::download::download_file_chunk_by_uuid`, the production chunk download.
	Download,
}

impl Stall {
	/// The head the transfer's CONNECT starts with.
	pub(crate) fn connect_line(self) -> &'static str {
		match self {
			Self::Upload => "CONNECT ingest.filen.io:443",
			Self::Download => "CONNECT egest.filen.io:443",
		}
	}

	/// Sends the transfer; its result is ignored, as the proxy never answers it.
	pub(crate) async fn send(self, client: &AuthClient) {
		match self {
			Self::Upload => {
				let ingest = random_ingest_url();
				let _ = client
					.post_raw_bytes_auth::<()>(
						Bytes::new(),
						&format!("{ingest}/v3/upload"),
						ingest.into(),
					)
					.await;
			}
			Self::Download => {
				let _ = download_file_chunk_by_uuid(
					&client.unauthed,
					"de-1",
					"filen-1",
					Uuid::from_u128(1),
					0,
					None::<&fn(u64, Option<u64>)>,
				)
				.await;
			}
		}
	}
}

/// Reads one request head off a scripted server's connection and returns it.
pub(crate) async fn read_head(socket: &mut TcpStream) -> String {
	let mut head = Vec::new();
	let mut byte = [0u8; 1];
	while !head.ends_with(b"\r\n\r\n") {
		socket.read_exact(&mut byte).await.unwrap();
		head.push(byte[0]);
	}
	String::from_utf8_lossy(&head).into_owned()
}

/// Answers a scripted server's request with `body`.
pub(crate) async fn respond_with(socket: &mut TcpStream, body: &str) {
	let response = format!(
		"HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
		body.len()
	);
	socket.write_all(response.as_bytes()).await.unwrap();
}

/// Serves one request with `answer` and returns the URL to send it to. The task holds the
/// connection until it is joined: closing it with the request body unread resets it.
pub(crate) async fn serve_once(answer: impl Into<String>) -> (String, JoinHandle<TcpStream>) {
	let answer = answer.into();
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let server = tokio::spawn(async move {
		let (mut socket, _) = listener.accept().await.unwrap();
		read_head(&mut socket).await;
		respond_with(&mut socket, &answer).await;
		socket
	});
	(format!("http://{addr}/"), server)
}
