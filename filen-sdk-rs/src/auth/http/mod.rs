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

use crate::{
	Error,
	auth::{Client, http::auth::AuthLayer},
	consts::gateway_url,
};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use bandwidth_limit::{
	BandwidthLimiter, DownloadBandwidthLimiterLayer, UploadBandwidthLimiterLayer,
	new_download_bandwidth_limiter, new_upload_bandwidth_limiter,
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

pub(crate) struct ClientConfig {
	concurrency: usize,
	retry_budget: TpsBudget,
	rate_limit_per_sec: NonZeroU32,
	upload_bandwidth_kilobytes_per_sec: Option<NonZeroU32>,
	download_bandwidth_kilobytes_per_sec: Option<NonZeroU32>,
	log_level: log::LevelFilter,
}

impl Default for ClientConfig {
	fn default() -> Self {
		Self {
			concurrency: 32,
			retry_budget: TpsBudget::default(),
			rate_limit_per_sec: NonZeroU32::new(64).unwrap(),
			upload_bandwidth_kilobytes_per_sec: None,
			download_bandwidth_kilobytes_per_sec: None,
			log_level: log::LevelFilter::Debug,
		}
	}
}

#[derive(Clone)]
pub(crate) struct SharedClientState {
	concurrency: GlobalConcurrencyLimitLayer,
	retry: retry::RetryMapLayer,
	rate_limiter: limit::GlobalRateLimitLayer,
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	upload_limiter: Option<Arc<BandwidthLimiter>>,
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	download_limiter: DownloadBandwidthLimiterLayer,
	log_level: log::LevelFilter,
}

impl SharedClientState {
	pub(crate) fn new(config: ClientConfig) -> Result<Self, Error> {
		Ok(Self {
			concurrency: GlobalConcurrencyLimitLayer::new(config.concurrency),
			retry: retry::RetryMapLayer::new(retry::RetryPolicy::new(config.retry_budget)),
			rate_limiter: limit::GlobalRateLimitLayer::new(limit::RateConfig::new(
				config.rate_limit_per_sec,
				std::time::Duration::from_secs(1),
			))
			.expect("1s is a valid duration"),
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			upload_limiter: config
				.upload_bandwidth_kilobytes_per_sec
				.map(|kbps| Ok::<_, Error>(Arc::new(new_upload_bandwidth_limiter(kbps)?)))
				.transpose()?,
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			download_limiter: DownloadBandwidthLimiterLayer::new(
				config
					.download_bandwidth_kilobytes_per_sec
					.map(|kbps| Arc::new(new_download_bandwidth_limiter(kbps))),
			),
			log_level: config.log_level,
		})
	}
}

pub(crate) struct UnauthClient {
	state: SharedClientState,
	reqwest_client: reqwest::Client,
}

impl UnauthClient {
	pub(crate) fn new(state: SharedClientState) -> Self {
		Self {
			reqwest_client: reqwest::Client::new(),
			state,
		}
	}

	pub(crate) fn into_authed(self, api_key: Arc<RwLock<APIKey<'static>>>) -> AuthClient {
		AuthClient {
			unauthed: self,
			api_key,
		}
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

impl UnauthClient {
	async fn inner_post<Req, Res>(
		&self,
		request: Request<(), String>,
		endpoint: Cow<'static, str>,
		body: &Req,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug,
	{
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
					.layer(UploadBandwidthLimiterLayer::new(
						self.state.upload_limiter.as_ref(),
					)) // required to map Request to RequestBuilder
					.layer(self.state.download_limiter.clone()) // optional
			}
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				builder.map_request(|r: Request<Bytes, reqwest::Url>| -> RequestBuilder {
					r.into_builder_map_body(|b| b)
				})
			}
		};

		builder.service_fn(execute_request).oneshot(request).await
	}

	async fn inner_post_large<Req, Res, F>(
		&self,
		request: Request<(), String>,
		endpoint: Cow<'static, str>,
		body: &Req,
		callback: Option<&F>,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug,
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
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
					.layer(UploadBandwidthLimiterLayer::new(
						self.state.upload_limiter.as_ref(),
					)) // required to map Request to RequestBuilder
					.layer(self.state.download_limiter.clone()) // optional
			}
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				builder.map_request(|r: Request<Bytes, reqwest::Url>| -> RequestBuilder {
					r.into_builder_map_body(|b| b)
				})
			}
		};

		builder.service_fn(execute_request).oneshot(request).await
	}

	async fn inner_get<Res>(
		&self,
		request: Request<(), String>,
		endpoint: Cow<'static, str>,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
	{
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

		builder.service_fn(execute_request).oneshot(request).await
	}
}

