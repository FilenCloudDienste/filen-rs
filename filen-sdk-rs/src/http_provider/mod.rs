use std::{
	collections::{HashMap, VecDeque},
	future::Future,
	io,
	ops::Bound,
	pin::Pin,
	sync::{Arc, Mutex},
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

use filen_types::traits::CowHelpers;

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
/// A connection that makes no read *or* write progress for this long is dropped, which tears down
/// the streaming reader behind it and frees its budget. Covers both a peer that stopped reading
/// (writes back up against a full socket buffer) and a peer that connected but never sent a
/// complete request (no inbound bytes). Mirrors nginx's `send_timeout` + `client_header_timeout`.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

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

fn idle_timeout_error() -> io::Error {
	io::Error::new(
		io::ErrorKind::TimedOut,
		"http provider: connection idle timeout (no read or write progress)",
	)
}

/// Wraps a connection's IO with an **idle** timeout covering both directions.
///
/// It shares one timer across reads and writes: it arms when an IO poll parks `Pending` and fails
/// the IO with [`io::ErrorKind::TimedOut`] once `timeout` elapses with no progress, tearing the
/// connection down so the streaming reader (and the memory budget) it owns is dropped. Any
/// successful read or write resets the timer.
///
/// The timer engages on real IO back-pressure, which reaps the cases that matter:
/// - a peer that stops reading — the send buffer fills, `poll_write` parks `Pending`, and the
///   connection (with its pinned reader/budget) is dropped. This is the budget-leak path the timer
///   was built for; see `timeout_listener_closes_abandoned_stream`.
/// - a peer that connects and never sends a request — the protocol-sniff `poll_read` parks
///   `Pending`; see `timeout_listener_closes_a_silent_client`.
///
/// It does NOT reap a connection that is merely *slow to produce* a response body while the client
/// keeps reading: that does not back the IO up (the send buffer drains), so the timer never engages
/// and a legitimately slow download completes (`timeout_listener_keeps_a_slow_download_alive`). A
/// peer that trickles a *partial* request then stalls mid-headers may likewise escape this timer
/// (hyper has no header-read timer installed here); acceptable, since it pins no reader/budget — the
/// handler has not run — and the provider is localhost-only.
struct IdleTimeout<S> {
	inner: S,
	timeout: Duration,
	idle: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S> IdleTimeout<S> {
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

impl<S: AsyncWrite + Unpin> AsyncWrite for IdleTimeout<S> {
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
				Poll::Ready(()) => Poll::Ready(Err(idle_timeout_error())),
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
				Poll::Ready(()) => Poll::Ready(Err(idle_timeout_error())),
				Poll::Pending => Poll::Pending,
			},
		}
	}

	fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
	}
}

