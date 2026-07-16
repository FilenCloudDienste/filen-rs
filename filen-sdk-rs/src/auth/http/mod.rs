use std::{
	borrow::Cow,
	fmt::Debug,
	num::NonZeroU32,
	sync::{Arc, RwLock},
	time::Duration,
};

use bytes::Bytes;
use filen_macros::js_type;
use filen_types::auth::APIKey;
use reqwest::{
	IntoUrl, RequestBuilder,
	header::{HeaderName, HeaderValue},
};
use serde::{Serialize, de::DeserializeOwned};
use tower::{ServiceBuilder, ServiceExt, limit::GlobalConcurrencyLimitLayer};

use crate::consts::{CHUNK_SIZE, FILE_CHUNK_SIZE_EXTRA_USIZE};
use crate::{
	Error,
	auth::{Client, http::auth::AuthLayer, unauth::UnauthClient},
	consts::gateway_url,
	util::{MaybeSend, MaybeSendSync},
};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use bandwidth_limit::{
	DownloadBandwidthLimiterLayer, UploadBandwidthLimiterLayer, new_download_bandwidth_limiter,
	new_upload_bandwidth_limiter, set_upload_bandwidth_limit,
};

mod auth;
// can't actually cap bandwidth in wasm, so this would just add overhead
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod bandwidth_limit;
mod deserialize;
mod download_body;
mod limit;
mod logging;
mod retry;
mod serialize;
mod tower_wasm_time;
mod url_parser;

use tower_wasm_time::tps_budget::TpsBudget;

impl Client {
	pub(crate) fn get_api_key(&self) -> String {
		self.http_client
			.api_key()
			.read()
			.unwrap()
			.0
			.clone()
			.into_owned()
	}
}

/// Default cap on the TCP/TLS connect phase. Connecting should be fast, so this mainly bounds a
/// host that accepts no connection (SYN black-hole) instead of letting it hang.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default read timeout. reqwest applies this as a deadline for the response to *start* (time to
/// first byte) and then as an idle (inter-chunk) timeout once the body is streaming. It must
/// therefore exceed the slowest legitimate time-to-first-byte: a directory listing over a very
/// large folder can take a couple of minutes to begin responding (though it streams quickly once
/// it starts), so this is generous on purpose. Without it a connected-but-silent host hangs a
/// request forever (a non-responding egest host has no HTTP status to classify). Tune via
/// [`ClientConfig::with_read_timeout`] (or `None` to disable) if your largest listings are slower.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(300);

pub struct ClientConfig {
	concurrency: usize,
	retry_budget: TpsBudget,
	file_io_memory_budget: usize,
	rate_limit_per_sec: NonZeroU32,
	upload_bandwidth_kilobytes_per_sec: Option<NonZeroU32>,
	download_bandwidth_kilobytes_per_sec: Option<NonZeroU32>,
	/// When `Some`, applied live to the global tracing filter via [`crate::obs::set_log_level`]
	/// when the client is built (see [`SharedClientState::new`]). `None` (the default) leaves the
	/// host- or cache-configured verbosity untouched, so a default-config client never reverts it.
	log_level: Option<LogLevel>,
	/// Timeout for the connect phase only. `None` disables it.
	connect_timeout: Option<Duration>,
	/// Idle/time-to-first-byte read timeout (see [`DEFAULT_READ_TIMEOUT`]). `None` disables it.
	read_timeout: Option<Duration>,
}

impl ClientConfig {
	pub fn with_concurrency(mut self, concurrency: usize) -> Self {
		self.concurrency = concurrency;
		self
	}

	pub fn with_retry(mut self, retry_budget: TpsBudget) -> Self {
		self.retry_budget = retry_budget;
		self
	}

	pub fn with_rate(mut self, rate_limit_per_sec: NonZeroU32) -> Self {
		self.rate_limit_per_sec = rate_limit_per_sec;
		self
	}

	pub fn with_upload(mut self, upload_bandwidth_kilobytes_per_sec: Option<NonZeroU32>) -> Self {
		self.upload_bandwidth_kilobytes_per_sec = upload_bandwidth_kilobytes_per_sec;
		self
	}

	pub fn with_download(
		mut self,
		download_bandwidth_kilobytes_per_sec: Option<NonZeroU32>,
	) -> Self {
		self.download_bandwidth_kilobytes_per_sec = download_bandwidth_kilobytes_per_sec;
		self
	}

	pub fn with_log_level(mut self, log_level: LogLevel) -> Self {
		self.log_level = Some(log_level);
		self
	}

	pub fn with_memory_budget(mut self, file_io_memory_budget: usize) -> Self {
		self.file_io_memory_budget = file_io_memory_budget;
		self
	}

	pub fn with_connect_timeout(mut self, connect_timeout: Option<Duration>) -> Self {
		self.connect_timeout = connect_timeout;
		self
	}

	pub fn with_read_timeout(mut self, read_timeout: Option<Duration>) -> Self {
		self.read_timeout = read_timeout;
		self
	}

