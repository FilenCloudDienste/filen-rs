use std::{
	borrow::Cow,
	fmt::Debug,
	num::NonZeroU32,
	sync::{Arc, RwLock},
};

use bytes::Bytes;
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
	util::MaybeSend,
};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use bandwidth_limit::{
	DownloadBandwidthLimiterLayer, UploadBandwidthLimiterLayer, new_download_bandwidth_limiter,
	new_upload_bandwidth_limiter,
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

pub struct ClientConfig {
	concurrency: usize,
	retry_budget: TpsBudget,
	file_io_memory_budget: usize,
	rate_limit_per_sec: NonZeroU32,
	upload_bandwidth_kilobytes_per_sec: Option<NonZeroU32>,
	download_bandwidth_kilobytes_per_sec: Option<NonZeroU32>,
	log_level: log::LevelFilter,
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

	pub fn with_log_level(mut self, log_level: log::LevelFilter) -> Self {
		self.log_level = log_level;
		self
	}

	pub fn with_memory_budget(mut self, file_io_memory_budget: usize) -> Self {
		self.file_io_memory_budget = file_io_memory_budget;
		self
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
			log_level: log::LevelFilter::Debug,
			file_io_memory_budget: {
				#[cfg(not(target_os = "ios"))]
				{
					// 4 full Chunks
					(CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE) * 4
				}
				#[cfg(target_os = "ios")]
				{
					// 2 full Chunks
					(CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE) * 2
				}
			},
		}
	}
}

#[cfg(any(feature = "uniffi", all(target_family = "wasm", target_os = "unknown")))]
#[derive(Clone, Copy, Debug, Default)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify, serde::Deserialize, serde::Serialize),
	tsify(from_wasm_abi, into_wasm_abi)
)]
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
impl From<LogLevel> for log::LevelFilter {
	fn from(value: LogLevel) -> Self {
		match value {
			LogLevel::Off => log::LevelFilter::Off,
			LogLevel::Error => log::LevelFilter::Error,
			LogLevel::Warn => log::LevelFilter::Warn,
			LogLevel::Info => log::LevelFilter::Info,
			LogLevel::Debug => log::LevelFilter::Debug,
			LogLevel::Trace => log::LevelFilter::Trace,
		}
	}
}

#[cfg(any(feature = "uniffi", all(target_family = "wasm", target_os = "unknown")))]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify, serde::Deserialize),
	tsify(from_wasm_abi)
)]
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
			config = config.with_concurrency(concurrency as usize);
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
			let level = match log_level {
				LogLevel::Off => log::LevelFilter::Off,
				LogLevel::Error => log::LevelFilter::Error,
				LogLevel::Warn => log::LevelFilter::Warn,
				LogLevel::Info => log::LevelFilter::Info,
				LogLevel::Debug => log::LevelFilter::Debug,
				LogLevel::Trace => log::LevelFilter::Trace,
			};
			config = config.with_log_level(level);
		}
		if let Some(file_io_memory_budget) = value.file_io_memory_budget {
			config =
				config.with_memory_budget(file_io_memory_budget.try_into().unwrap_or(usize::MAX));
		}
		config
	}
}

#[derive(Clone)]
pub(crate) struct SharedClientState {
	concurrency: GlobalConcurrencyLimitLayer,
	retry: retry::RetryMapLayer,
	rate_limiter: limit::GlobalRateLimitLayer,
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	upload_limiter: limit::RateLimiter,
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	download_limiter: DownloadBandwidthLimiterLayer,
	log_level: log::LevelFilter,
	zip_lock: Arc<tokio::sync::Mutex<()>>,
	memory_semaphore: Arc<tokio::sync::Semaphore>,
}

impl SharedClientState {
	pub(crate) fn new(config: ClientConfig) -> Result<Self, Error> {
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
			retry: retry::RetryMapLayer::new(retry::RetryPolicy::new(config.retry_budget)),
			rate_limiter: limit::GlobalRateLimitLayer::new(config.rate_limit_per_sec),
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			upload_limiter,
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			download_limiter,
			log_level: config.log_level,
			zip_lock: Arc::new(tokio::sync::Mutex::new(())),
			memory_semaphore: Arc::new(tokio::sync::Semaphore::new(config.file_io_memory_budget)),
		})
	}

	pub(crate) async fn zip_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
		self.zip_lock.lock().await
	}

	pub(crate) fn memory_semaphore(&self) -> &Arc<tokio::sync::Semaphore> {
		&self.memory_semaphore
	}
}

