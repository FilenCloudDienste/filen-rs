use axum_extra::headers::{Header, Range};

use crate::{
	auth::{http::ClientConfig, unauth::UnauthClient},
	http_provider::client_impl::HttpProviderSharedClientExt,
};

fn range_header(value: &str) -> Range {
	let value = http::HeaderValue::from_str(value).expect("valid header value");
	Range::decode(&mut std::iter::once(&value)).expect("valid range header")
}

/// A suffix range longer than the file (e.g. a blind MP4 tail fetch) must serve the whole
/// representation per RFC 9110 §14.1.2, not `416`. `satisfiable_ranges` drops the suffix, so
/// `resolve_ranges` has to recover the whole-file range itself.
#[test]
fn suffix_longer_than_file_serves_whole_representation() {
	let size = 10 * 1024;
	let raw = "bytes=-65536";
	let range = range_header(raw);
	// The header crate drops the oversized suffix, leaving nothing satisfiable...
	assert_eq!(range.satisfiable_ranges(size).count(), 0);
	// ...but we recover the entire representation instead of returning 416.
	assert_eq!(
		super::resolve_ranges(&range, Some(raw), size),
		vec![(0, size)]
	);
}

/// A suffix that fits inside the file is served as a normal partial range.
#[test]
fn suffix_within_file_is_served_as_partial() {
	let size = 10 * 1024;
	let raw = "bytes=-4096";
	let range = range_header(raw);
	assert_eq!(
		super::resolve_ranges(&range, Some(raw), size),
		vec![(size - 4096, size)]
	);
}

/// An ordinary `start-end` range is unaffected by the suffix handling.
#[test]
fn normal_range_is_resolved_to_half_open_bounds() {
	let size = 10 * 1024;
	let raw = "bytes=0-1023";
	let range = range_header(raw);
	assert_eq!(
		super::resolve_ranges(&range, Some(raw), size),
		vec![(0, 1024)]
	);
}

/// A genuinely unsatisfiable range (start past EOF, not a suffix) is still a 416, not a
/// whole-file fallback.
#[test]
fn out_of_range_non_suffix_stays_unsatisfiable() {
	let size = 10 * 1024;
	let raw = "bytes=20000-30000";
	let range = range_header(raw);
	assert!(super::resolve_ranges(&range, Some(raw), size).is_empty());
}

/// `has_oversized_suffix` fires only for a suffix strictly longer than the file — a suffix
/// equal to the size is satisfiable (the whole file) and non-suffix specs never match.
#[test]
fn has_oversized_suffix_detects_only_too_long_suffixes() {
	assert!(super::has_oversized_suffix("bytes=-65536", 10 * 1024));
	assert!(!super::has_oversized_suffix("bytes=-4096", 10 * 1024));
	assert!(!super::has_oversized_suffix("bytes=0-1023", 10 * 1024));
	assert!(!super::has_oversized_suffix("bytes=-1024", 1024));
	assert!(super::has_oversized_suffix("bytes=-1025", 1024));
}

/// An inclusive range end of `u64::MAX` (`bytes=0-18446744073709551615`) must not overflow when
/// converted to an exclusive bound. Per RFC it means "remainder of the representation", so it
/// clamps to the file size rather than panicking (overflow-checks) or wrapping to 0 (which would
/// wrongly yield 416).
#[test]
fn included_end_at_u64_max_clamps_to_size_without_overflow() {
	use std::ops::Bound;

	let size = 10 * 1024;
	assert_eq!(
		super::get_real_bounds(size, (Bound::Included(0), Bound::Included(u64::MAX))),
		(0, size)
	);
	// The same through the full range resolution from the raw header.
	let raw = "bytes=0-18446744073709551615";
	let range = range_header(raw);
	assert_eq!(
		super::resolve_ranges(&range, Some(raw), size),
		vec![(0, size)]
	);
}

