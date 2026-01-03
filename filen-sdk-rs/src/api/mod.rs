#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::time::Instant;
use std::{borrow::Cow, cmp::min, time::Duration};

use bytes::Bytes;
use filen_types::{api::response::FilenResponse, error::ResponseError};
use futures::StreamExt;
use reqwest::RequestBuilder;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::std::Instant;

use crate::{
	ErrorKind,
	auth::http::{AuthorizedClient, UnauthorizedClient},
	consts::{CALLBACK_INTERVAL, gateway_url},
	error::{Error, ErrorExt, ResultExt},
};

pub(crate) mod download;
pub(crate) mod v3;

pub(crate) const DEFAULT_NUM_RETRIES: usize = 7;
pub(crate) const DEFAULT_MAX_RETRY_TIME: Duration = Duration::from_secs(30);

fn fibonacci_iter(max_retry_time: Duration) -> impl Iterator<Item = Duration> {
	std::iter::successors(
		Some((
			max_retry_time,
			Duration::from_secs(0),
			Duration::from_millis(250),
		)),
		|&(max, a, b)| Some((max, b, min(max, a + b))),
	)
	.map(|(_, a, _)| a)
}

/// Represents an error that occured during the `response_handler` closure in [`retry_wrap`].
///
/// The `Retry` variant may still be returned if the number of retries is > [`DEFAULT_NUM_RETRIES`].
pub(crate) enum RetryError {
	Retry(Error),
	NoRetry(Error),
}

pub(crate) async fn large_retry_wrap<T>(
	body_bytes: Bytes,
	request_builder_fn: impl Fn() -> RequestBuilder,
	endpoint: impl Into<Cow<'static, str>>,
	response_handler: impl AsyncFnMut(reqwest::Response) -> Result<T, RetryError>,
	attempts: usize,
	max_retry_time: Duration,
) -> Result<T, Error> {
	retry_wrap(
		body_bytes,
		|| {
			request_builder_fn()
				.timeout(Duration::from_secs(600))
				.header("msgpack", "1")
		},
		endpoint,
		response_handler,
		attempts,
		max_retry_time,
	)
	.await
}

/// Retries a request with exponential backoff using the Fibonacci sequence for delays.
///
/// `body_bytes` are the bytes to be sent in the request.
///
/// `request_builder_fn` returns the request builder for the request to be sent.
///
/// `endpoint` is used for logging and error handling.
///
/// `response_handler` is passed the response body and returns a result with the error being [`RetryError`].
/// - `Ok(data)` if the request was successful,
/// - `Err(RetryError::Retry(error))` if the request should be retried
/// - `Err(RetryError::NoRetry(error))` if the request failed and should not be retried.
///
/// `attempts` specifies the number of attempts to retry the request.
pub(crate) async fn retry_wrap<T>(
	body_bytes: Bytes,
	request_builder_fn: impl Fn() -> RequestBuilder,
	endpoint: impl Into<Cow<'static, str>>,
	mut response_handler: impl AsyncFnMut(reqwest::Response) -> Result<T, RetryError>,
	attempts: usize,
	max_retry_time: Duration,
) -> Result<T, Error> {
	let endpoint = endpoint.into();
	let mut last_error: Option<Error> = None;
	for (i, delay) in (0..attempts).zip(fibonacci_iter(max_retry_time)) {
		if i > 0 {
			#[cfg(not(target_family = "wasm"))]
			tokio::time::sleep(delay).await;
			#[cfg(target_family = "wasm")]
			wasmtimer::tokio::sleep(delay).await;
			log::warn!(
				"Retrying: {endpoint} ({i}/{attempts}) after {}ms",
				delay.as_millis()
			);
		}

		// cloning the body bytes is necessary because the request builder consumes it
		// fortunately cloning it is allocation free
		// cloning a new request builder is not free
		// which is why we use a closure rather than cloning.
		let resp = match request_builder_fn().body(body_bytes.clone()).send().await {
			Ok(resp) => resp,
			Err(e) if e.is_timeout() => {
				log::warn!("Request to {endpoint} timed out");
				last_error = Some(e.with_context(endpoint.clone()));
				continue;
			}
			// wish I could use a if let guard here
			Err(e) if e.status().is_some() => {
				let status = e.status().expect("status should be present");
				log::warn!("Request to {endpoint} failed with status {status}");
				last_error = Some(e.with_context(endpoint.clone()));
				continue;
			}
			Err(e) => {
				#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
				use std::error::Error as StdError;
				#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
				if let Some(source) = e.source()
					&& let Some(e) = source.downcast_ref::<hyper_util::client::legacy::Error>()
					&& e.is_connect()
				{
					// dns error
					log::warn!("Request to {endpoint} failed to connect: {e}");
					last_error = Some(
						Error::custom(ErrorKind::Reqwest, e.to_string())
							.with_context(endpoint.clone()),
					);
					continue;
				}

				log::error!("Request to {endpoint} failed: {e}");
				return Err(e.with_context(endpoint));
			}
		};

		match response_handler(resp).await {
			Ok(data) => return Ok(data),
			Err(RetryError::Retry(e)) => {
				log::warn!("Request to {endpoint} failed, retrying: {e}");
				last_error = Some(e.with_context(endpoint.clone()));
			}
			Err(RetryError::NoRetry(e)) => {
				log::error!("Request to {endpoint} failed, not retrying: {e}");
				return Err(e.with_context(endpoint));
			}
		}
	}
	log::error!("Request to {endpoint} failed after {attempts} retries",);
	Err(last_error.expect("retries must be more than 0"))
}

