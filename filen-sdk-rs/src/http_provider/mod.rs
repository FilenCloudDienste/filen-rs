use std::{
	future::Future,
	io,
	ops::Bound,
	pin::Pin,
	sync::Arc,
	task::{Context, Poll},
	time::Duration,
};

use axum::{
	extract::{Query, State},
	response::{IntoResponse, Response},
	routing::get,
};
use axum_extra::{TypedHeader, headers::Range};
use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use futures::AsyncReadExt;
use http::{
	StatusCode,
	header::{CONTENT_LENGTH, CONTENT_TYPE},
	response,
};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::{
	Error,
	auth::unauth::UnauthClient,
	consts::{CHUNK_SIZE_U64, FILE_CHUNK_SIZE_EXTRA},
	fs::file::{enums::RemoteFileType, read::FileReaderBuilder},
	io::HasFileInfo,
};

/// Default streaming read-ahead window (8 MiB) used when the URL omits `?buffer=`.
///
/// This bounds how much a single stream prefetches and therefore how much of the shared
/// per-client memory budget it can pin. A small window keeps an abandoned/paused stream
/// cheap (like a normal file server's small copy buffer) instead of letting it greedily
/// acquire the entire budget and starve concurrent downloads.
const DEFAULT_READ_AHEAD_BYTES: u64 = 8 * 1024 * 1024;
/// Lower bound for the read-ahead window: one full *encrypted* chunk (plaintext + the 28-byte
/// auth-tag/nonce overhead) so the reader can always eagerly prefetch at least one chunk. Using
/// the bare plaintext size would leave the minimum window one chunk short and prefetch nothing.
const MIN_READ_AHEAD_BYTES: u64 = CHUNK_SIZE_U64 + FILE_CHUNK_SIZE_EXTRA.get() as u64;
/// Upper bound for the read-ahead window: keep one stream from pinning the whole budget.
const MAX_READ_AHEAD_BYTES: u64 = 64 * 1024 * 1024;
/// A connection that cannot make write progress for this long (the peer stopped reading and
/// the socket buffers are full) is dropped, which tears down the streaming reader behind it
/// and frees its budget. Mirrors nginx's `send_timeout`.
const WRITE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Resolves the effective read-ahead window from the optional `?buffer=` query value: applies the
/// default, clamps to `[MIN, MAX]`, then caps at half the shared memory `budget` so a single stream
/// can never pin more than half the budget — always leaving room for a concurrent download.
/// [`SharedClientState::new`](crate::auth::Client) rejects a budget whose half is below one chunk,
/// so the budget cap never pushes the window below [`MIN_READ_AHEAD_BYTES`].
fn effective_read_ahead(requested: Option<u64>, budget: usize) -> u64 {
	requested
		.unwrap_or(DEFAULT_READ_AHEAD_BYTES)
		.clamp(MIN_READ_AHEAD_BYTES, MAX_READ_AHEAD_BYTES)
		.min((budget / 2) as u64)
}

fn write_idle_timeout_error() -> io::Error {
	io::Error::new(
		io::ErrorKind::TimedOut,
		"http provider: write idle timeout (peer stopped reading)",
	)
}

/// Wraps a connection's IO with a write **idle** timeout.
///
/// When the peer stops reading, the socket send buffer fills and `poll_write` returns
/// `Pending` indefinitely — hyper parks the response body (and the streaming reader it owns)
/// without ever dropping it. This wrapper arms a timer on the first stalled write and fails
/// the IO with [`io::ErrorKind::TimedOut`] once `timeout` elapses with no progress, which
/// tears the connection down so the reader is dropped and its budget reclaimed. The timer is
/// reset on every successful write, so a slow-but-progressing client is never penalised.
struct WriteIdleTimeout<S> {
	inner: S,
	timeout: Duration,
	idle: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S> WriteIdleTimeout<S> {
	fn new(inner: S, timeout: Duration) -> Self {
		Self {
			inner,
			timeout,
			idle: None,
		}
	}

	/// Arms the idle timer if needed and polls it. `Ready` means the deadline elapsed.
	fn poll_idle_elapsed(&mut self, cx: &mut Context<'_>) -> Poll<()> {
		let timeout = self.timeout;
		self.idle
			.get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)))
			.as_mut()
			.poll(cx)
	}
}