/// When `start_http_provider` is called a second time while an existing provider is
/// still live (its `Arc` has not been dropped), the second call silently discards
/// the `port` argument and returns a handle to the already-running provider.
///
/// Hypothesis: the returned handle from both calls has the same port number, and that
/// port is the port chosen when the *first* provider was started — not the port
/// explicitly requested in the second call.
///
/// This is not a bug; it is the documented "only one provider at a time" contract.
/// The test exists to:
/// 1. Make the silent-port-discard contract explicit and visible in CI.
/// 2. Catch any future regression where the second call spawns a second provider
///    on the requested port instead of reusing the first.
#[tokio::test]
async fn test_start_http_provider_second_call_discards_port_arg() {
	let client = UnauthClient::from_config(ClientConfig::default())
		.expect("default ClientConfig should always succeed");

	// Start a provider on an OS-assigned ephemeral port.
	let handle1 = client
		.start_http_provider(None)
		.await
		.expect("first start_http_provider call should succeed");
	let port1 = handle1.port();
	assert_ne!(port1, 0, "OS should assign a non-zero ephemeral port");

	// Request a second provider on a *different* explicit port.
	// The current implementation returns the same provider as handle1.
	let handle2 = client
		.start_http_provider(Some(port1.wrapping_add(1)))
		.await
		.expect("second start_http_provider call should succeed");
	let port2 = handle2.port();

	// Both handles must refer to the same underlying server (same port).
	// If this assertion fails it means a second, independent provider was
	// created on the requested port — breaking the singleton guarantee.
	assert_eq!(
		port1, port2,
		"second call with a different port should reuse the existing provider \
			 (port1={port1}, port2={port2}); the `port` argument is silently ignored \
			 when a provider is already live"
	);

	// Verify the provider is still reachable on port1 (not the discarded port).
	let url = format!("http://127.0.0.1:{port1}/file?file=x");
	// We expect a 400 (bad `file` param) not a connection error — confirming the
	// server is alive on port1.
	let resp = reqwest::get(&url)
		.await
		.expect("server on port1 should accept connections");
	assert_eq!(
		resp.status(),
		400,
		"server should return 400 for an invalid `file` query param, not a connection error"
	);

	drop(handle1);
	drop(handle2);
}

/// Verifies that after all `Arc<HttpProviderHandle>` clones are dropped, the provider
/// stops and its port no longer accepts connections.
///
/// Hypothesis: the graceful-shutdown path (cancel_sender + axum graceful shutdown)
/// causes the server to stop within a reasonable time after the last handle is dropped.
#[tokio::test]
async fn test_start_http_provider_stops_after_all_handles_dropped() {
	let client = UnauthClient::from_config(ClientConfig::default())
		.expect("default ClientConfig should always succeed");

	let handle = client
		.start_http_provider(None)
		.await
		.expect("start_http_provider should succeed");
	let port = handle.port();

	// Confirm the server is up.
	let alive_resp = reqwest::get(format!("http://127.0.0.1:{port}/file?file=x"))
		.await
		.expect("server should be reachable while handle is live");
	assert_eq!(
		alive_resp.status(),
		400,
		"pre-drop: expect 400 for bad file param"
	);

	// Drop the only handle — this sends the cancel signal.
	drop(handle);

	// Give axum's graceful shutdown up to 1 s to complete.
	tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

	// The server should now refuse new connections.
	let dead_result = reqwest::get(format!("http://127.0.0.1:{port}/file?file=x")).await;
	assert!(
		dead_result.is_err(),
		"after all handles dropped and 1s elapsed, the server should no longer \
			 accept connections on port {port}"
	);
}

// ─── token store ──────────────────────────────────────────────────────────