impl<S: AsyncRead + Unpin> AsyncRead for IdleTimeout<S> {
	fn poll_read(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &mut ReadBuf<'_>,
	) -> Poll<io::Result<()>> {
		let this = self.get_mut();
		match Pin::new(&mut this.inner).poll_read(cx, buf) {
			// Any read result (data, EOF, or error) is progress on the read side — reset the
			// shared timer. Crucially, while a response streams out the read side sits `Pending`
			// here, but the concurrent writes keep the timer reset, so a long download is never
			// reaped; only a connection idle in *both* directions trips the timeout.
			Poll::Ready(res) => {
				this.idle = None;
				Poll::Ready(res)
			}
			Poll::Pending => match this.poll_idle_elapsed(cx) {
				Poll::Ready(()) => Poll::Ready(Err(idle_timeout_error())),
				Poll::Pending => Poll::Pending,
			},
		}
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

/// An [`axum::serve::Listener`] that wraps every accepted connection in an [`IdleTimeout`],
/// so abandoned streaming connections are dropped instead of pinning a reader forever.
struct TimeoutListener {
	inner: tokio::net::TcpListener,
	idle_timeout: Duration,
}

impl axum::serve::Listener for TimeoutListener {
	type Io = IdleTimeout<tokio::net::TcpStream>;
	type Addr = std::net::SocketAddr;

	async fn accept(&mut self) -> (Self::Io, Self::Addr) {
		loop {
			match self.inner.accept().await {
				Ok((stream, addr)) => {
					return (IdleTimeout::new(stream, self.idle_timeout), addr);
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

/// Upper bound on live URL tokens. With deduplication this is only reachable by
/// browsing that many *distinct* files in one provider lifetime; at a few hundred
/// bytes per entry the store tops out well under 1 MiB. Eviction is LRU and
/// [`TokenStore::resolve`] counts as use, so an actively streaming URL stays warm.
const MAX_URL_TOKENS: usize = 1024;

/// The raw random token minted per served file; rides in the URL base64url-encoded.
type UrlToken = [u8; 32];

/// Maps opaque URL tokens to the files they serve. The file metadata (including
/// its decryption key) lives here in-process instead of in the URL, so no key
/// material reaches media-framework/OS request logs.
///
/// The store is bounded: minting a URL for a file that already has a live token
/// reuses it (the app re-derives URLs on every preview mount, so this is the hot
/// path), and once [`MAX_URL_TOKENS`] distinct files are live the least recently
/// used entry is evicted. Long term this should move to a file-backed store in an
/// app-provided private directory — that removes the eviction bound entirely and
/// lets tokens survive a provider restart — but it needs a writable-directory
/// handoff from the host apps first, and today they treat every URL as scoped to
/// one foreground session anyway.
struct TokenStore {
	entries: HashMap<UrlToken, RemoteFileType<'static>>,
	/// LRU order over `entries`' keys; front is the eviction candidate.
	order: VecDeque<UrlToken>,
}

impl TokenStore {
	fn new() -> Self {
		Self {
			entries: HashMap::new(),
			order: VecDeque::new(),
		}
	}

	/// Returns the live token for `file`, minting (and if needed evicting) one if
	/// no equal file is currently served. Takes `file` by value so an already-owned
	/// file is stored as-is instead of cloned.
	fn token_for(&mut self, file: RemoteFileType<'_>) -> UrlToken {
		let existing = self
			.entries
			.iter()
			.find_map(|(token, stored)| (stored == &file).then_some(*token));
		if let Some(token) = existing {
			self.mark_used(token);
			return token;
		}

		if self.entries.len() >= MAX_URL_TOKENS
			&& let Some(oldest) = self.order.pop_front()
		{
			self.entries.remove(&oldest);
		}

		let token: UrlToken = rand::random();
		self.entries.insert(token, file.into_owned_cow());
		self.order.push_back(token);
		token
	}

	/// Resolves a URL's `?token=` value to the file it serves, refreshing its LRU
	/// slot. Malformed, wrong-length, and unknown tokens are all `None` (a 404).
	fn resolve(&mut self, token_param: &str) -> Option<RemoteFileType<'static>> {
		let token: UrlToken = BASE64_URL_SAFE_NO_PAD
			.decode(token_param)
			.ok()?
			.try_into()
			.ok()?;
		let file = self.entries.get(&token)?.clone();
		self.mark_used(token);
		Some(file)
	}

	fn mark_used(&mut self, token: UrlToken) {
		if let Some(pos) = self.order.iter().position(|t| *t == token) {
			self.order.remove(pos);
			self.order.push_back(token);
		}
	}
}

type SharedTokenStore = Arc<Mutex<TokenStore>>;

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct HttpProviderHandle {
	task: Option<tokio::task::JoinHandle<()>>,
	cancel_sender: Option<tokio::sync::oneshot::Sender<()>>,
	port: u16,
	tokens: SharedTokenStore,
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
		let tokens: SharedTokenStore = Arc::new(Mutex::new(TokenStore::new()));
		let router = axum::Router::new()
			.route("/file", get(file_handler))
			.with_state(ProviderState {
				client,
				tokens: tokens.clone(),
			});

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
					idle_timeout: IDLE_TIMEOUT,
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
			tokens,
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
	/// The URL carries only an opaque random token; the file metadata (including
	/// its decryption key) is stored in this provider's in-process store and looked
	/// up by the handler, so no key material rides in the URL where a media
	/// framework or the OS could log it.
	pub fn get_file_url(&self, file: RemoteFileType<'_>) -> String {
		let token = self
			.tokens
			.lock()
			.expect("http-provider token store lock poisoned")
			.token_for(file);
		format!(
			"http://127.0.0.1:{}/file?token={}",
			self.port,
			BASE64_URL_SAFE_NO_PAD.encode(token)
		)
	}

	/// Like [`get_file_url`](Self::get_file_url) but pins this stream's read-ahead window
	/// (in bytes) via the `buffer` query parameter. The server clamps it to a sane range
	/// (`[1 chunk, 64 MiB]`). Use a small value for latency-sensitive previews/thumbnails
	/// and a larger one for sustained playback; the default when omitted is 8 MiB.
	pub fn get_file_url_with_buffer_size(
		&self,
		file: RemoteFileType<'_>,
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
		Ok(self.get_file_url(file.try_into()?))
	}

	#[uniffi::method(name = "getFileUrlWithBufferSize")]
	pub fn get_file_url_with_buffer_size_uniffi(
		&self,
		file: crate::js::AnyFile,
		buffer_bytes: u64,
	) -> Result<String, Error> {
		Ok(self.get_file_url_with_buffer_size(file.try_into()?, buffer_bytes))
	}
}

#[derive(Clone)]
struct ProviderState {
	client: Arc<UnauthClient>,
	tokens: SharedTokenStore,
}

#[derive(Deserialize)]
struct FileQuery {
	/// Opaque token minted by [`get_file_url`](HttpProviderHandle::get_file_url);
	/// resolves to the file (and its key) in the provider's in-process store.
	token: String,
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
	// Resolve the opaque token to the file (and its key) held in-process. The entry
	// stays in the store (the token is reusable, not single-use) and resolving
	// refreshes its LRU slot, so the media player's later range requests over the
	// same playback session keep working.
	let Some(file) = state
		.tokens
		.lock()
		.expect("http-provider token store lock poisoned")
		.resolve(&params.token)
	else {
		return http::Response::builder()
			.status(StatusCode::NOT_FOUND)
			.body(axum::body::Body::empty())
			.expect("should always be able to build a response");
	};

	let ranges = if let Some(TypedHeader(range)) = range {
		range
			.satisfiable_ranges(file.size())
			.filter_map(|r| {
				let (start, end) = get_real_bounds(&file, r);
				if start < end {
					Some((start, end))
				} else {
					None
				}
			})
			.collect::<Vec<_>>()
	} else {
		vec![(0, file.size())]
	};

	let read_ahead = effective_read_ahead(params.buffer, state.client.state().memory_budget());
	let response_builder = http::Response::builder().header(http::header::ACCEPT_RANGES, "bytes");

	match ranges {
		ranges if let [range] = *ranges => single_range_response_builder(
			file,
			range,
			read_ahead,
			state.client.clone(),
			response_builder,
		),
		ranges if ranges.is_empty() => response_builder
			.status(StatusCode::RANGE_NOT_SATISFIABLE)
			.header(
				http::header::CONTENT_RANGE,
				format!("bytes */{}", file.size()),
			)
			.body(axum::body::Body::empty()),

		ranges => multiple_range_response_builder(
			file,
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

		RemoteFileType::File(Cow::Owned(RemoteFile {
			uuid: Uuid::new_v4(),
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
			fn poll_shutdown(
				self: Pin<&mut Self>,
				_: &mut Context<'_>,
			) -> Poll<std::io::Result<()>> {
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
			fn poll_shutdown(
				self: Pin<&mut Self>,
				_: &mut Context<'_>,
			) -> Poll<std::io::Result<()>> {
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
}
