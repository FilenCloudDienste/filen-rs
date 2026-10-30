use std::{
	borrow::Cow,
	sync::Arc,
	time::{Duration, Instant},
};

use bytes::Bytes;
use filen_types::{api::v3::user::lock::LockType, fs::Uuid};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{TcpListener, TcpStream},
};

use super::{
	AuthClient, ClientConfig, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT,
	class_limit::CONTROL_REQUEST_HEADROOM,
	test_support::{GATEWAY_CONNECT, Stall, client, client_via_proxy, read_head, respond_with},
};

#[test]
fn default_config_enables_both_timeouts_and_builds() {
	let config = ClientConfig::default();
	assert_eq!(config.connect_timeout, Some(DEFAULT_CONNECT_TIMEOUT));
	assert_eq!(config.read_timeout, Some(DEFAULT_READ_TIMEOUT));
	config
		.build_reqwest_client()
		.expect("the default-configured client must build");
}

/// A connected-but-silent host — the residual hang the 404/retry fixes could not cover, since
/// there is no HTTP status to classify — is aborted by the read timeout instead of hanging.
#[tokio::test]
async fn read_timeout_aborts_a_silent_server() {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	tokio::spawn(async move {
		// Accept the connection, then never send a single byte.
		let (_socket, _) = listener.accept().await.unwrap();
		tokio::time::sleep(Duration::from_secs(30)).await;
	});

	let client = ClientConfig::default()
		.with_read_timeout(Some(Duration::from_millis(300)))
		.build_reqwest_client()
		.unwrap();

	let started = Instant::now();
	let err = client
		.get(format!("http://{addr}/"))
		.send()
		.await
		.unwrap_err();

	assert!(err.is_timeout(), "expected a timeout error, got {err:?}");
	assert!(
		started.elapsed() < Duration::from_secs(5),
		"should have timed out promptly, took {:?}",
		started.elapsed()
	);

	// The read timeout surfaces as a request-kind error, and the production classifier treats
	// it as non-retryable: a stalled host fails fast rather than burning another full timeout.
	assert!(
		err.is_request(),
		"a read timeout should be a request-kind error"
	);
	assert!(
		!super::is_pre_send_connect_failure(&err),
		"a timeout must not be absorbed by the pre-send connect class — that class exists for \
			 failures that cost nothing, and a stalled host must keep failing fast"
	);
	assert!(
		!super::is_attempt_retryable(
			err.status(),
			err.is_builder(),
			err.is_request(),
			super::is_dispatch_gone(&err),
		),
		"a read timeout must be classified non-retryable"
	);
}

/// The dir-listing constraint: a server that is slow to START responding (high time-to-first-
/// byte) but is alive must NOT be killed, as long as it responds within the read timeout. Here
/// it stalls 400ms before sending a 200, well under the 2s read timeout.
#[tokio::test]
async fn slow_time_to_first_byte_within_read_timeout_succeeds() {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	tokio::spawn(async move {
		let (mut socket, _) = listener.accept().await.unwrap();
		// Drain the request so the client's write completes, then simulate slow server-side
		// work before the first response byte.
		let mut buf = [0u8; 1024];
		let _ = socket.read(&mut buf).await;
		tokio::time::sleep(Duration::from_millis(400)).await;
		let body = "ok";
		let response = format!(
			"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
			body.len()
		);
		socket.write_all(response.as_bytes()).await.unwrap();
		socket.flush().await.unwrap();
	});

	let client = ClientConfig::default()
		.with_read_timeout(Some(Duration::from_millis(2000)))
		.build_reqwest_client()
		.unwrap();

	let response = client.get(format!("http://{addr}/")).send().await.unwrap();
	assert!(response.status().is_success());
	assert_eq!(response.text().await.unwrap(), "ok");
}