impl<S: AsyncWrite + Unpin> AsyncWrite for WriteIdleTimeout<S> {
	fn poll_write(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &[u8],
	) -> Poll<io::Result<usize>> {
		let this = self.get_mut();
		match Pin::new(&mut this.inner).poll_write(cx, buf) {
			Poll::Ready(res) => {
				this.idle = None;
				Poll::Ready(res)
			}
			Poll::Pending => match this.poll_idle_elapsed(cx) {
				Poll::Ready(()) => Poll::Ready(Err(write_idle_timeout_error())),
				Poll::Pending => Poll::Pending,
			},
		}
	}

	fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		let this = self.get_mut();
		match Pin::new(&mut this.inner).poll_flush(cx) {
			Poll::Ready(res) => {
				this.idle = None;
				Poll::Ready(res)
			}
			Poll::Pending => match this.poll_idle_elapsed(cx) {
				Poll::Ready(()) => Poll::Ready(Err(write_idle_timeout_error())),
				Poll::Pending => Poll::Pending,
			},
		}
	}

	fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
	}
}

impl<S: AsyncRead + Unpin> AsyncRead for WriteIdleTimeout<S> {
	fn poll_read(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &mut ReadBuf<'_>,
	) -> Poll<io::Result<()>> {
		Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
	}
}

/// Per-connection accept errors that are safe to retry immediately — the listener itself is still
/// healthy (the peer just went away between the kernel accept and ours). Matches axum's own
/// `is_connection_error`; anything else (e.g. fd exhaustion) is treated as persistent.
fn is_connection_error(e: &io::Error) -> bool {
	matches!(
		e.kind(),
		io::ErrorKind::ConnectionRefused
			| io::ErrorKind::ConnectionAborted
			| io::ErrorKind::ConnectionReset
	)
}

/// An [`axum::serve::Listener`] that wraps every accepted connection in a [`WriteIdleTimeout`],
/// so abandoned streaming connections are dropped instead of pinning a reader forever.
struct TimeoutListener {
	inner: tokio::net::TcpListener,
	write_idle_timeout: Duration,
}

impl axum::serve::Listener for TimeoutListener {
	type Io = WriteIdleTimeout<tokio::net::TcpStream>;
	type Addr = std::net::SocketAddr;

	async fn accept(&mut self) -> (Self::Io, Self::Addr) {
		loop {
			match self.inner.accept().await {
				Ok((stream, addr)) => {
					return (WriteIdleTimeout::new(stream, self.write_idle_timeout), addr);
				}
				// A per-connection error is transient and the listener stays healthy, so retry
				// immediately (matching axum). Anything else is likely persistent (e.g. fd
				// exhaustion): surface it and back off a full second so we don't hot-loop
				// accept+log at ~1 kHz, burning CPU/battery with the fault invisible at debug.
				Err(e) if is_connection_error(&e) => continue,
				Err(e) => {
					tracing::error!("http provider accept error: {e}");
					tokio::time::sleep(Duration::from_secs(1)).await;
				}
			}
		}
	}

	fn local_addr(&self) -> io::Result<Self::Addr> {
		self.inner.local_addr()
	}
}

pub mod client_impl;
#[cfg(feature = "uniffi")]
mod js_impl;

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct HttpProviderHandle {
	task: Option<tokio::task::JoinHandle<()>>,
	cancel_sender: Option<tokio::sync::oneshot::Sender<()>>,
	port: u16,
}