	/// Build the [`reqwest::Client`] backing every request, applying the connect/read timeouts.
	///
	/// On wasm the timeouts are ignored — reqwest's fetch-based client exposes no
	/// connect/read-timeout knobs; the browser governs those instead.
	pub(crate) fn build_reqwest_client(&self) -> Result<reqwest::Client, Error> {
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			let mut builder = reqwest::Client::builder();
			if let Some(connect_timeout) = self.connect_timeout {
				builder = builder.connect_timeout(connect_timeout);
			}
			if let Some(read_timeout) = self.read_timeout {
				builder = builder.read_timeout(read_timeout);
			}
			builder.build().map_err(Error::from)
		}
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			let _ = (self.connect_timeout, self.read_timeout);
			Ok(reqwest::Client::new())
		}
	}
}

impl Default for ClientConfig {
	fn default() -> Self {
		Self {
			concurrency: 16,
			retry_budget: TpsBudget::default(),
			rate_limit_per_sec: NonZeroU32::new(64).unwrap(),
			upload_bandwidth_kilobytes_per_sec: None,
			download_bandwidth_kilobytes_per_sec: None,
			log_level: None,
			connect_timeout: Some(DEFAULT_CONNECT_TIMEOUT),
			read_timeout: Some(DEFAULT_READ_TIMEOUT),
			file_io_memory_budget: {
				#[cfg(not(target_os = "ios"))]
				{
					// 16 full Chunks
					(CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE) * 16
				}
				#[cfg(target_os = "ios")]
				{
					// 8 full Chunks (lower than other targets for the tighter iOS memory limits)
					(CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE) * 8
				}
			},
		}
	}
}

#[derive(Default)]
#[js_type(import, wasm_all)]
pub enum LogLevel {
	Off,
	Error,
	Warn,
	#[default]
	Info,
	Debug,
	Trace,
}

#[cfg(any(feature = "uniffi", all(target_family = "wasm", target_os = "unknown")))]
#[js_type(import, no_ser, wasm_all)]
pub struct JsClientConfig {
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(default))]
	pub concurrency: Option<u32>,
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(default))]
	pub rate_limit_per_sec: Option<u32>,
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(default))]
	pub upload_bandwidth_kilobytes_per_sec: Option<u32>,
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(default))]
	pub download_bandwidth_kilobytes_per_sec: Option<u32>,
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(default))]
	pub log_level: Option<LogLevel>,
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(default))]
	pub file_io_memory_budget: Option<u64>,
}

#[cfg(any(feature = "uniffi", all(target_family = "wasm", target_os = "unknown")))]
impl From<JsClientConfig> for ClientConfig {
	fn from(value: JsClientConfig) -> Self {
		let mut config = ClientConfig::default();
		if let Some(concurrency) = value.concurrency {
			// A zero-permit concurrency limiter makes every request await its permit forever; treat
			// 0 (a natural "unlimited" assumption from FFI callers) as a floor of 1.
			config = config.with_concurrency((concurrency as usize).max(1));
		}
		if let Some(rate_limit_per_sec) = value.rate_limit_per_sec
			&& let Some(nz) = NonZeroU32::new(rate_limit_per_sec)
		{
			config = config.with_rate(nz);
		}
		if let Some(upload_kbps) = value.upload_bandwidth_kilobytes_per_sec
			&& let Some(nz) = NonZeroU32::new(upload_kbps)
		{
			config = config.with_upload(Some(nz));
		}
		if let Some(download_kbps) = value.download_bandwidth_kilobytes_per_sec
			&& let Some(nz) = NonZeroU32::new(download_kbps)
		{
			config = config.with_download(Some(nz));
		}
		if let Some(log_level) = value.log_level {
			config = config.with_log_level(log_level);
		}
		if let Some(file_io_memory_budget) = value.file_io_memory_budget {
			// tokio's Semaphore panics above MAX_PERMITS (usize::MAX >> 3), so clamp the budget to
			// that ceiling instead of overflowing into a construction-time panic.
			let budget = usize::try_from(file_io_memory_budget)
				.unwrap_or(usize::MAX)
				.min(tokio::sync::Semaphore::MAX_PERMITS);
			config = config.with_memory_budget(budget);
		}
		config
	}
}

#[derive(Clone)]
pub(crate) struct SharedClientState {
	concurrency: GlobalConcurrencyLimitLayer,
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	max_concurrency: usize,
	retry: retry::RetryMapLayer,
	rate_limiter: limit::GlobalRateLimitLayer,
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	upload_limiter: limit::RateLimiter,
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	download_limiter: DownloadBandwidthLimiterLayer,
	memory_semaphore: Arc<tokio::sync::Semaphore>,
	/// The semaphore's initial permit count (bytes). Stored so the HTTP provider can cap a
	/// stream's read-ahead at half the total budget, which `available_permits()` cannot give.
	#[cfg(feature = "http-provider")]
	file_io_memory_budget: usize,
}