/// A pooled keep-alive connection the server has closed surfaces, on reuse, as hyper's
/// `IncompleteMessage` ("connection closed before message completed") — reqwest models it as a
/// request-kind error (`status()==None`, `is_request()==true`). It is the classic stale-pool race
/// and must be RETRYABLE: the request is idempotent-by-construction for this SDK's client-keyed
/// endpoints (client-generated uuid + server name-hash dedup), so a transient connection close
/// must not fail the call on the first attempt. Modelled by a server that reads the request then
/// closes without responding, which makes the client's response read hit the same
/// IncompleteMessage.
#[tokio::test]
async fn incomplete_message_is_retryable() {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	tokio::spawn(async move {
		let (mut socket, _) = listener.accept().await.unwrap();
		// Read the FULL request (until the header terminator) so the client has finished writing
		// before we close — otherwise a request split across TCP segments could fail on the
		// write side (a reset, not IncompleteMessage) and flake. Then close without responding so
		// the response read hits EOF -> "connection closed before message completed".
		let mut buf = Vec::new();
		let mut chunk = [0u8; 256];
		loop {
			match socket.read(&mut chunk).await {
				Ok(0) => break,
				Ok(n) => {
					buf.extend_from_slice(&chunk[..n]);
					if buf.windows(4).any(|w| w == b"\r\n\r\n") {
						break;
					}
				}
				Err(_) => break,
			}
		}
		drop(socket);
	});

	let client = ClientConfig::default().build_reqwest_client().unwrap();
	let err = super::execute_request(
		client.get(format!("http://{addr}/")),
		std::sync::Arc::default(),
	)
	.await
	.expect_err("a connection closed before responding must error");

	match err {
		super::retry::RetryError::Retry(_) => {}
		super::retry::RetryError::RetryAfterBackoff(e) => panic!(
			"IncompleteMessage is a response-read failure, not a pre-send connect failure, so \
				 it must retry immediately rather than pay a backoff: {e}"
		),
		super::retry::RetryError::NoRetry(e) => panic!(
			"IncompleteMessage (stale-pool connection close) must be classified retryable, \
				 but was NoRetry: {e}"
		),
	}
}