/// Builds a distinct decoded file per `name`; equal inputs do NOT produce equal
/// files (uuid and timestamps are fresh), so tests that need "the same file"
/// must reuse one instance.
fn token_store_test_file(name: &str) -> crate::fs::file::enums::RemoteFileType<'static> {
	use std::borrow::Cow;

	use chrono::Utc;
	use filen_types::{
		auth::FileEncryptionVersion,
		fs::{ParentUuid, Uuid},
	};

	use crate::{
		crypto::file::FileKey,
		fs::file::{
			RemoteFile,
			enums::RemoteFileType,
			meta::{DecryptedFileMeta, FileMeta},
		},
	};

	let uuid = Uuid::new_v4();
	// The url token store keeps no file identity, so the fixture builds the
	// same shape the read path does.
	RemoteFileType::File(Cow::Owned(RemoteFile {
		uuid,
		stable_uuid: (),
		parent: ParentUuid::Links,
		size: 10,
		favorited: false,
		region: "de-1".to_string(),
		bucket: "bucket-a".to_string(),
		timestamp: Utc::now(),
		chunks: 1,
		meta: FileMeta::Decoded(DecryptedFileMeta {
			name: Cow::Owned(name.to_string()),
			size: 10,
			mime: Cow::Borrowed("text/plain"),
			key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3)
				.unwrap(),
			last_modified: Utc::now(),
			created: None,
			hash: None,
		}),
	}))
}

fn encode_token(token: super::UrlToken) -> String {
	use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
	BASE64_URL_SAFE_NO_PAD.encode(token)
}

/// The app re-derives a file's URL on every preview mount, so minting must be
/// idempotent per file: the same file reuses its live token instead of growing
/// the store.
#[test]
fn token_store_reuses_the_token_of_an_equal_file() {
	let mut store = super::TokenStore::new();
	let file = token_store_test_file("a.txt");

	let first = store.token_for(file.clone());
	let second = store.token_for(file);

	assert_eq!(first, second, "an equal file must reuse its live token");
	assert_eq!(
		store.entries.len(),
		1,
		"deduplication must not grow the store"
	);
}

#[test]
fn token_store_gives_distinct_files_distinct_tokens() {
	let mut store = super::TokenStore::new();

	let a = store.token_for(token_store_test_file("a.txt"));
	let b = store.token_for(token_store_test_file("b.txt"));

	assert_ne!(a, b, "distinct files must not share a token");
	assert_eq!(store.entries.len(), 2);
}

/// The bound that fixes the unbounded-growth leak: past [`super::MAX_URL_TOKENS`]
/// distinct files, the least recently used entry is evicted and its URL dies.
#[test]
fn token_store_caps_at_max_and_evicts_the_least_recently_used() {
	let mut store = super::TokenStore::new();
	let first = store.token_for(token_store_test_file("first.txt"));

	for i in 0..super::MAX_URL_TOKENS {
		store.token_for(token_store_test_file(&format!("{i}.txt")));
	}

	assert_eq!(
		store.entries.len(),
		super::MAX_URL_TOKENS,
		"the store must never exceed its cap"
	);
	assert!(
		store.resolve(&encode_token(first)).is_none(),
		"the oldest untouched entry must have been evicted"
	);
}

/// Resolving counts as use: an actively streamed URL (the player hits it with
/// range requests) must survive eviction pressure from newly minted URLs.
#[test]
fn token_store_resolve_keeps_an_entry_warm_across_eviction() {
	let mut store = super::TokenStore::new();
	let streaming = store.token_for(token_store_test_file("streaming.mp4"));

	// Fill the store to its cap, leaving `streaming` the LRU candidate...
	for i in 0..super::MAX_URL_TOKENS - 1 {
		store.token_for(token_store_test_file(&format!("{i}.txt")));
	}
	// ...then touch it the way a playing media player does.
	assert!(store.resolve(&encode_token(streaming)).is_some());

	store.token_for(token_store_test_file("overflow.txt"));

	assert!(
		store.resolve(&encode_token(streaming)).is_some(),
		"a recently resolved entry must not be the one evicted"
	);
	assert_eq!(store.entries.len(), super::MAX_URL_TOKENS);
}

/// Malformed, wrong-length, and unknown tokens must all resolve to `None` (the
/// handler's 404), never panic.
#[test]
fn token_store_rejects_malformed_and_unknown_tokens() {
	let mut store = super::TokenStore::new();
	store.token_for(token_store_test_file("a.txt"));

	assert!(
		store.resolve("not base64 !!!").is_none(),
		"malformed base64"
	);
	assert!(
		store.resolve("c2hvcnQ").is_none(),
		"valid base64, wrong length"
	);
	assert!(
		store.resolve(&encode_token(rand::random())).is_none(),
		"well-formed but unknown token"
	);
}