pub(crate) async fn handle_request<U>(
	body_bytes: Bytes,
	request_builder_fn: impl Fn() -> RequestBuilder,
	endpoint: &'static str,
	attempts: usize,
	max_retry_time: Duration,
) -> Result<FilenResponse<'static, U>, Error>
where
	U: serde::de::DeserializeOwned,
{
	retry_wrap(
		body_bytes,
		request_builder_fn,
		endpoint,
		async |resp| {
			let body = match resp.json::<FilenResponse<U>>().await {
				Ok(body) => body,
				Err(e) => {
					log::error!("Failed to parse response from {endpoint}: {e}");
					return Err(RetryError::NoRetry(e.with_context(endpoint)));
				}
			};

			if let Some(ResponseError::ApiError { message, code }) = body.as_error() {
				log::warn!("Request to {endpoint} failed. message: {message:?}, code: {code:?}");
				if let Some("internal_error") = code.as_deref() {
					log::warn!("Internal error, retrying: {message:?}");
					return Err(RetryError::Retry(
						ResponseError::ApiError { message, code }.with_context(endpoint),
					));
				} else {
					return Err(RetryError::NoRetry(
						ResponseError::ApiError { message, code }.with_context(endpoint),
					));
				}
			}
			Ok(body)
		},
		attempts,
		max_retry_time,
	)
	.await
}

pub(crate) async fn handle_large_request<U>(
	body_bytes: Bytes,
	request_builder_fn: impl Fn() -> RequestBuilder,
	endpoint: &'static str,
	attempts: usize,
	max_retry_time: Duration,
	mut progress_callback: Option<&mut impl FnMut(u64, Option<u64>)>,
) -> Result<FilenResponse<'static, U>, Error>
where
	U: serde::de::DeserializeOwned,
{
	large_retry_wrap(
		body_bytes,
		request_builder_fn,
		endpoint,
		async move |resp| {
			let real_content_length = resp
				.headers()
				.get("X-Cl")
				.and_then(|h| h.to_str().ok().and_then(|h| str::parse::<u64>(h).ok()));
			let content_length: usize = real_content_length
				.unwrap_or_default()
				.try_into()
				.map_err(|e| {
					RetryError::NoRetry(Error::custom_with_source(
						ErrorKind::InsufficientMemory,
						e,
						Some(format!("content length too large for {endpoint}")),
					))
				})?;
			let mut body_bytes = Vec::with_capacity(content_length);
			let mut body = resp.bytes_stream();

			let mut last_update_time = Instant::now();

			while let Some(chunk) = body.next().await {
				let chunk = chunk.map_err(|e| {
					if e.is_timeout() {
						log::warn!("Request to {endpoint} timed out during download");
						RetryError::Retry(e.with_context(endpoint))
					} else {
						log::error!("Request to {endpoint} failed during download: {e}");
						RetryError::NoRetry(e.with_context(endpoint))
					}
				})?;

				body_bytes.extend_from_slice(&chunk);
				if last_update_time.elapsed() >= CALLBACK_INTERVAL
					&& let Some(ref mut progress_callback) = progress_callback
				{
					progress_callback(body_bytes.len() as u64, real_content_length);
					last_update_time = Instant::now();
				}
			}
			if let Some(ref mut progress_callback) = progress_callback {
				progress_callback(body_bytes.len() as u64, real_content_length);
			}

			let body = match rmp_serde::from_slice::<FilenResponse<U>>(&body_bytes) {
				Ok(body) => body,
				Err(e) => {
					log::error!("Failed to parse response from {endpoint}: {e}");
					return Err(RetryError::NoRetry(e.with_context(endpoint)));
				}
			};

			if let Some(ResponseError::ApiError { message, code }) = body.as_error() {
				log::warn!("Request to {endpoint} failed. message: {message:?}, code: {code:?}");
				if let Some("internal_error") = code.as_deref() {
					log::warn!("Internal error, retrying: {message:?}");
					return Err(RetryError::Retry(
						ResponseError::ApiError { message, code }.with_context(endpoint),
					));
				} else {
					return Err(RetryError::NoRetry(
						ResponseError::ApiError { message, code }.with_context(endpoint),
					));
				}
			}
			Ok(body)
		},
		attempts,
		max_retry_time,
	)
	.await
}