/// Answers a scripted server's request with an empty success.
async fn respond(socket: &mut TcpStream) {
	respond_with(socket, r#"{"status":true}"#).await;
}

/// Sends a lock request with `send` while requests the server never answers hold every place of
/// a `with_concurrency(1)` client's request pool (one chunk transfer and
/// `CONTROL_REQUEST_HEADROOM` control requests), answering it with `answer`. A lock request must
/// not wait for that pool.
async fn lock_request_beside_a_full_request_pool<F, Fut>(answer: &'static str, send: F)
where
	F: FnOnce(Arc<AuthClient>, String) -> Fut,
	Fut: Future<Output = Result<(), crate::Error>>,
{
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let (transferring, transfer_held) = tokio::sync::oneshot::channel();
	let (filled, pool_full) = tokio::sync::oneshot::channel();
	let server = tokio::spawn(async move {
		// every request but the lock's is read and never answered, so each keeps its places
		let (mut transfer, _) = listener.accept().await.unwrap();
		let head = read_head(&mut transfer).await;
		assert!(head.starts_with(Stall::Upload.connect_line()), "{head}");
		transferring.send(()).unwrap();
		let mut controls = Vec::new();
		for _ in 0..CONTROL_REQUEST_HEADROOM {
			let (mut control, _) = listener.accept().await.unwrap();
			let head = read_head(&mut control).await;
			assert!(head.starts_with(GATEWAY_CONNECT), "{head}");
			controls.push(control);
		}
		filled.send(()).unwrap();
		// plain http through a proxy: the request arrives in absolute form
		let (mut lock, _) = listener.accept().await.unwrap();
		let head = read_head(&mut lock).await;
		assert!(
			head.starts_with("POST http://lock.test/v3/user/lock"),
			"{head}"
		);
		respond_with(&mut lock, answer).await;
		// kept open until the client has read the answer: closing a socket with the request
		// body still unread resets the connection, which fails the request on Windows
		(transfer, controls, lock)
	});

	let client = Arc::new(client_via_proxy(
		ClientConfig::default().with_concurrency(1),
		addr,
	));
	let transfer = tokio::spawn({
		let client = Arc::clone(&client);
		async move { Stall::Upload.send(&client).await }
	});
	// each wait is bounded, so a pool that cannot fill fails the test instead of hanging it
	tokio::time::timeout(Duration::from_secs(10), transfer_held)
		.await
		.expect("the chunk transfer must reach the network")
		.expect("the scripted proxy reads the chunk transfer");
	let controls = (0..CONTROL_REQUEST_HEADROOM)
		.map(|_| {
			let client = Arc::clone(&client);
			tokio::spawn(async move {
				let _ = client.get_auth::<()>("v3/user/info".into()).await;
			})
		})
		.collect::<Vec<_>>();
	tokio::time::timeout(Duration::from_secs(10), pool_full)
		.await
		.expect("the control requests must fill the request pool")
		.expect("the scripted proxy reads every control request");
	tokio::time::timeout(
		Duration::from_secs(10),
		send(client, "http://lock.test/v3/user/lock".to_owned()),
	)
	.await
	.expect("the lock request must not queue behind the request pool")
	.unwrap();
	transfer.abort();
	for control in controls {
		control.abort();
	}
	drop(server.await.unwrap());
}

#[tokio::test]
async fn lock_acquires_do_not_wait_for_the_request_pool() {
	lock_request_beside_a_full_request_pool(LOCK_ANSWER, |client, url| async move {
		// the poll acquire_lock sends
		crate::sync::lock::send_acquire(&client, &url, Bytes::new())
			.await
			.map(drop)
	})
	.await;
}

/// What the server answers a lock request with.
const LOCK_ANSWER: &str =
	r#"{"status":true,"data":{"acquired":false,"released":true,"refreshed":true,"resource":"r"}}"#;

/// A lock request of `kind`, as the lock's keep-alive and its release send it.
async fn send_lock_request(
	client: Arc<AuthClient>,
	url: String,
	kind: LockType,
) -> Result<(), crate::Error> {
	crate::api::v3::user::lock::post(
		&client,
		&url,
		&crate::api::v3::user::lock::Request {
			uuid: Uuid::new_v4(),
			r#type: kind,
			resource: Cow::Borrowed("r"),
		},
	)
	.await
	.map(drop)
}

#[tokio::test]
async fn lock_refreshes_do_not_wait_for_the_request_pool() {
	lock_request_beside_a_full_request_pool(LOCK_ANSWER, |client, url| {
		send_lock_request(client, url, LockType::Refresh)
	})
	.await;
}

#[tokio::test]
async fn lock_releases_do_not_wait_for_the_request_pool() {
	lock_request_beside_a_full_request_pool(LOCK_ANSWER, |client, url| {
		send_lock_request(client, url, LockType::Release)
	})
	.await;
}

/// A keep-alive connection that dies AFTER the next request was written to it surfaces as
/// hyper's `SendRequest(Io)`, which hyper-util does not replay. On macOS runners pooled
/// connections died this way: lock polls failed with EADDRNOTAVAIL 20-27s after the send while
/// fresh connections made soon after worked. Modelled by a server that answers one request,
/// reads the next in full, then resets: the SDK must replay it on a fresh connection. The reset
/// has to follow the second request — one sent while the connection sits idle is noticed by
/// hyper before reuse and never reaches the SDK, so it would pass without the fix.
#[tokio::test]
async fn connection_reset_after_request_written_is_retried() {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	// Scripted, so running to the end proves the exact sequence: two requests on the first
	// connection, the second one reset, then one more on a fresh connection.
	let server = tokio::spawn(async move {
		let (mut pooled, _) = listener.accept().await.unwrap();
		read_head(&mut pooled).await;
		respond(&mut pooled).await;
		read_head(&mut pooled).await;
		// A reset, not a close: a FIN surfaces as IncompleteMessage, which is already retried.
		pooled.set_zero_linger().unwrap();
		drop(pooled);
		let (mut fresh, _) = listener.accept().await.unwrap();
		read_head(&mut fresh).await;
		respond(&mut fresh).await;
	});

	let client = client(ClientConfig::default());
	let url = format!("http://{addr}/v3/user/lock");
	for call in 1..=2 {
		// An empty body keeps each request to one header block: a non-empty one is streamed
		// chunked by the upload limiter, and the reset could then land before it is all read.
		client
			.post_raw_bytes_auth::<()>(Bytes::new(), &url, "v3/user/lock".into())
			.await
			.unwrap_or_else(|e| panic!("call {call} must succeed, got {e:?}"));
	}
	server.await.unwrap();
}