impl Drop for HttpProviderHandle {
	fn drop(&mut self) {
		if let Some(cancel_sender) = self.cancel_sender.take() {
			let _ = cancel_sender.send(());
		}

		if let Some(task) = self.task.take() {
			match tokio::runtime::Handle::try_current() {
				Ok(runtime_handle) => {
					runtime_handle.spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        if !task.is_finished() {
                            tracing::error!("HTTPProviderCanceller was dropped but the task is still running after 10 seconds. Forcing abort.");
                            task.abort();
                        }
                    });
				}
				Err(_) => {
					std::thread::spawn(move || {
						std::thread::sleep(std::time::Duration::from_secs(10));
						if !task.is_finished() {
							tracing::error!(
								"HTTPProviderCanceller was dropped but the task is still running after 10 seconds. Forcing abort."
							);
							task.abort();
						}
					});
				}
			}
		}
	}
}

impl HttpProviderHandle {
	pub(crate) async fn new(port: Option<u16>, client: Arc<UnauthClient>) -> Result<Self, Error> {
		let router = axum::Router::new()
			.route("/file", get(file_handler))
			.with_state(ProviderState { client });

		let (port_sender, port_receiver) = tokio::sync::oneshot::channel();
		let (cancel_sender, cancel_receiver) = tokio::sync::oneshot::channel();

		let task =
			tokio::task::spawn(async move {
				let listener =
					match tokio::net::TcpListener::bind(("127.0.0.1", port.unwrap_or_default()))
						.await
					{
						Ok(listener) => listener,
						Err(e) => {
							let _ = port_sender.send(Err(e));
							return;
						}
					};

				let _ = port_sender.send(listener.local_addr().map(|addr| addr.port()));
				let listener = TimeoutListener {
					inner: listener,
					write_idle_timeout: WRITE_IDLE_TIMEOUT,
				};
				axum::serve(listener, router)
					.with_graceful_shutdown(async {
						let _ = cancel_receiver.await;
					})
					.await
					.expect("Failed to start HTTP server");
			});
		let port = port_receiver.await.unwrap()?;

		Ok(Self {
			task: Some(task),
			cancel_sender: Some(cancel_sender),
			port,
		})
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl HttpProviderHandle {
	pub fn port(&self) -> u16 {
		self.port
	}
}

impl HttpProviderHandle {
	/// Returns the local HTTP URL that serves the given file through this provider.
	///
	/// The URL encodes the file metadata as a msgpack+base64url query parameter so
	/// that the server can reconstruct the file reader without any additional RPC.
	pub fn get_file_url(&self, file: &RemoteFileType<'_>) -> String {
		let msgpack = rmp_serde::to_vec(&file)
			.expect("RemoteFile serialization to msgpack should never fail");
		let encoded = BASE64_URL_SAFE_NO_PAD.encode(&msgpack);

		format!("http://127.0.0.1:{}/file?file={}", self.port, encoded)
	}

	/// Like [`get_file_url`](Self::get_file_url) but pins this stream's read-ahead window
	/// (in bytes) via the `buffer` query parameter. The server clamps it to a sane range
	/// (`[1 chunk, 64 MiB]`). Use a small value for latency-sensitive previews/thumbnails
	/// and a larger one for sustained playback; the default when omitted is 8 MiB.
	pub fn get_file_url_with_buffer_size(
		&self,
		file: &RemoteFileType<'_>,
		buffer_bytes: u64,
	) -> String {
		format!("{}&buffer={}", self.get_file_url(file), buffer_bytes)
	}
}

#[cfg(feature = "uniffi")]
#[uniffi::export]
impl HttpProviderHandle {
	#[uniffi::method(name = "getFileUrl")]
	pub fn get_file_url_uniffi(&self, file: crate::js::AnyFile) -> Result<String, Error> {
		Ok(self.get_file_url(&file.try_into()?))
	}

	#[uniffi::method(name = "getFileUrlWithBufferSize")]
	pub fn get_file_url_with_buffer_size_uniffi(
		&self,
		file: crate::js::AnyFile,
		buffer_bytes: u64,
	) -> Result<String, Error> {
		Ok(self.get_file_url_with_buffer_size(&file.try_into()?, buffer_bytes))
	}
}

#[derive(Clone)]
struct ProviderState {
	client: Arc<UnauthClient>,
}

mod custom_serde {
	use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
	use serde::Deserializer;

	use crate::fs::file::enums::RemoteFileType;

	pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<RemoteFileType<'static>, D::Error>
	where
		D: Deserializer<'de>,
	{
		let string = filen_types::serde::cow::deserialize(deserializer)?;
		let bytes = BASE64_URL_SAFE_NO_PAD
			.decode(string.as_ref())
			.map_err(serde::de::Error::custom)?;
		rmp_serde::from_slice::<RemoteFileType<'static>>(&bytes).map_err(serde::de::Error::custom)
	}
}

#[derive(Deserialize)]
struct FileQuery {
	#[serde(deserialize_with = "custom_serde::deserialize")]
	file: RemoteFileType<'static>,
	/// Optional read-ahead window in bytes (`?buffer=`); bounds how much this stream
	/// prefetches and therefore how much of the shared memory budget it can pin.
	#[serde(default)]
	buffer: Option<u64>,
}

fn get_real_bounds(file: &RemoteFileType, (start, end): (Bound<u64>, Bound<u64>)) -> (u64, u64) {
	let start = match start {
		Bound::Included(start) => start,
		Bound::Excluded(start) => start + 1,
		Bound::Unbounded => 0,
	};
	let end = match end {
		Bound::Included(end) => end + 1,
		Bound::Excluded(end) => end,
		Bound::Unbounded => u64::MAX,
	}
	.min(file.size());
	(start, end)
}

fn single_range_response_builder(
	file: RemoteFileType<'static>,
	(start, end): (u64, u64),
	read_ahead: u64,
	client: Arc<UnauthClient>,
	mut builder: response::Builder,
) -> Result<Response, http::Error> {
	let size = file.size();

	let status = if start == 0 && end == size {
		StatusCode::OK
	} else {
		StatusCode::PARTIAL_CONTENT
	};

	builder = builder
		.status(status)
		.header(CONTENT_LENGTH, end.saturating_sub(start).to_string())
		.header(
			CONTENT_TYPE,
			file.mime().unwrap_or("application/octet-stream"),
		);

	let stream = async_stream::stream! {
		let mut reader = FileReaderBuilder::new(&client, &file)
			.with_start(start)
			.with_end(end)
			.with_max_buffer_size(read_ahead)
			.build();
		let mut buf = vec![0; 8192];
		loop {
			match reader.read(&mut buf).await {
				Ok(0) => break, // EOF
				Ok(n) => yield Ok(buf[..n].to_vec()),
				Err(e) => {
					yield Err(e);
				}
			}
		}
	};

	if status == StatusCode::PARTIAL_CONTENT {
		let content_range_value = format!("bytes {}-{}/{}", start, end - 1, size);
		builder = builder.header(http::header::CONTENT_RANGE, content_range_value);
	}
	builder.body(axum::body::Body::from_stream(stream))
}

fn multiple_range_response_builder(
	file: RemoteFileType<'static>,
	ranges: Vec<(u64, u64)>,
	read_ahead: u64,
	client: Arc<UnauthClient>,
	mut builder: response::Builder,
) -> Result<Response, http::Error> {
	let boundary = BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 16]>());
	builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
		CONTENT_TYPE,
		format!("multipart/byteranges; boundary={}", boundary),
	);

	let stream = async_stream::stream! {
		let mime = file.mime().unwrap_or("application/octet-stream");
		let mut first = true;
		for (start, end) in ranges {
			// RFC 2046 §5.1: encapsulation boundary is CRLF + "--" + boundary.
			// The first boundary has no leading CRLF (it is the dash-boundary itself).
			let prefix = if first { "" } else { "\r\n" };
			first = false;
			let header = format!(
				"{prefix}--{boundary}\r\nContent-Type: {mime}\r\nContent-Range: bytes {start}-{end_incl}/{size}\r\n\r\n",
				end_incl = end.saturating_sub(1),
				size = file.size(),
			);
			yield Ok(header.into_bytes());
			let mut reader = FileReaderBuilder::new(&client, &file)
				.with_start(start)
				.with_end(end)
				.with_max_buffer_size(read_ahead)
				.build();
			let mut buf = vec![0; 8192];
			loop {
				match reader.read(&mut buf).await {
					Ok(0) => break, // EOF
					Ok(n) => yield Ok(buf[..n].to_vec()),
					Err(e) => {
						yield Err(e);
					}
				}
			}
		}
		// RFC 2046 §5.1: close-delimiter is CRLF + "--" + boundary + "--".
		yield Ok(format!("\r\n--{boundary}--\r\n").into_bytes());
	};

	builder.body(axum::body::Body::from_stream(stream))
}