// ─── read-ahead window ────────────────────────────────────────────────────

#[test]
fn read_ahead_window_applies_default_and_clamps() {
	// A practically-unbounded budget, so the half-budget cap never binds and this exercises
	// only the default and the `[MIN, MAX]` clamp.
	let big = usize::MAX;
	assert_eq!(
		super::effective_read_ahead(None, big),
		super::DEFAULT_READ_AHEAD_BYTES,
		"absent `buffer` should use the 8 MiB default"
	);
	assert_eq!(
		super::effective_read_ahead(Some(0), big),
		super::MIN_READ_AHEAD_BYTES,
		"a zero/too-small window must be clamped up to one chunk"
	);
	assert_eq!(
		super::effective_read_ahead(Some(u64::MAX), big),
		super::MAX_READ_AHEAD_BYTES,
		"an oversized window must be clamped down to the cap"
	);
	let in_range = 4 * 1024 * 1024;
	assert_eq!(
		super::effective_read_ahead(Some(in_range), big),
		in_range,
		"an in-range window must pass through unchanged"
	);
}

/// The window is capped at half the shared memory budget, so a single stream can never pin more
/// than half the budget and starve a concurrent download.
#[test]
fn read_ahead_capped_at_half_budget() {
	let one_chunk = super::CHUNK_SIZE_U64 + u64::from(super::FILE_CHUNK_SIZE_EXTRA.get());

	// A 4-chunk budget: half is 2 chunks, which caps the otherwise-8-MiB default window.
	let four_chunks = usize::try_from(one_chunk * 4).unwrap();
	assert_eq!(
		super::effective_read_ahead(None, four_chunks),
		one_chunk * 2,
		"the default window must be capped to half the budget (2 of 4 chunks)"
	);
	assert_eq!(
		super::effective_read_ahead(Some(u64::MAX), four_chunks),
		one_chunk * 2,
		"a huge requested window is still capped to half the budget"
	);

	// A 2-chunk budget (the iOS default): half is exactly one chunk, the minimum window.
	let two_chunks = usize::try_from(one_chunk * 2).unwrap();
	assert_eq!(
		super::effective_read_ahead(None, two_chunks),
		super::MIN_READ_AHEAD_BYTES,
		"on a 2-chunk budget the window is exactly one chunk (half the budget)"
	);
}

// ─── write idle timeout ───────────────────────────────────────────────────

/// A peer that stopped reading is modelled by a writer whose `poll_write` never
/// completes. The wrapper must surface a `TimedOut` error once the idle window elapses.
#[tokio::test(start_paused = true)]
async fn idle_timeout_fires_when_writes_stall() {
	use std::pin::Pin;
	use std::task::{Context, Poll};
	use tokio::io::{AsyncWrite, AsyncWriteExt as _};

	struct StalledWriter;
	impl AsyncWrite for StalledWriter {
		fn poll_write(
			self: Pin<&mut Self>,
			_: &mut Context<'_>,
			_: &[u8],
		) -> Poll<std::io::Result<usize>> {
			Poll::Pending
		}
		fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
			Poll::Ready(Ok(()))
		}
		fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
			Poll::Ready(Ok(()))
		}
	}

	let mut writer = super::IdleTimeout::new(StalledWriter, std::time::Duration::from_secs(30));
	// With paused time the runtime auto-advances to the armed deadline, so this resolves
	// without any real waiting.
	let err = writer
		.write(b"hello")
		.await
		.expect_err("a stalled write must time out");
	assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
}

/// A writer that always accepts data must never trip the idle timeout.
#[tokio::test]
async fn idle_timeout_does_not_fire_while_progressing() {
	use std::pin::Pin;
	use std::task::{Context, Poll};
	use tokio::io::{AsyncWrite, AsyncWriteExt as _};

	struct ReadyWriter;
	impl AsyncWrite for ReadyWriter {
		fn poll_write(
			self: Pin<&mut Self>,
			_: &mut Context<'_>,
			buf: &[u8],
		) -> Poll<std::io::Result<usize>> {
			Poll::Ready(Ok(buf.len()))
		}
		fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
			Poll::Ready(Ok(()))
		}
		fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
			Poll::Ready(Ok(()))
		}
	}

	let mut writer = super::IdleTimeout::new(ReadyWriter, std::time::Duration::from_millis(1));
	for _ in 0..1000 {
		let n = writer
			.write(b"chunk")
			.await
			.expect("progressing write must succeed");
		assert_eq!(n, 5);
	}
}