impl SharedClientState {
	pub(crate) fn new(config: ClientConfig) -> Result<Self, Error> {
		// A budget below one full encrypted chunk can never satisfy `Chunk::acquire`, which calls
		// `acquire_many(chunk_size)` on the memory semaphore: it would await more permits than the
		// semaphore will ever hold and hang every upload/download forever. Reject it in *every*
		// build, not only when `http-provider` (with its stricter half-budget rule below) is
		// compiled in.
		if config.file_io_memory_budget < CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE {
			return Err(Error::custom(
				crate::ErrorKind::InvalidState,
				format!(
					"file_io_memory_budget ({}) is too small: it must hold at least one chunk \
					 ({} bytes)",
					config.file_io_memory_budget,
					CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE
				),
			));
		}

		// The local HTTP provider caps a single stream's read-ahead window at half the shared
		// memory budget, so one stream can never pin the whole budget and starve a concurrent
		// download. That cap is meaningless if half the budget cannot hold even one chunk, so
		// reject such a configuration at construction rather than silently degrade.
		#[cfg(feature = "http-provider")]
		if config.file_io_memory_budget / 2 < CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE {
			return Err(Error::custom(
				crate::ErrorKind::InvalidState,
				format!(
					"file_io_memory_budget ({}) is too small: half of it must hold at least one \
					 chunk ({} bytes)",
					config.file_io_memory_budget,
					CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE
				),
			));
		}

		// Apply this client's level to the (host- or SDK-installed) global tracing filter only if
		// one was explicitly set. A default-config client leaves this `None` so routine client
		// construction never reverts the host's or cache's configured verbosity (or a runtime
		// `set_log_level`). No-op anyway if logging has not been initialised yet.
		if let Some(log_level) = config.log_level {
			crate::obs::set_log_level(log_level);
		}

		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		let upload_limiter = {
			if let Some(upload_kbps) = config.upload_bandwidth_kilobytes_per_sec {
				new_upload_bandwidth_limiter(upload_kbps)?
			} else {
				limit::RateLimiter::default()
			}
		};
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		let download_limiter = {
			if let Some(download_kbps) = config.download_bandwidth_kilobytes_per_sec {
				DownloadBandwidthLimiterLayer::new(new_download_bandwidth_limiter(download_kbps))
			} else {
				DownloadBandwidthLimiterLayer::new(limit::RateLimiter::default())
			}
		};

		Ok(Self {
			concurrency: GlobalConcurrencyLimitLayer::new(config.concurrency),
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			max_concurrency: config.concurrency,
			retry: retry::RetryMapLayer::new(retry::RetryPolicy::new(config.retry_budget)),
			rate_limiter: limit::GlobalRateLimitLayer::new(config.rate_limit_per_sec),
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			upload_limiter,
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			download_limiter,
			memory_semaphore: Arc::new(tokio::sync::Semaphore::new(config.file_io_memory_budget)),
			#[cfg(feature = "http-provider")]
			file_io_memory_budget: config.file_io_memory_budget,
		})
	}

	pub(crate) fn memory_semaphore(&self) -> &Arc<tokio::sync::Semaphore> {
		&self.memory_semaphore
	}

	/// Total file-IO memory budget in bytes (the semaphore's initial permit count). Used by the
	/// HTTP provider to cap a stream's read-ahead window at half the budget.
	#[cfg(feature = "http-provider")]
	pub(crate) fn memory_budget(&self) -> usize {
		self.file_io_memory_budget
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub(crate) fn max_concurrency(&self) -> usize {
		self.max_concurrency
	}
}

pub struct AuthClient {
	pub(crate) unauthed: UnauthClient,
	api_key: Arc<RwLock<APIKey<'static>>>,
}

impl std::fmt::Debug for AuthClient {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let api_key = self
			.api_key
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.to_string();
		let hash = blake3::hash(api_key.as_bytes());
		let hex_string = hash.to_hex();
		f.debug_struct("AuthClient")
			.field("api_key", &hex_string)
			.finish()
	}
}

impl PartialEq for AuthClient {
	fn eq(&self, other: &Self) -> bool {
		let self_key = self
			.api_key
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.clone();
		let other_key = other
			.api_key
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.clone();
		self_key == other_key
	}
}

impl Eq for AuthClient {}

async fn execute_request(
	request: RequestBuilder,
) -> Result<reqwest::Response, retry::RetryError<Error>> {
	let (client, request) = request.build_split();
	let request = request
		.map_err(Error::from)
		.map_err(retry::RetryError::NoRetry)?;
	client
		.execute(request)
		.await
		.and_then(|resp| resp.error_for_status())
		.map_err(|e| {
			let retryable = is_attempt_retryable(
				e.status(),
				e.is_builder(),
				e.is_request(),
				is_dispatch_gone(&e) || is_incomplete_message(&e),
			);
			retry::RetryError::from_retryable(retryable, Error::from(e))
		})
}

/// Decide whether a failed HTTP attempt should be retried.
///
/// `status` is `Some` only when the failure is an HTTP error *response* surfaced by
/// [`error_for_status`](reqwest::Response::error_for_status). In that case retry only transient
/// statuses — any 5xx, plus 408 (Request Timeout) and 429 (Too Many Requests). A permanent 4xx
/// such as 404 must NOT be retried: the egest/ingest file servers answer a genuinely-missing
/// object with a real `404 Not Found`, and retrying it spins against the retry budget's `reserve`
/// floor — which throttles but never *exhausts* for a single forever-failing request — so above a
/// modest round-trip time the retries never outpace the floor and the call hangs indefinitely.
/// (This is exactly what hung the nightly test job: `download_file_chunk_by_uuid` for a random UUID
/// 404s, was retried forever at the runners' egest RTT, and the binary never finished. Gateway API
/// errors instead arrive as HTTP 200 + `{status:false}` JSON, so they never reach this branch.)
///
/// When `status` is `None` the failure is a transport/connection error with no HTTP response.
/// `dispatch_gone` flags a *dead pooled connection* — hyper `DispatchGone` (its dispatch task was
/// dropped before the request was written) or `IncompleteMessage` (the server closed an idle
/// keep-alive the SDK then reused): the connection died at the request boundary, so retrying is safe
/// (the request never reached the server, or is replayable for this SDK's idempotent-by-construction
/// endpoints). It is retryable, as is anything that is neither a builder nor a request error. A
/// builder or request error may have been partially sent, so it stays non-retryable — EXCEPT when
/// `dispatch_gone` already marked it a dead-pool failure. A connect or read timeout also surfaces as
/// a `Kind::Request` error (`is_request`) but is not a dead-pool failure, so timeouts fall here and
/// are NOT retried — fail fast rather than spend another full timeout on a stalled host.
fn is_attempt_retryable(
	status: Option<reqwest::StatusCode>,
	is_builder: bool,
	is_request: bool,
	dispatch_gone: bool,
) -> bool {
	if dispatch_gone {
		return true;
	}
	match status {
		Some(status) => {
			status.is_server_error()
				|| status == reqwest::StatusCode::REQUEST_TIMEOUT
				|| status == reqwest::StatusCode::TOO_MANY_REQUESTS
		}
		None => !(is_builder || is_request),
	}
}