trait RequestCallTrait {
	fn call(&self) -> Result<bytes::Bytes, Error>;
}

impl<Req> RequestCallTrait for Req
where
	Req: serde::Serialize,
{
	default fn call(&self) -> Result<bytes::Bytes, Error> {
		let body = serde_json::to_vec(self)?;
		Ok(bytes::Bytes::from_owner(body))
	}
}

impl RequestCallTrait for () {
	fn call(&self) -> Result<bytes::Bytes, Error> {
		Ok(bytes::Bytes::new())
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
		.map_err(Error::from)
		.map_err(retry::RetryError::NoRetry)
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
			self.unauthed
				.state
				.upload_limiter
				.change_rate_per_sec(upload_kbps),
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

	pub(crate) async fn get<Res>(&self, endpoint: Cow<'static, str>) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
	{
		let url = gateway_url(&endpoint);

		let builder = ServiceBuilder::new()
			.layer(logging::LogLayer::new(self.state.log_level, endpoint)) // optional logging
			.layer(url_parser::UrlParseLayer) // required to parse URL string to reqwest::Url
			.layer(self.state.concurrency.clone()) // optional
			.layer(self.state.retry.clone()) // required to map RetryError to Error
			.layer(self.state.rate_limiter.clone()) // optional
			.layer(deserialize::DeserializeLayer::<Res>::new()) // required to convert AsRef<u8> to T
			.layer(download_body::full::DownloadLayer::new()) // required to download full response body to bytes
			.map_request(|request: Request<(), reqwest::Url>| {
				request.into_builder_map_body(|()| bytes::Bytes::new())
			}); // required to map Request to RequestBuilder

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
			.layer(logging::LogLayer::new(self.state.log_level, endpoint)) // optional logging
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
			.layer(logging::LogLayer::new(self.state.log_level, endpoint)) // optional logging
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

	pub(crate) async fn get_raw_bytes(
		&self,
		url: &str,
		endpoint: Cow<'static, str>,
	) -> Result<Vec<u8>, Error> {
		let builder = ServiceBuilder::new()
			.layer(logging::LogLayer::new(self.state.log_level, endpoint)) // optional logging
			.layer(url_parser::UrlParseLayer) // required to parse URL string to reqwest::Url
			.layer(self.state.concurrency.clone()) // optional
			.layer(self.state.retry.clone()) // required to map RetryError to Error
			.layer(self.state.rate_limiter.clone()) // optional
			.map_request(|request: Request<(), reqwest::Url>| {
				request.into_builder_map_body(|()| bytes::Bytes::new())
			}) // required to map Request to RequestBuilder
			.layer(download_body::full::DownloadLayer::new()); // required to download full response body to bytes
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
			.layer(logging::LogLayer::new(
				self.unauthed.state.log_level,
				endpoint,
			)) // optional logging
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
			.layer(logging::LogLayer::new(
				self.unauthed.state.log_level,
				endpoint,
			)) // optional logging
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
			.layer(logging::LogLayer::new(
				self.unauthed.state.log_level,
				endpoint,
			)) // optional logging
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
			.layer(logging::LogLayer::new(
				self.unauthed.state.log_level,
				endpoint,
			)) // optional logging
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

impl<Body, Url> Request<Body, Url> {
	fn try_map_body<B, E>(
		self,
		map_body: impl FnOnce(Body) -> Result<B, E>,
	) -> Result<Request<B, Url>, E> {
		let body = match self.method {
			RequestMethod::Get => RequestMethod::Get,
			RequestMethod::Post(body) => RequestMethod::Post(map_body(body)?),
		};
		Ok(Request {
			method: body,
			response_type: self.response_type,
			url: self.url,
			client: self.client,
		})
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