/// A reader whose `poll_read` never completes (a peer that connected but sends nothing) must
/// trip the same idle timeout — the read-side mirror of the write-stall case.
#[tokio::test(start_paused = true)]
async fn idle_timeout_fires_when_reads_stall() {
	use std::pin::Pin;
	use std::task::{Context, Poll};
	use tokio::io::{AsyncRead, AsyncReadExt as _, ReadBuf};

	struct StalledReader;
	impl AsyncRead for StalledReader {
		fn poll_read(
			self: Pin<&mut Self>,
			_: &mut Context<'_>,
			_: &mut ReadBuf<'_>,
		) -> Poll<std::io::Result<()>> {
			Poll::Pending
		}
	}

	let mut reader = super::IdleTimeout::new(StalledReader, std::time::Duration::from_secs(30));
	let err = reader
		.read(&mut [0u8; 8])
		.await
		.expect_err("a stalled read must time out");
	assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
}

/// A reader that always yields data must never trip the idle timeout.
#[tokio::test]
async fn idle_timeout_does_not_fire_while_reads_progress() {
	use std::pin::Pin;
	use std::task::{Context, Poll};
	use tokio::io::{AsyncRead, AsyncReadExt as _, ReadBuf};

	struct ReadyReader;
	impl AsyncRead for ReadyReader {
		fn poll_read(
			self: Pin<&mut Self>,
			_: &mut Context<'_>,
			buf: &mut ReadBuf<'_>,
		) -> Poll<std::io::Result<()>> {
			buf.put_slice(&[0u8; 8]);
			Poll::Ready(Ok(()))
		}
	}

	let mut reader = super::IdleTimeout::new(ReadyReader, std::time::Duration::from_millis(1));
	for _ in 0..1000 {
		let n = reader
			.read(&mut [0u8; 8])
			.await
			.expect("progressing read must succeed");
		assert_eq!(n, 8);
	}
}

/// End-to-end (read side): a client that connects but never sends a request is closed by the
/// server's idle timeout — the read-side mirror of `timeout_listener_closes_abandoned_stream`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_listener_closes_a_silent_client() {
	use std::time::Duration;
	use tokio::io::AsyncReadExt as _;

	async fn hello() -> &'static str {
		"ok"
	}
	let router = axum::Router::new().route("/", axum::routing::get(hello));
	let tcp = tokio::net::TcpListener::bind(("127.0.0.1", 0))
		.await
		.unwrap();
	let port = tcp.local_addr().unwrap().port();
	let listener = super::TimeoutListener {
		inner: tcp,
		idle_timeout: Duration::from_millis(500),
	};
	let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
	let server = tokio::spawn(async move {
		axum::serve(listener, router)
			.with_graceful_shutdown(async {
				let _ = cancel_rx.await;
			})
			.await
			.unwrap();
	});

	// Connect but send nothing: the server's request read stalls and, after the 500 ms idle
	// timeout, the connection is dropped.
	let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
		.await
		.unwrap();
	let closed = tokio::time::timeout(Duration::from_secs(5), async {
		let mut sink = [0u8; 1024];
		loop {
			match sock.read(&mut sink).await {
				Ok(0) => return true,
				Ok(_) => continue,
				Err(_) => return true,
			}
		}
	})
	.await
	.unwrap_or(false);
	assert!(
		closed,
		"server must close a silent (never-sending) client via the read-idle timeout"
	);

	let _ = cancel_tx.send(());
	let _ = server.await;
}