/// Walks `err` and its [`source`](std::error::Error::source) chain, returning true if any link's
/// `Display` contains one of `needles`. Used to detect a specific lower-layer error that the
/// public error API does not otherwise expose.
fn error_chain_mentions(err: &(dyn std::error::Error + 'static), needles: &[&str]) -> bool {
	let mut source = Some(err);
	while let Some(e) = source {
		let msg = e.to_string();
		if needles.iter().any(|needle| msg.contains(needle)) {
			return true;
		}
		source = e.source();
	}
	false
}

/// True for hyper's `DispatchGone` (`Kind::User(DispatchGone)`, Display "dispatch task is gone",
/// inner cause "runtime dropped the dispatch task"): the pooled connection's dispatch task was
/// dropped — its tokio runtime went away — BEFORE the request was written to the socket, so the
/// request never reached the server and retrying is safe even for non-idempotent endpoints. This
/// arises when a connection opened on one runtime (e.g. a short-lived cache-worker runtime) is
/// later reused from another — a known reqwest/hyper cross-runtime connection-pool hazard. hyper
/// exposes no predicate for it (`is_canceled()` covers only `Kind::Canceled`; `is_user()` would
/// also match the partially-sent body-write-abort case), so we match its stable `Display` string.
fn is_dispatch_gone(err: &reqwest::Error) -> bool {
	error_chain_mentions(
		err,
		&["dispatch task is gone", "runtime dropped the dispatch task"],
	)
}

/// True for hyper's `IncompleteMessage` (Display "connection closed before message completed"): the
/// server closed an idle pooled keep-alive connection that the SDK then reused for the next request
/// — the classic stale-pool race. Like [`is_dispatch_gone`] this surfaces as a request-kind error,
/// but UNLIKE `DispatchGone` (which fails provably *before* the request is written) `IncompleteMessage`
/// surfaces when the *response* read hits EOF, so the server may already have received and processed
/// the request — receipt is genuinely ambiguous (hyperium/hyper#2136). Retrying is nonetheless safe
/// because every endpoint reached through [`execute_request`] is idempotent-by-construction: the
/// serialize layer sits *outside* the retry layer, so a retry replays byte-identical bytes
/// (client-generated uuid + server-side name-hash dedup; content-addressed chunk uploads; idempotent
/// GETs). A future endpoint whose identical-byte replay is not a no-op must carry a client
/// idempotency key before relying on this. reqwest does not re-export hyper's
/// `Error::is_incomplete_message()`, so — like [`is_dispatch_gone`] — we match the stable `Display`.
fn is_incomplete_message(err: &reqwest::Error) -> bool {
	error_chain_mentions(err, &["connection closed before message completed"])
}

#[cfg(test)]
mod retry_classification_tests {
	use reqwest::StatusCode;

	use super::{error_chain_mentions, is_attempt_retryable};

	/// A permanent 4xx (the egest `404 Not Found` for a missing chunk) must NOT be retried — this
	/// is the regression that hung the nightly test job forever.
	#[test]
	fn permanent_4xx_status_is_not_retried() {
		for status in [
			StatusCode::NOT_FOUND,
			StatusCode::FORBIDDEN,
			StatusCode::BAD_REQUEST,
			StatusCode::UNAUTHORIZED,
			StatusCode::GONE,
		] {
			assert!(
				!is_attempt_retryable(Some(status), false, false, false),
				"{status} should not be retried"
			);
		}
	}

	/// Transient statuses stay retryable.
	#[test]
	fn transient_statuses_are_retried() {
		for status in [
			StatusCode::INTERNAL_SERVER_ERROR,
			StatusCode::BAD_GATEWAY,
			StatusCode::SERVICE_UNAVAILABLE,
			StatusCode::GATEWAY_TIMEOUT,
			StatusCode::REQUEST_TIMEOUT,
			StatusCode::TOO_MANY_REQUESTS,
		] {
			assert!(
				is_attempt_retryable(Some(status), false, false, false),
				"{status} should be retried"
			);
		}
	}

	/// Transport errors (no HTTP status) keep the prior behavior: a builder or request error may
	/// have been partially sent and is not retryable; any other transport error is.
	#[test]
	fn transport_errors_without_status_keep_prior_behavior() {
		assert!(!is_attempt_retryable(None, true, false, false)); // builder error
		assert!(!is_attempt_retryable(None, false, true, false)); // request error (maybe sent)
		assert!(is_attempt_retryable(None, false, false, false)); // connect/decode error
	}

	/// A dead pooled connection (`DispatchGone`) failed before the request was written, so it is
	/// retryable even though it surfaces as a request error.
	#[test]
	fn dispatch_gone_is_always_retryable() {
		assert!(is_attempt_retryable(None, false, true, true));
	}

	#[derive(Debug)]
	struct Layered {
		msg: &'static str,
		source: Option<Box<Layered>>,
	}

	impl std::fmt::Display for Layered {
		fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
			f.write_str(self.msg)
		}
	}

	impl std::error::Error for Layered {
		fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
			self.source
				.as_ref()
				.map(|s| s.as_ref() as &(dyn std::error::Error + 'static))
		}
	}

	/// Mirrors the reqwest → hyper-util → hyper layering: the `DispatchGone` marker is only on the
	/// innermost error, so the walk must descend the whole `source` chain.
	#[test]
	fn matches_needle_deep_in_the_source_chain() {
		let err = Layered {
			msg: "error sending request for url (https://example/v3/dir/create)",
			source: Some(Box::new(Layered {
				msg: "client error (SendRequest)",
				source: Some(Box::new(Layered {
					msg: "dispatch task is gone",
					source: None,
				})),
			})),
		};
		assert!(error_chain_mentions(
			&err,
			&["dispatch task is gone", "runtime dropped the dispatch task"]
		));
		assert!(!error_chain_mentions(&err, &["connection refused"]));
	}
}

impl AuthClient {
	pub(crate) fn api_key(&self) -> &Arc<RwLock<APIKey<'static>>> {
		&self.api_key
	}

	pub(crate) fn state(&self) -> &SharedClientState {
		&self.unauthed.state
	}

	pub(crate) async fn set_request_rate_limit(&self, rate_limit_per_second: NonZeroU32) {
		self.unauthed
			.state
			.rate_limiter
			.limiter
			.change_rate_per_sec(Some(rate_limit_per_second))
			.await;
	}

	pub(crate) fn from_unauthed(
		unauthed: UnauthClient,
		api_key: Arc<RwLock<APIKey<'static>>>,
	) -> Self {
		Self { unauthed, api_key }
	}

	pub(crate) fn to_unauthed(&self) -> UnauthClient {
		self.unauthed.clone()
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub(crate) async fn set_bandwidth_limits(
		&self,
		upload_kbps: Option<NonZeroU32>,
		download_kbps: Option<NonZeroU32>,
	) {
		futures::join!(
			set_upload_bandwidth_limit(&self.unauthed.state.upload_limiter, upload_kbps),
			self.unauthed
				.state
				.download_limiter
				.limiter
				.change_rate_per_sec(download_kbps)
		);
	}
}

impl UnauthClient {
	pub(crate) fn state(&self) -> &SharedClientState {
		&self.state
	}

	pub(crate) async fn post<Req, Res>(
		&self,
		endpoint: Cow<'static, str>,
		body: &Req,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug,
	{
		let url = gateway_url(&endpoint);

		let builder = ServiceBuilder::new()
			.layer(logging::LogLayer::new(endpoint)) // optional logging
			.layer(serialize::SerializeLayer::<Req>::new(body)) // required to serialize body
			.layer(url_parser::UrlParseLayer) // required to parse URL string to reqwest::Url
			.layer(self.state.concurrency.clone()) // optional
			.layer(self.state.retry.clone()) // required to map RetryError to Error
			.layer(self.state.rate_limiter.clone()) // optional
			.layer(deserialize::DeserializeLayer::<Res>::new()) // required to convert AsRef<u8> to T
			.layer(download_body::full::DownloadLayer::new()); // required to download full response body to bytes

		let builder = {
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			{
				builder
					.layer(UploadBandwidthLimiterLayer::new(&self.state.upload_limiter)) // required to map Request to RequestBuilder
					.layer(self.state.download_limiter.clone()) // optional
			}
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				builder.map_request(|r: Request<Bytes, reqwest::Url>| -> RequestBuilder {
					r.into_builder_map_body(|b| b)
				})
			}
		};

		builder
			.service_fn(execute_request)
			.oneshot(Request {
				method: RequestMethod::Post(()),
				response_type: ResponseType::Standard,
				url,
				client: self.reqwest_client.clone(),
			})
			.await
	}

	pub(crate) fn post_large_response<Req, Res, F>(
		&self,
		endpoint: Cow<'static, str>,
		body: &Req,
		callback: Option<&F>,
	) -> impl Future<Output = Result<Res, Error>> + MaybeSend
	where
		Res: DeserializeOwned + Debug + Send,
		Req: Serialize + Debug + Sync,
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		let url = gateway_url(&endpoint);

		let builder = ServiceBuilder::new()
			.layer(logging::LogLayer::new(endpoint)) // optional logging
			.layer(serialize::SerializeLayer::<Req>::new(body)) // required to serialize body
			.layer(url_parser::UrlParseLayer) // required to parse URL string to reqwest::Url
			.layer(self.state.concurrency.clone()) // optional
			.layer(self.state.retry.clone()) // required to map RetryError to Error
			.layer(self.state.rate_limiter.clone()) // optional
			.layer(deserialize::DeserializeLayer::<Res>::new()) // required to convert AsRef<u8> to T
			.layer(download_body::callback::DownloadWithCallbackLayer::new(
				callback,
			)); // required to download full response body to bytes

		let builder = {
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			{
				builder
					.layer(UploadBandwidthLimiterLayer::new(&self.state.upload_limiter)) // required to map Request to RequestBuilder
					.layer(self.state.download_limiter.clone()) // optional
			}
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				builder.map_request(|r: Request<Bytes, reqwest::Url>| -> RequestBuilder {
					r.into_builder_map_body(|b| b)
				})
			}
		};

		builder.service_fn(execute_request).oneshot(Request {
			method: RequestMethod::Post(()),
			response_type: ResponseType::Large,
			url,
			client: self.reqwest_client.clone(),
		})
	}

	/// Streams a GET response body to bytes, invoking `callback(bytes_so_far, content_length)` as
	/// data arrives (throttled to one call per [`CALLBACK_INTERVAL`](crate::consts::CALLBACK_INTERVAL)).
	/// Lets callers report download progress *while a chunk arrives* rather than only once it is
	/// fully buffered — important for heavily-parallel downloads, where every in-flight chunk shares
	/// bandwidth and none completes (so none would report) for the first several seconds.
	pub(crate) async fn get_raw_bytes_with_callback<F>(
		&self,
		url: &str,
		endpoint: Cow<'static, str>,
		max_body_len: Option<usize>,
		callback: Option<&F>,
	) -> Result<Vec<u8>, Error>
	where
		F: Fn(u64, Option<u64>) + MaybeSendSync,
	{
		let callback_layer = download_body::callback::DownloadWithCallbackLayer::new(callback);
		let callback_layer = match max_body_len {
			Some(max_body_len) => callback_layer.with_max_body_len(max_body_len),
			None => callback_layer,
		};
		let builder = ServiceBuilder::new()
			.layer(logging::LogLayer::new(endpoint)) // optional logging
			.layer(url_parser::UrlParseLayer) // required to parse URL string to reqwest::Url
			.layer(self.state.concurrency.clone()) // optional
			.layer(self.state.retry.clone()) // required to map RetryError to Error
			.layer(self.state.rate_limiter.clone()) // optional
			.map_request(|request: Request<(), reqwest::Url>| {
				request.into_builder_map_body(|()| bytes::Bytes::new())
			}) // required to map Request to RequestBuilder
			.layer(callback_layer); // stream the body to bytes (capped for chunk downloads)
		let builder = {
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			{
				builder.layer(self.state.download_limiter.clone()) // optional
			}
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				builder
			}
		};
		builder
			.service_fn(execute_request)
			.oneshot(Request {
				method: RequestMethod::Get,
				response_type: ResponseType::Standard,
				url,
				client: self.reqwest_client.clone(),
			})
			.await
	}
}