async fn handle_request_debug<U>(
	request_builder: RequestBuilder,
	endpoint: &'static str,
) -> Result<FilenResponse<'static, U>, Error>
where
	U: serde::de::DeserializeOwned + std::fmt::Debug,
{
	let response = request_builder.send().await.context(endpoint)?;
	let text = response.text().await.context(endpoint)?;
	println!("{endpoint} response: {text}");
	let mut deserializer = serde_json::Deserializer::from_str(&text);
	let response: FilenResponse<U> =
		serde_path_to_error::deserialize(&mut deserializer).map_err(|e| {
			let error_string = e.to_string();
			Error::custom(
				ErrorKind::Response,
				format!("Failed to deserialize response from {endpoint}: {error_string}"),
			)
		})?;
	Ok(response)
}

pub(crate) async fn post_request<T, U>(
	client: &impl UnauthorizedClient,
	request: &T,
	endpoint: &'static str,
) -> Result<U, Error>
where
	T: serde::Serialize,
	U: serde::de::DeserializeOwned + std::fmt::Debug,
{
	handle_request(
		Bytes::from_owner(serde_json::to_vec(request).context(endpoint)?),
		|| {
			client.post_request(gateway_url(endpoint)).header(
				reqwest::header::CONTENT_TYPE,
				reqwest::header::HeaderValue::from_static("application/json"),
			)
		},
		endpoint,
		DEFAULT_NUM_RETRIES,
		DEFAULT_MAX_RETRY_TIME,
	)
	.await?
	.into_data()
	.context(endpoint)
}

pub(crate) async fn post_request_debug<T, U>(
	client: &impl UnauthorizedClient,
	request: &T,
	endpoint: &'static str,
) -> Result<U, Error>
where
	T: serde::Serialize,
	U: serde::de::DeserializeOwned + std::fmt::Debug,
{
	println!(
		"{} request: {:?}",
		endpoint,
		serde_json::to_string(request)?
	);
	handle_request_debug(
		client.post_request(gateway_url(endpoint)).json(request),
		endpoint,
	)
	.await?
	.into_data()
	.context(endpoint)
}

pub(crate) async fn post_request_empty<T>(
	client: &impl UnauthorizedClient,
	request: &T,
	endpoint: &'static str,
) -> Result<(), Error>
where
	T: serde::Serialize,
{
	handle_request::<()>(
		Bytes::from_owner(serde_json::to_vec(request).context(endpoint)?),
		|| client.post_request(gateway_url(endpoint)).json(request),
		endpoint,
		DEFAULT_NUM_RETRIES,
		DEFAULT_MAX_RETRY_TIME,
	)
	.await?
	.ignore_data()
	.context(endpoint)
}

pub(crate) async fn post_request_empty_debug<T>(
	client: &impl UnauthorizedClient,
	request: &T,
	endpoint: &'static str,
) -> Result<(), Error>
where
	T: serde::Serialize,
{
	println!(
		"{} request: {:?}",
		endpoint,
		serde_json::to_string(request)?
	);
	handle_request::<()>(
		Bytes::from_owner(serde_json::to_vec(request).context(endpoint)?),
		|| client.post_request(gateway_url(endpoint)).json(request),
		endpoint,
		DEFAULT_NUM_RETRIES,
		DEFAULT_MAX_RETRY_TIME,
	)
	.await?
	.ignore_data()
	.context(endpoint)
}

pub(crate) async fn post_large_auth_request<T, U>(
	client: &impl AuthorizedClient,
	request: &T,
	endpoint: &'static str,
	progress_callback: Option<&mut impl FnMut(u64, Option<u64>)>,
) -> Result<U, Error>
where
	T: serde::Serialize,
	U: serde::de::DeserializeOwned + std::fmt::Debug,
{
	let _permit = client.get_semaphore_permit().await;
	handle_large_request(
		Bytes::from_owner(serde_json::to_vec(request).context(endpoint)?),
		|| {
			client.post_auth_request(gateway_url(endpoint)).header(
				reqwest::header::CONTENT_TYPE,
				reqwest::header::HeaderValue::from_static("application/json"),
			)
		},
		endpoint,
		DEFAULT_NUM_RETRIES,
		DEFAULT_MAX_RETRY_TIME,
		progress_callback,
	)
	.await?
	.into_data()
	.context(endpoint)
}