pub(crate) struct AuthClient {
	unauthed: UnauthClient,
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

	async fn inner_post<Req, Res>(
		&self,
		request: Request<(), String>,
		endpoint: Cow<'static, str>,
		body: &Req,
		auth: bool,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug,
	{
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
						self.unauthed.state.upload_limiter.as_ref(),
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
			.option_layer(if auth {
				Some(AuthLayer::new(&self.api_key))
			} else {
				None
			})
			.service_fn(execute_request)
			.oneshot(request)
			.await // optional
	}

	async fn inner_post_large<Req, Res, F>(
		&self,
		request: Request<(), &str>,
		endpoint: Cow<'static, str>,
		body: &Req,
		callback: Option<&F>,
		auth: bool,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug,
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
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
						self.unauthed.state.upload_limiter.as_ref(),
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
			.option_layer(if auth {
				// optional
				Some(auth::AuthLayer::new(&self.api_key))
			} else {
				None
			})
			.service_fn(execute_request)
			.oneshot(request)
			.await
	}

	async fn inner_get<Res>(
		&self,
		request: Request<(), String>,
		endpoint: Cow<'static, str>,
		auth: bool,
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
			.option_layer(if auth {
				// optional
				Some(auth::AuthLayer::new(&self.api_key))
			} else {
				None
			})
			.service_fn(execute_request)
			.oneshot(request)
			.await
	}

	async fn inner_post_raw_bytes<Res>(
		&self,
		request: Request<Bytes, &str>,
		endpoint: Cow<'static, str>,
		auth: bool,
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
						self.unauthed.state.upload_limiter.as_ref(),
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
			.option_layer(if auth {
				// optional
				Some(auth::AuthLayer::new(&self.api_key))
			} else {
				None
			})
			.service_fn(execute_request)
			.oneshot(request)
			.await
	}

	async fn inner_get_raw_bytes(
		&self,
		request: Request<(), &str>,
		endpoint: Cow<'static, str>,
		auth: bool,
	) -> Result<Vec<u8>, Error> {
		let builder = ServiceBuilder::new()
			.layer(logging::LogLayer::new(
				self.unauthed.state.log_level,
				endpoint,
			)) // optional logging
			.layer(url_parser::UrlParseLayer) // required to parse URL string to reqwest::Url
			.layer(self.unauthed.state.concurrency.clone()) // optional
			.layer(self.unauthed.state.retry.clone()) // required to map RetryError to Error
			.layer(self.unauthed.state.rate_limiter.clone()) // optional
			.map_request(|request: Request<(), reqwest::Url>| {
				request.into_builder_map_body(|()| bytes::Bytes::new())
			}) // required to map Request to RequestBuilder
			.layer(download_body::full::DownloadLayer::new()); // required to download full response body to bytes
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
			.option_layer(if auth {
				// optional
				Some(auth::AuthLayer::new(&self.api_key))
			} else {
				None
			})
			.service_fn(execute_request)
			.oneshot(request)
			.await
	}
}

pub(crate) trait UnauthorizedClient {
	async fn get<Res>(&self, endpoint: Cow<'static, str>) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug;
	async fn post<Req, Res>(&self, endpoint: Cow<'static, str>, body: &Req) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug;
	async fn post_large_response<Req, Res, F>(
		&self,
		endpoint: Cow<'static, str>,
		body: &Req,
		callback: Option<&F>,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug,
		F: Fn(u64, Option<u64>) + Send + Sync + 'static;
}

impl UnauthorizedClient for UnauthClient {
	async fn get<Res>(&self, endpoint: Cow<'static, str>) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
	{
		self.inner_get(
			Request {
				method: RequestMethod::Get,
				response_type: ResponseType::Standard,
				url: gateway_url(&endpoint),
				client: self.reqwest_client.clone(),
			},
			endpoint,
		)
		.await
	}

	async fn post<Req, Res>(&self, endpoint: Cow<'static, str>, body: &Req) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug,
	{
		self.inner_post(
			Request {
				method: RequestMethod::Post(()),
				response_type: ResponseType::Standard,
				url: gateway_url(&endpoint),
				client: self.reqwest_client.clone(),
			},
			endpoint,
			body,
		)
		.await
	}

	async fn post_large_response<Req, Res, F>(
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
		self.inner_post_large(
			Request {
				method: RequestMethod::Post(()),
				response_type: ResponseType::Large,
				url: gateway_url(&endpoint),
				client: self.reqwest_client.clone(),
			},
			endpoint,
			body,
			callback,
		)
		.await
	}
}

impl UnauthorizedClient for AuthClient {
	async fn get<Res>(&self, endpoint: Cow<'static, str>) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
	{
		self.inner_get(
			Request {
				method: RequestMethod::Get,
				response_type: ResponseType::Standard,
				url: gateway_url(&endpoint),
				client: self.unauthed.reqwest_client.clone(),
			},
			endpoint,
			false,
		)
		.await
	}

	async fn post<Req, Res>(&self, endpoint: Cow<'static, str>, body: &Req) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug,
	{
		self.inner_post(
			Request {
				method: RequestMethod::Post(()),
				response_type: ResponseType::Standard,
				url: gateway_url(&endpoint),
				client: self.unauthed.reqwest_client.clone(),
			},
			endpoint,
			body,
			false,
		)
		.await
	}

	async fn post_large_response<Req, Res, F>(
		&self,
		endpoint: Cow<'static, str>,
		body: &Req,
		callback: Option<&F>,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug,
		F: Fn(u64, Option<u64>) + Send + Sync + 'static,
	{
		self.inner_post_large(
			Request {
				method: RequestMethod::Post(()),
				response_type: ResponseType::Large,
				url: &gateway_url(&endpoint),
				client: self.unauthed.reqwest_client.clone(),
			},
			endpoint,
			body,
			callback,
			false,
		)
		.await
	}
}

pub(crate) trait AuthorizedClient: Send + Sync {
	async fn get_auth<Res>(&self, endpoint: Cow<'static, str>) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug;
	async fn post_auth<Req, Res>(
		&self,
		endpoint: Cow<'static, str>,
		body: &Req,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug;
	async fn post_large_response_auth<Req, Res, F>(
		&self,
		endpoint: Cow<'static, str>,
		body: &Req,
		callback: Option<&F>,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug,
		F: Fn(u64, Option<u64>) + Send + Sync;

	async fn post_raw_bytes_auth<Res>(
		&self,
		request: Bytes,
		url: &str,
		endpoint: Cow<'static, str>,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug;

	async fn get_raw_bytes_auth(
		&self,
		url: &str,
		endpoint: Cow<'static, str>,
	) -> Result<Vec<u8>, Error>;
}

impl AuthorizedClient for AuthClient {
	async fn get_auth<Res>(&self, endpoint: Cow<'static, str>) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
	{
		self.inner_get(
			Request {
				method: RequestMethod::Get,
				response_type: ResponseType::Standard,
				url: gateway_url(&endpoint),
				client: self.unauthed.reqwest_client.clone(),
			},
			endpoint,
			true,
		)
		.await
	}

	async fn post_auth<Req, Res>(
		&self,
		endpoint: Cow<'static, str>,
		body: &Req,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
		Req: Serialize + Debug,
	{
		self.inner_post(
			Request {
				method: RequestMethod::Post(()),
				response_type: ResponseType::Standard,
				url: gateway_url(&endpoint),
				client: self.unauthed.reqwest_client.clone(),
			},
			endpoint,
			body,
			true,
		)
		.await
	}

	async fn post_large_response_auth<Req, Res, F>(
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
		self.inner_post_large(
			Request {
				method: RequestMethod::Post(()),
				response_type: ResponseType::Large,
				url: &gateway_url(&endpoint),
				client: self.unauthed.reqwest_client.clone(),
			},
			endpoint,
			body,
			callback,
			true,
		)
		.await
	}

	async fn post_raw_bytes_auth<Res>(
		&self,
		request: Bytes,
		url: &str,
		endpoint: Cow<'static, str>,
	) -> Result<Res, Error>
	where
		Res: DeserializeOwned + Debug,
	{
		self.inner_post_raw_bytes(
			Request {
				method: RequestMethod::Post(request),
				response_type: ResponseType::Standard,
				url,
				client: self.unauthed.reqwest_client.clone(),
			},
			endpoint,
			true,
		)
		.await
	}

	async fn get_raw_bytes_auth(
		&self,
		url: &str,
		endpoint: Cow<'static, str>,
	) -> Result<Vec<u8>, Error> {
		self.inner_get_raw_bytes(
			Request {
				method: RequestMethod::Get,
				response_type: ResponseType::Standard,
				url,
				client: self.unauthed.reqwest_client.clone(),
			},
			endpoint,
			true,
		)
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