impl AuthClient {
	pub(crate) async fn get_auth<Res>(&self, endpoint: Cow<'static, str>) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
	{
		let url = gateway_url(&endpoint);

		let builder = ServiceBuilder::new()
			.layer(logging::LogLayer::new(endpoint)) // optional logging
			.layer(url_parser::UrlParseLayer) // required to parse URL string to reqwest::Url
			.layer(self.unauthed.state.concurrency.clone()) // optional
			.layer(self.unauthed.state.retry.clone()) // required to map RetryError to Error
			.layer(self.unauthed.state.rate_limiter.clone()) // optional
			.layer(deserialize::DeserializeLayer::<Res>::new()) // required to convert AsRef<u8> to T
			.layer(download_body::full::DownloadLayer::new()) // required to download full response body to bytes
			.map_request(|request: Request<(), reqwest::Url>| {
				request.into_builder_map_body(|()| "")
			}); // required to map Request to RequestBuilder

		let builder = {
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			{
				builder.layer(self.unauthed.state.download_limiter.clone()) // optional
			}
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				builder
			}
		};

		builder
			.layer(auth::AuthLayer::new(&self.api_key))
			.service_fn(execute_request)
			.oneshot(Request {
				method: RequestMethod::Get,
				response_type: ResponseType::Standard,
				url,
				client: self.unauthed.reqwest_client.clone(),
			})
			.await
	}