pub(crate) async fn post_auth_request<T, U>(
	client: &impl AuthorizedClient,
	request: &T,
	endpoint: &'static str,
) -> Result<U, Error>
where
	T: serde::Serialize,
	U: serde::de::DeserializeOwned + std::fmt::Debug,
{
	let _permit = client.get_semaphore_permit().await;
	handle_request(
		Bytes::from_owner(serde_json::to_vec(request).context(endpoint)?),
		|| {
			client.post_auth_request(gateway_url(endpoint)).header(
				reqwest::header::CONTENT_TYPE,
				reqwest::header::HeaderValue::from_static("application/json"),
			)
		},
		endpoint,
		DEFAULT_NUM_RETRIES,
		DEFAULT_MAX_RETRY_TIME,
	)
	.await?
	.into_data()
	.context(endpoint)
}

pub(crate) async fn post_auth_request_debug<T, U>(
	client: &impl AuthorizedClient,
	request: &T,
	endpoint: &'static str,
) -> Result<U, Error>
where
	T: serde::Serialize + std::fmt::Debug,
	U: serde::de::DeserializeOwned + std::fmt::Debug,
{
	println!(
		"{} request: {:?}",
		endpoint,
		serde_json::to_string(request)?
	);
	let _permit = client.get_semaphore_permit().await;
	handle_request_debug(
		client
			.post_auth_request(gateway_url(endpoint))
			.json(request),
		endpoint,
	)
	.await?
	.into_data()
	.context(endpoint)
}

pub(crate) async fn post_auth_request_no_body_empty(
	client: &impl AuthorizedClient,
	endpoint: &'static str,
) -> Result<(), Error> {
	let _permit = client.get_semaphore_permit().await;
	handle_request::<()>(
		Bytes::new(),
		|| client.post_auth_request(gateway_url(endpoint)),
		endpoint,
		DEFAULT_NUM_RETRIES,
		DEFAULT_MAX_RETRY_TIME,
	)
	.await?
	.ignore_data()
	.context(endpoint)
}

pub(crate) async fn post_auth_request_empty<T>(
	client: &impl AuthorizedClient,
	request: &T,
	endpoint: &'static str,
) -> Result<(), Error>
where
	T: serde::Serialize,
{
	let _permit = client.get_semaphore_permit().await;
	handle_request::<()>(
		Bytes::from_owner(serde_json::to_vec(request).context(endpoint)?),
		|| {
			client.post_auth_request(gateway_url(endpoint)).header(
				reqwest::header::CONTENT_TYPE,
				reqwest::header::HeaderValue::from_static("application/json"),
			)
		},
		endpoint,
		DEFAULT_NUM_RETRIES,
		DEFAULT_MAX_RETRY_TIME,
	)
	.await?
	.ignore_data()
	.context(endpoint)
}

pub(crate) async fn post_auth_request_empty_debug<T>(
	client: &impl AuthorizedClient,
	request: &T,
	endpoint: &'static str,
) -> Result<(), Error>
where
	T: serde::Serialize,
{
	println!(
		"{} request: {:?}",
		endpoint,
		serde_json::to_string(request)?
	);
	let _permit = client.get_semaphore_permit().await;
	handle_request_debug::<()>(
		client
			.post_auth_request(gateway_url(endpoint))
			.json(request),
		endpoint,
	)
	.await?
	.ignore_data()
	.context(endpoint)
}

pub(crate) async fn get_auth_request<T>(
	client: &impl AuthorizedClient,
	endpoint: &'static str,
) -> Result<T, Error>
where
	T: serde::de::DeserializeOwned + std::fmt::Debug,
{
	let _permit = client.get_semaphore_permit().await;
	handle_request(
		Bytes::new(),
		|| client.get_auth_request(gateway_url(endpoint)),
		endpoint,
		DEFAULT_NUM_RETRIES,
		DEFAULT_MAX_RETRY_TIME,
	)
	.await?
	.into_data()
	.context(endpoint)
}

pub(crate) async fn get_auth_request_debug<T>(
	client: &impl AuthorizedClient,
	endpoint: &'static str,
) -> Result<T, Error>
where
	T: serde::de::DeserializeOwned + std::fmt::Debug,
{
	let _permit = client.get_semaphore_permit().await;
	handle_request_debug(client.get_auth_request(gateway_url(endpoint)), endpoint)
		.await?
		.into_data()
		.context(endpoint)
}