/// End-to-end: a slow download — the response body stalls (no writes) for longer than
/// `idle_timeout` while the client keeps reading — must NOT be reaped. The idle timer only
/// engages on real IO back-pressure (a `poll_write` that parks because the client stopped
/// reading, or a `poll_read` that parks awaiting the request); a server-side body-produce stall
/// is neither (the send buffer drains as the client reads), so the connection survives. Guards
/// against a future change that reaps a legitimately slow download from the egest server.
/// (Empirically this passes with or without the `poll_flush` empty-buffer reset, so that reset
/// is not the load-bearing mechanism here.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_listener_keeps_a_slow_download_alive() {
	use std::time::Duration;
	use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

	// Two body frames with a pause LONGER than the idle timeout between them, modelling a slow
	// upstream chunk fetch while the client reads steadily.
	async fn slow_body() -> axum::response::Response {
		let stream = async_stream::stream! {
			yield Ok::<_, std::io::Error>(b"<<FIRST>>".to_vec());
			tokio::time::sleep(Duration::from_millis(600)).await;
			yield Ok::<_, std::io::Error>(b"<<SECOND>>".to_vec());
		};
		axum::response::Response::new(axum::body::Body::from_stream(stream))
	}

	let router = axum::Router::new().route("/", axum::routing::get(slow_body));
	let tcp = tokio::net::TcpListener::bind(("127.0.0.1", 0))
		.await
		.unwrap();
	let port = tcp.local_addr().unwrap().port();
	let listener = super::TimeoutListener {
		inner: tcp,
		idle_timeout: Duration::from_millis(200), // < the 600 ms inter-frame stall
	};
	let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
	let server = tokio::spawn(async move {
		axum::serve(listener, router)
			.with_graceful_shutdown(async {
				let _ = cancel_rx.await;
			})
			.await
			.unwrap();
	});

	let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
		.await
		.unwrap();
	sock.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
		.await
		.unwrap();

	// The client reads steadily across the >idle_timeout stall; the second frame arriving proves
	// the connection survived (chunked body, so the frame bytes appear verbatim in the stream).
	let got_second_frame = tokio::time::timeout(Duration::from_secs(5), async {
		let mut all = Vec::new();
		let mut buf = [0u8; 1024];
		loop {
			match sock.read(&mut buf).await {
				Ok(0) => break,
				Ok(n) => {
					all.extend_from_slice(&buf[..n]);
					if all.windows(10).any(|w| w == b"<<SECOND>>") {
						return true;
					}
				}
				Err(_) => break,
			}
		}
		false
	})
	.await
	.unwrap_or(false);

	assert!(
		got_second_frame,
		"a slow download (stall > idle_timeout while the client keeps reading) must complete, \
			 not be reaped by the idle timer"
	);

	let _ = cancel_tx.send(());
	let _ = server.await;
}

/// End-to-end: an abandoned streaming connection (client stops reading but keeps the
/// socket open) is closed by the server's write-idle timeout, dropping the reader behind
/// it. Uses the real hyper/axum stack via [`super::TimeoutListener`].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_listener_closes_abandoned_stream() {
	use std::time::Duration;
	use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

	// Infinite body: the server keeps trying to write, so it backpressures the moment
	// the client stops reading.
	async fn infinite() -> axum::response::Response {
		let stream = async_stream::stream! {
			loop {
				yield Ok::<_, std::io::Error>(vec![0u8; 256 * 1024]);
			}
		};
		axum::response::Response::new(axum::body::Body::from_stream(stream))
	}

	let router = axum::Router::new().route("/", axum::routing::get(infinite));
	let tcp = tokio::net::TcpListener::bind(("127.0.0.1", 0))
		.await
		.unwrap();
	let port = tcp.local_addr().unwrap().port();
	let listener = super::TimeoutListener {
		inner: tcp,
		idle_timeout: Duration::from_millis(500),
	};
	let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
	let server = tokio::spawn(async move {
		axum::serve(listener, router)
			.with_graceful_shutdown(async {
				let _ = cancel_rx.await;
			})
			.await
			.unwrap();
	});

	let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
		.await
		.unwrap();
	sock.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
		.await
		.unwrap();
	// Kick the response off, then stop reading: the server's writes stall and, after the
	// 500 ms write-idle timeout, the connection is dropped.
	let mut head = [0u8; 1024];
	let _ = sock.read(&mut head).await.unwrap();
	tokio::time::sleep(Duration::from_secs(2)).await;

	// Draining must reach EOF/reset because the server closed the connection.
	let closed = tokio::time::timeout(Duration::from_secs(5), async {
		let mut sink = vec![0u8; 256 * 1024];
		loop {
			match sock.read(&mut sink).await {
				Ok(0) => return true,
				Ok(_) => continue,
				Err(_) => return true,
			}
		}
	})
	.await
	.unwrap_or(false);
	assert!(
		closed,
		"server must close an abandoned (non-reading) connection via the write-idle timeout"
	);

	let _ = cancel_tx.send(());
	let _ = server.await;
}