	pub(crate) async fn post_auth<Req, Res>(
		&self,
		endpoint: Cow<'static, str>,
		body: &Req,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug,
	{
		let url = gateway_url(&endpoint);

		// This could be improved, all the boxes should be removable with type_alias_impl_trait
		// and using references instead of Arcs
		let builder = ServiceBuilder::new()
			.layer(logging::LogLayer::new(endpoint)) // optional logging
			.layer(serialize::SerializeLayer::<Req>::new(body)) // required to serialize body
			.layer(url_parser::UrlParseLayer) // required to parse URL string to reqwest::Url
			.layer(self.unauthed.state.concurrency.clone()) // optional
			.layer(self.unauthed.state.retry.clone()) // required to map RetryError to Error
			.layer(self.unauthed.state.rate_limiter.clone()) // optional
			.layer(deserialize::DeserializeLayer::<Res>::new()) // required to convert AsRef<u8> to T
			.layer(download_body::full::DownloadLayer::new()); // required to download full response body to bytes

		let builder = {
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			{
				builder
					.layer(UploadBandwidthLimiterLayer::new(
						&self.unauthed.state.upload_limiter,
					)) // required to map Request to RequestBuilder
					.layer(self.unauthed.state.download_limiter.clone()) // optional
			}
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				builder.map_request(|r: Request<Bytes, reqwest::Url>| -> RequestBuilder {
					r.into_builder_map_body(|b| b)
				})
			}
		};

		builder
			.layer(AuthLayer::new(&self.api_key))
			.service_fn(execute_request)
			.oneshot(Request {
				method: RequestMethod::Post(()),
				response_type: ResponseType::Standard,
				url,
				client: self.unauthed.reqwest_client.clone(),
			})
			.await // optional
	}

