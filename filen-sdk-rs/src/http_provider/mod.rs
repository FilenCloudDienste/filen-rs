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
	HeaderMap, StatusCode,
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

fn get_real_bounds(size: u64, (start, end): (Bound<u64>, Bound<u64>)) -> (u64, u64) {
	let start = match start {
		Bound::Included(start) => start,
		Bound::Excluded(start) => start + 1,
		Bound::Unbounded => 0,
	};
	let end = match end {
		// Clamp before incrementing: `bytes=0-18446744073709551615` yields `Included(u64::MAX)`,
		// and `end + 1` would overflow (panic under overflow-checks, wrap to 0 otherwise). Per RFC
		// an end at/past the size means the remainder of the representation, so saturating here
		// clamps to `size` below rather than mis-serving a 416.
		Bound::Included(end) => end.saturating_add(1),
		Bound::Excluded(end) => end,
		Bound::Unbounded => u64::MAX,
	}
	.min(size);
	(start, end)
}

/// Resolves a `Range` header against the file `size` into concrete `[start, end)` byte ranges.
///
/// [`Range::satisfiable_ranges`] drops a suffix range (`bytes=-N`) whose length exceeds the file
/// instead of clamping it. RFC 9110 §14.1.2 requires such a suffix to select the entire
/// representation, so when it would otherwise leave us with no satisfiable range we recover the
/// whole-file range from the raw header rather than returning `416 Range Not Satisfiable`.
fn resolve_ranges(range: &Range, raw_range: Option<&str>, size: u64) -> Vec<(u64, u64)> {
	let mut ranges = range
		.satisfiable_ranges(size)
		.filter_map(|r| {
			let (start, end) = get_real_bounds(size, r);
			if start < end {
				Some((start, end))
			} else {
				None
			}
		})
		.collect::<Vec<_>>();
	if ranges.is_empty() && raw_range.is_some_and(|raw| has_oversized_suffix(raw, size)) {
		ranges.push((0, size));
	}
	ranges
}

/// Whether the raw `Range` header requests a suffix (`bytes=-N`) longer than `size`, which per RFC
/// 9110 §14.1.2 means the entire representation. `satisfiable_ranges` silently drops these, so we
/// detect them here to serve the whole file instead of `416`.
fn has_oversized_suffix(raw_range: &str, size: u64) -> bool {
	raw_range
		.strip_prefix("bytes=")
		.into_iter()
		.flat_map(|set| set.split(','))
		.any(|spec| {
			spec.trim()
				.strip_prefix('-')
				.and_then(|n| n.parse::<u64>().ok())
				.is_some_and(|n| n > size)
		})
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
	headers: HeaderMap,
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
		let raw_range = headers
			.get(http::header::RANGE)
			.and_then(|value| value.to_str().ok());
		resolve_ranges(&range, raw_range, file.size())
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
mod tests;
