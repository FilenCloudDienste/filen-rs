//! A scripted local HTTP server for tests: the test reads each request off a `TcpListener` it
//! bound itself and writes the answer it wants the client to get.

use std::{
	borrow::Cow,
	sync::{Arc, RwLock},
};

use filen_types::auth::APIKey;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpStream,
};

use super::{AuthClient, ClientConfig, UnauthClient};

/// A client with a placeholder API key, for requests to a scripted server.
pub(crate) fn client(config: ClientConfig) -> AuthClient {
	AuthClient::from_unauthed(
		UnauthClient::from_config(config).expect("a test config is valid"),
		Arc::new(RwLock::new(APIKey(Cow::Borrowed("test-key")))),
	)
}

/// Reads one request head off a scripted server's connection.
pub(crate) async fn read_head(socket: &mut TcpStream) {
	let mut head = Vec::new();
	let mut byte = [0u8; 1];
	while !head.ends_with(b"\r\n\r\n") {
		socket.read_exact(&mut byte).await.unwrap();
		head.push(byte[0]);
	}
}

/// Answers a scripted server's request with `body`.
pub(crate) async fn respond_with(socket: &mut TcpStream, body: &str) {
	let response = format!(
		"HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
		body.len()
	);
	socket.write_all(response.as_bytes()).await.unwrap();
}