	pub(crate) async fn post_large_response_auth<Req, Res, F>(
		&self,
		endpoint: Cow<'static, str>,
		body: &Req,
		callback: Option<&F>,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug,
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		let url = gateway_url(&endpoint);

		// This could be improved, all the boxes should be removable with type_alias_impl_trait
		// and using references instead of Arcs
		let builder = ServiceBuilder::new()
			.layer(logging::LogLayer::new(endpoint)) // optional logging
			.layer(serialize::SerializeLayer::<Req>::new(body)) // required to serialize body
			.layer(url_parser::UrlParseLayer) // required to parse URL string to reqwest::Url
			.layer(self.unauthed.state.concurrency.clone()) // optional
			.layer(self.unauthed.state.retry.clone()) // required to map RetryError to Error
			.layer(self.unauthed.state.rate_limiter.clone()) // optional
			.layer(deserialize::DeserializeLayer::<Res>::new())
			.layer(download_body::callback::DownloadWithCallbackLayer::new(
				callback,
			)); // required to download full response body to bytes
		let builder = {
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			{
				builder
					.layer(UploadBandwidthLimiterLayer::new(
						&self.unauthed.state.upload_limiter,
					)) // required to map Request to RequestBuilder
					.layer(self.unauthed.state.download_limiter.clone()) // optional
			}
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				builder.map_request(|r: Request<Bytes, reqwest::Url>| -> RequestBuilder {
					r.into_builder_map_body(|b| b)
				})
			}
		};
		builder
			.layer(auth::AuthLayer::new(&self.api_key))
			.service_fn(execute_request)
			.oneshot(Request {
				method: RequestMethod::Post(()),
				response_type: ResponseType::Large,
				url,
				client: self.unauthed.reqwest_client.clone(),
			})
			.await
	}

	pub(crate) async fn post_raw_bytes_auth<Res>(
		&self,
		request: Bytes,
		url: &str,
		endpoint: Cow<'static, str>,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
	{
		let builder = ServiceBuilder::new()
			.layer(logging::LogLayer::new(endpoint)) // optional logging
			.layer(url_parser::UrlParseLayer) // required to parse URL string to reqwest::Url
			.layer(self.unauthed.state.concurrency.clone()) // optional
			.layer(self.unauthed.state.retry.clone()) // required to map RetryError to Error
			.layer(self.unauthed.state.rate_limiter.clone()) // optional
			.layer(deserialize::DeserializeLayer::<Res>::new())
			.layer(download_body::full::DownloadLayer::new()); // required to download full response body to bytes

		let builder = {
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			{
				builder
					.layer(UploadBandwidthLimiterLayer::new(
						&self.unauthed.state.upload_limiter,
					)) // required to map Request to RequestBuilder
					.layer(self.unauthed.state.download_limiter.clone()) // optional
			}
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				builder.map_request(|r: Request<Bytes, reqwest::Url>| -> RequestBuilder {
					r.into_builder_map_body(|b| b)
				})
			}
		};

		builder
			.layer(auth::AuthLayer::new(&self.api_key))
			.service_fn(execute_request)
			.oneshot(Request {
				method: RequestMethod::Post(request),
				response_type: ResponseType::Standard,
				url,
				client: self.unauthed.reqwest_client.clone(),
			})
			.await
	}
}

#[derive(Clone, Debug)]
enum RequestMethod<Body> {
	Get,
	Post(Body),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ResponseType {
	#[default]
	Standard,
	Large,
}

#[derive(Clone, Debug)]
struct Request<Body, Url> {
	method: RequestMethod<Body>,
	response_type: ResponseType,
	url: Url,
	client: reqwest::Client,
}

impl<Body> Request<Body, reqwest::Url> {
	fn into_builder_map_body<B>(self, map_body: impl FnOnce(Body) -> B) -> RequestBuilder
	where
		B: Into<reqwest::Body>,
	{
		let request = match self.method {
			RequestMethod::Get => self.client.get(self.url),
			RequestMethod::Post(body) => post_request(self.client, self.url, map_body(body)),
		};
		if self.response_type == ResponseType::Large {
			request.header(
				HeaderName::from_static("msgpack"),
				HeaderValue::from_static("1"),
			)
		} else {
			request
		}
	}
}

fn post_request(
	client: reqwest::Client,
	url: impl IntoUrl,
	body: impl Into<reqwest::Body>,
) -> reqwest::RequestBuilder {
	client.post(url).body(body).header(
		reqwest::header::CONTENT_TYPE,
		HeaderValue::from_static("application/json"),
	)
}

impl From<Request<Bytes, reqwest::Url>> for RequestBuilder {
	fn from(req: Request<Bytes, reqwest::Url>) -> Self {
		req.into_builder_map_body(|body| body)
	}
}

// Native-only: the timeouts are applied via reqwest's native builder, and these tests drive a real
// local TCP server.
#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod client_timeout_tests {
	use std::time::{Duration, Instant};