async fn file_handler(
	Query(params): Query<FileQuery>,
	State(state): State<ProviderState>,
	range: Option<TypedHeader<Range>>,
) -> impl IntoResponse {
	let ranges = if let Some(TypedHeader(range)) = range {
		range
			.satisfiable_ranges(params.file.size())
			.filter_map(|r| {
				let (start, end) = get_real_bounds(&params.file, r);
				if start < end {
					Some((start, end))
				} else {
					None
				}
			})
			.collect::<Vec<_>>()
	} else {
		vec![(0, params.file.size())]
	};

	let read_ahead = effective_read_ahead(params.buffer, state.client.state().memory_budget());
	let response_builder = http::Response::builder().header(http::header::ACCEPT_RANGES, "bytes");

	match ranges {
		ranges if let [range] = *ranges => single_range_response_builder(
			params.file,
			range,
			read_ahead,
			state.client.clone(),
			response_builder,
		),
		ranges if ranges.is_empty() => response_builder
			.status(StatusCode::RANGE_NOT_SATISFIABLE)
			.header(
				http::header::CONTENT_RANGE,
				format!("bytes */{}", params.file.size()),
			)
			.body(axum::body::Body::empty()),

		ranges => multiple_range_response_builder(
			params.file,
			ranges,
			read_ahead,
			state.client.clone(),
			response_builder,
		),
	}
	.unwrap_or_else(|e| {
		http::Response::builder()
			.status(StatusCode::INTERNAL_SERVER_ERROR)
			.body(axum::body::Body::from(format!(
				"Error building response: {e}"
			)))
			.expect("should always be able to build a response")
	})
}

