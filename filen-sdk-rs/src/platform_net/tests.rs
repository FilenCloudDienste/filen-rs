use std::time::Duration;

use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpListener,
	task::JoinHandle,
};

use super::*;
use crate::auth::http::{ClientConfig, test_support::read_head};

const SOCKET_URL: &str = "wss://socket.filen.io/socket.io/?EIO=3&transport=websocket&t=1";

/// A transport whose `https` proxy is `proxy`, exempting `no_proxy`.
fn via(proxy: &str, no_proxy: &str) -> SocketTransport {
	SocketTransport::with_proxy(
		platform_tls_config(),
		Matcher::builder().https(proxy).no(no_proxy).build(),
		Some(Duration::from_secs(5)),
	)
}

fn socket_target() -> Uri {
	proxy_target(&SOCKET_URL.parse().unwrap()).unwrap()
}

/// A scripted proxy for one connection: it reads the CONNECT head, answers `answer`, and
/// returns the head with whatever the client sent through the tunnel next (up to 5 bytes).
async fn scripted_proxy(answer: &'static str) -> (String, JoinHandle<(String, Vec<u8>)>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let proxy = tokio::spawn(async move {
		let (mut socket, _) = listener.accept().await.unwrap();
		let head = read_head(&mut socket).await;
		socket.write_all(answer.as_bytes()).await.unwrap();
		let mut tunnelled = vec![0u8; 5];
		let read = socket.read(&mut tunnelled).await.unwrap_or(0);
		tunnelled.truncate(read);
		(head, tunnelled)
	});
	(format!("127.0.0.1:{}", addr.port()), proxy)
}

#[test]
fn platform_tls_speaks_http1() {
	let config = platform_tls_config();
	// macOS and Windows always have a verifier; Linux has one when the system has CA
	// certificates, which every CI image does.
	#[cfg(any(target_os = "windows", target_os = "macos"))]
	assert!(config.is_some(), "the platform verifier must build");
	if let Some(config) = config {
		assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
	}
}

#[test]
fn reqwest_takes_the_platform_tls() {
	// reqwest fails to build with a preconfigured TLS of a rustls version other than its own
	// ("Unknown TLS backend"), so building proves it took ours.
	if let Some(tls) = platform_tls_config() {
		reqwest::Client::builder()
			.use_preconfigured_tls(tls)
			.build()
			.expect("reqwest must accept the platform TLS configuration");
	}
	ClientConfig::default()
		.build_http()
		.expect("a default client builds");
}

#[test]
fn proxy_target_is_the_sockets_https_origin() {
	assert_eq!(socket_target(), "https://socket.filen.io/");
	let with_port = proxy_target(&"wss://socket.filen.io:8443/x?y=1".parse().unwrap()).unwrap();
	assert_eq!(with_port, "https://socket.filen.io:8443/");
}

#[test]
fn proxy_resolution_honours_no_proxy() {
	let proxied = via("http://127.0.0.1:3128", "example.com");
	let proxy = proxied
		.proxy_for(&socket_target())
		.expect("a host NO_PROXY does not name goes through the proxy");
	assert_eq!(proxy.uri(), "http://127.0.0.1:3128/");

	// A domain exempts its subdomains, as curl and reqwest read NO_PROXY.
	for no_proxy in ["socket.filen.io", "filen.io", ".filen.io", "*"] {
		assert!(
			via("http://127.0.0.1:3128", no_proxy)
				.proxy_for(&socket_target())
				.is_none(),
			"NO_PROXY={no_proxy} must exempt the socket"
		);
	}

	// An `http`-only proxy is not used for the socket, which is `https`, as reqwest does.
	let http_only = SocketTransport::with_proxy(
		None,
		Matcher::builder().http("http://127.0.0.1:3128").build(),
		None,
	);
	assert!(http_only.proxy_for(&socket_target()).is_none());
}

#[tokio::test]
async fn connect_tunnels_through_the_proxy_then_speaks_tls() {
	let (proxy, server) = scripted_proxy("HTTP/1.1 200 Connection established\r\n\r\n").await;
	let transport = via(&format!("http://user:pass@{proxy}"), "");
	let connect = tokio::time::timeout(
		Duration::from_secs(10),
		transport.connect(
			SOCKET_URL,
			tungstenite::protocol::WebSocketConfig::default(),
		),
	);
	let (result, proxied) = tokio::join!(connect, server);
	let (head, tunnelled) = proxied.unwrap();

	assert!(
		head.starts_with("CONNECT socket.filen.io:443 HTTP/1.1\r\nHost: socket.filen.io:443\r\n"),
		"unexpected CONNECT head: {head:?}"
	);
	// base64("user:pass")
	assert!(
		head.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"),
		"the proxy URL's credentials must be sent: {head:?}"
	);
	// The first thing through the tunnel is a TLS handshake record (ClientHello), not the
	// websocket upgrade in the clear.
	assert_eq!(tunnelled.first(), Some(&0x16), "got {tunnelled:?}");
	// The proxy hung up instead of relaying the server's answer.
	assert!(result.expect("the connect must not hang").is_err());
}

#[tokio::test]
async fn a_refused_tunnel_fails_the_connect() {
	for answer in [
		"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n",
		"HTTP/1.1 502 Bad Gateway\r\n\r\n",
	] {
		let (proxy, server) = scripted_proxy(answer).await;
		let transport = via(&format!("http://{proxy}"), "");
		let result = tokio::time::timeout(
			Duration::from_secs(10),
			transport.connect(
				SOCKET_URL,
				tungstenite::protocol::WebSocketConfig::default(),
			),
		)
		.await
		.expect("the connect must not hang");
		let err = match result {
			Ok(_) => panic!("{answer:?} must fail the connect"),
			Err(e) => e,
		};
		assert!(matches!(err.kind(), ErrorKind::Server), "{err}");
		assert!(err.to_string().contains("tunnel"), "{err}");
		let (_, tunnelled) = server.await.unwrap();
		assert!(tunnelled.is_empty(), "nothing may follow a refused CONNECT");
	}
}

#[tokio::test]
async fn an_unsupported_proxy_is_refused_without_connecting() {
	// Nothing listens on port 1: a connection attempt would fail differently.
	for proxy in ["socks5://127.0.0.1:1", "https://127.0.0.1:1"] {
		let err = match via(proxy, "")
			.connect(
				SOCKET_URL,
				tungstenite::protocol::WebSocketConfig::default(),
			)
			.await
		{
			Ok(_) => panic!("{proxy} must be refused"),
			Err(e) => e,
		};
		assert!(err.to_string().contains("only http"), "{proxy}: {err}");
	}
}