	use tokio::{
		io::{AsyncReadExt, AsyncWriteExt},
		net::TcpListener,
	};

	use super::{ClientConfig, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT};

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
	/// and must be RETRYABLE: the request is idempotent-by-construction for this SDK's endpoints
	/// (client-generated uuid + server name-hash dedup), so a transient connection close must not
	/// fail the call on the first attempt. Modelled by a server that reads the request then closes
	/// without responding, which makes the client's response read hit the same IncompleteMessage.
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
		let err = super::execute_request(client.get(format!("http://{addr}/")))
			.await
			.expect_err("a connection closed before responding must error");

		match err {
			super::retry::RetryError::Retry(_) => {}
			super::retry::RetryError::NoRetry(e) => panic!(
				"IncompleteMessage (stale-pool connection close) must be classified retryable, \
				 but was NoRetry: {e}"
			),
		}
	}
}

#[cfg(test)]
mod min_memory_budget_tests {
	use super::{CHUNK_SIZE, ClientConfig, FILE_CHUNK_SIZE_EXTRA_USIZE, SharedClientState};

	/// A budget below one full encrypted chunk can never satisfy `Chunk::acquire`'s `acquire_many`
	/// (it would await more permits than the semaphore will ever hold), hanging every transfer.
	/// `SharedClientState::new` must reject it in every build — not only when the `http-provider`
	/// feature (with its stricter half-budget rule) is compiled in.
	#[tokio::test]
	async fn rejects_budget_below_one_chunk() {
		let one_chunk = CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE;

		// Half a chunk: acquire_many(chunk_size) would await more permits than exist -> hang.
		assert!(
			SharedClientState::new(ClientConfig::default().with_memory_budget(one_chunk / 2))
				.is_err(),
			"a sub-chunk budget must be rejected at construction"
		);
		// One byte below a chunk is still rejected.
		assert!(
			SharedClientState::new(ClientConfig::default().with_memory_budget(one_chunk - 1))
				.is_err(),
			"a budget one byte below a chunk must be rejected"
		);
		// Exactly one chunk is the minimum that can satisfy a single acquire. With `http-provider`
		// compiled in the stricter half-budget rule rejects it, so only assert acceptance here
		// when that feature is absent.
		#[cfg(not(feature = "http-provider"))]
		SharedClientState::new(ClientConfig::default().with_memory_budget(one_chunk))
			.expect("a one-chunk budget must be valid without http-provider");
	}
}

#[cfg(all(test, feature = "http-provider"))]
mod memory_budget_validation_tests {
	use super::{CHUNK_SIZE, ClientConfig, FILE_CHUNK_SIZE_EXTRA_USIZE, SharedClientState};

	/// `SharedClientState::new` rejects a memory budget whose half cannot hold one chunk, because
	/// the HTTP provider caps a stream's read-ahead window at half the budget.
	#[tokio::test]
	async fn rejects_budget_whose_half_cannot_hold_one_chunk() {
		let one_chunk = CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE;

		// budget == 1 chunk -> half is half a chunk -> rejected.
		assert!(
			SharedClientState::new(ClientConfig::default().with_memory_budget(one_chunk)).is_err(),
			"a 1-chunk budget (half < one chunk) must be rejected at construction"
		);

		// budget == 2 chunks -> half is exactly one chunk -> accepted (the iOS default boundary).
		SharedClientState::new(ClientConfig::default().with_memory_budget(2 * one_chunk))
			.expect("a 2-chunk budget (half == one chunk) must be valid");

		// the platform defaults must remain valid.
		SharedClientState::new(ClientConfig::default()).expect("the default budget must be valid");
	}
}

#[cfg(all(test, feature = "uniffi"))]
mod js_client_config_tests {
	use super::{ClientConfig, JsClientConfig};

	fn base() -> JsClientConfig {
		JsClientConfig {
			concurrency: None,
			rate_limit_per_sec: None,
			upload_bandwidth_kilobytes_per_sec: None,
			download_bandwidth_kilobytes_per_sec: None,
			log_level: None,
			file_io_memory_budget: None,
		}
	}

	/// concurrency 0 (a natural "unlimited" assumption from FFI callers) must not become a
	/// zero-permit limiter that makes every request await its permit forever.
	#[test]
	fn zero_concurrency_is_clamped_to_one() {
		let config = ClientConfig::from(JsClientConfig {
			concurrency: Some(0),
			..base()
		});
		assert_eq!(
			config.concurrency, 1,
			"concurrency 0 must be clamped to 1, not left as a zero-permit (hang-forever) limiter"
		);
	}

	/// An oversized memory budget must be clamped instead of panicking tokio's Semaphore::new,
	/// which rejects any permit count above MAX_PERMITS (usize::MAX >> 3).
	#[test]
	fn huge_memory_budget_is_clamped_to_semaphore_max() {
		let config = ClientConfig::from(JsClientConfig {
			file_io_memory_budget: Some(u64::MAX),
			..base()
		});
		assert_eq!(
			config.file_io_memory_budget,
			tokio::sync::Semaphore::MAX_PERMITS,
			"an oversized budget must be clamped to Semaphore::MAX_PERMITS to avoid a panic"
		);
	}
}