/// Unit tests for http_provider logic that do not require network credentials.
///
/// These tests use the `http-provider` feature flag and a local Tokio runtime.
/// They document and guard behavioral contracts of the provider lifecycle.
#[cfg(all(test, feature = "http-provider"))]
mod tests {
	use crate::{
		auth::{http::ClientConfig, unauth::UnauthClient},
		http_provider::client_impl::HttpProviderSharedClientExt,
	};

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
		let four_chunks = (one_chunk * 4) as usize;
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
		let two_chunks = (one_chunk * 2) as usize;
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
	async fn write_idle_timeout_fires_when_writes_stall() {
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
			fn poll_shutdown(
				self: Pin<&mut Self>,
				_: &mut Context<'_>,
			) -> Poll<std::io::Result<()>> {
				Poll::Ready(Ok(()))
			}
		}

		let mut writer =
			super::WriteIdleTimeout::new(StalledWriter, std::time::Duration::from_secs(30));
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
	async fn write_idle_timeout_does_not_fire_while_progressing() {
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
			fn poll_shutdown(
				self: Pin<&mut Self>,
				_: &mut Context<'_>,
			) -> Poll<std::io::Result<()>> {
				Poll::Ready(Ok(()))
			}
		}

		let mut writer =
			super::WriteIdleTimeout::new(ReadyWriter, std::time::Duration::from_millis(1));
		for _ in 0..1000 {
			let n = writer
				.write(b"chunk")
				.await
				.expect("progressing write must succeed");
			assert_eq!(n, 5);
		}
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
			write_idle_timeout: Duration::from_millis(500),
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
	async fn write_idle_timeout_reclaims_the_budget_held_by_an_abandoned_stream() {
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
			write_idle_timeout: Duration::from_millis(500),
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
}