/// The reclaim guarantee at the heart of the fix: closing an abandoned stream must DROP the
/// response body — and with it whatever budget the streaming reader was holding. A
/// [`FileReader`](crate::fs::file::read::FileReader) serving a range holds `Chunk::acquire`
/// permits from the shared `memory_semaphore` for its whole lifetime; the original bug was that
/// an abandoned connection never dropped that reader, so the permits (the ~96 MiB budget) were
/// never returned and every later download blocked forever on `Chunk::acquire`. Here the budget
/// is modelled by a [`tokio::sync::Semaphore`] (the same primitive) whose permit the response
/// body holds; the test asserts the write-idle timeout reclaims it. Without the timeout firing
/// the permit would stay pinned forever — which is exactly the leak this fix closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_timeout_reclaims_the_budget_held_by_an_abandoned_stream() {
	use std::sync::Arc;
	use std::time::Duration;
	use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
	use tokio::sync::Semaphore;

	// The shared per-client memory budget, modelled with a single permit.
	let budget = Arc::new(Semaphore::new(1));

	let handler_budget = budget.clone();
	// Each response acquires the budget permit and moves it into its (infinite) body stream,
	// so the permit lives exactly as long as the body — like a FileReader holding Chunk
	// permits while it serves a range — and is released only when the body is dropped.
	let handler = move || {
		let budget = handler_budget.clone();
		async move {
			let permit = budget.acquire_owned().await.unwrap();
			let stream = async_stream::stream! {
				let _permit = permit;
				loop {
					yield Ok::<_, std::io::Error>(vec![0u8; 256 * 1024]);
				}
			};
			axum::response::Response::new(axum::body::Body::from_stream(stream))
		}
	};

	let router = axum::Router::new().route("/", axum::routing::get(handler));
	let tcp = tokio::net::TcpListener::bind(("127.0.0.1", 0))
		.await
		.unwrap();
	let port = tcp.local_addr().unwrap().port();
	let listener = super::TimeoutListener {
		inner: tcp,
		idle_timeout: Duration::from_millis(500),
	};
	let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
	let server = tokio::spawn(async move {
		axum::serve(listener, router)
			.with_graceful_shutdown(async {
				let _ = cancel_rx.await;
			})
			.await
			.unwrap();
	});

	let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
		.await
		.unwrap();
	sock.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
		.await
		.unwrap();
	// Read just the head so the handler runs and the body takes the budget permit, then stop
	// reading: the server's writes stall against the full socket buffer.
	let mut head = [0u8; 1024];
	let _ = sock.read(&mut head).await.unwrap();

	let acquired = tokio::time::timeout(Duration::from_secs(2), async {
		loop {
			if budget.available_permits() == 0 {
				return true;
			}
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
	})
	.await
	.unwrap_or(false);
	assert!(
		acquired,
		"the streaming body must hold the budget permit while it is serving"
	);

	// After the 500 ms write-idle timeout fires, the connection — and the body it owns — is
	// dropped, releasing the permit. The original leak would leave it pinned at 0 forever.
	let reclaimed = tokio::time::timeout(Duration::from_secs(5), async {
		loop {
			if budget.available_permits() == 1 {
				return true;
			}
			tokio::time::sleep(Duration::from_millis(25)).await;
		}
	})
	.await
	.unwrap_or(false);
	assert!(
		reclaimed,
		"write-idle timeout must drop the abandoned stream's body and reclaim its budget"
	);

	let _ = cancel_tx.send(());
	let _ = server.await;
}
