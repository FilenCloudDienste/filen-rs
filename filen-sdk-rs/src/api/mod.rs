use std::time::Duration;

use bytes::Bytes;
use filen_types::api::response::FilenResponse;
use reqwest::RequestBuilder;

use crate::{
	ErrorKind,
	auth::http::{AuthorizedClient, UnauthorizedClient},
	consts::gateway_url,
	error::{Error, ErrorExt, ResultExt},
};

pub(crate) mod download;
pub(crate) mod v3;

const NUM_RETRIES: u64 = 7;

fn fibonacci_iter() -> impl Iterator<Item = u64> {
	std::iter::successors(Some((0_u64, Some(1))), |&(a, b)| {
		Some((b?, a.checked_add(b?)))
	})
	.map(|(a, _)| a)
}

async fn handle_request<U>(
	body_bytes: Bytes,
	request_builder_fn: impl Fn() -> RequestBuilder,
	endpoint: &'static str,
) -> Result<FilenResponse<'static, U>, Error>
where
	U: serde::de::DeserializeOwned + std::fmt::Debug,
{
	let mut last_error: Option<Error> = None;
	for (i, delay) in (0..=NUM_RETRIES).zip(fibonacci_iter()) {
		if i > 0 {
			futures_timer::Delay::new(Duration::from_secs(delay)).await;
			log::warn!("Retrying: {endpoint} ({}/{NUM_RETRIES})", i);
		}

		// cloning the body bytes is necessary because the request builder consumes it
		// fortunately cloning it is allocation free
		// cloning a new request builder is not free
		// which is why we use a closure rather than cloning.
		let resp = match request_builder_fn().body(body_bytes.clone()).send().await {
			Ok(resp) => resp,
			Err(e) if e.is_timeout() => {
				log::warn!("Request to {endpoint} timed out");
				last_error = Some(e.with_context(endpoint));
				continue;
			}
			// wish I could use a if let guard here
			Err(e) if e.status().is_some() => {
				let status = e.status().expect("status should be present");
				log::warn!("Request to {endpoint} failed with status {status}",);
				last_error = Some(e.with_context(endpoint));
				continue;
			}
			Err(e) => {
				log::error!("Request to {endpoint} failed: {}", e);
				return Err(e.with_context(endpoint));
			}
		};

		let body = match resp.json::<FilenResponse<U>>().await {
			Ok(body) => body,
			Err(e) => {
				log::error!("Failed to parse response from {endpoint}: {}", e);
				return Err(e.with_context(endpoint));
			}
		};

		if let Some(e) = body.as_error() {
			log::warn!("Request to {endpoint} failed: {}", e);
			last_error = Some(e.with_context(endpoint));
			continue;
		}
		return Ok(body);
	}
	Err(last_error.expect("retries must be more than 0"))
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
	client: impl UnauthorizedClient,
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
	)
	.await?
	.into_data()
	.context(endpoint)
}

pub(crate) async fn post_request_debug<T, U>(
	client: impl UnauthorizedClient,
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

pub(crate) async fn post_auth_request<T, U>(
	client: impl AuthorizedClient,
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
	)
	.await?
	.into_data()
	.context(endpoint)
}

pub(crate) async fn post_auth_request_debug<T, U>(
	client: impl AuthorizedClient,
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
	client: impl AuthorizedClient,
	endpoint: &'static str,
) -> Result<(), Error> {
	let _permit = client.get_semaphore_permit().await;
	handle_request::<()>(
		Bytes::new(),
		|| client.post_auth_request(gateway_url(endpoint)),
		endpoint,
	)
	.await?
	.ignore_data()
	.context(endpoint)
}

pub(crate) async fn post_auth_request_empty<T>(
	client: impl AuthorizedClient,
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
	)
	.await?
	.ignore_data()
	.context(endpoint)
}

pub(crate) async fn post_auth_request_empty_debug<T>(
	client: impl AuthorizedClient,
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
	client: impl AuthorizedClient,
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
	)
	.await?
	.into_data()
	.context(endpoint)
}

pub(crate) async fn get_auth_request_debug<T>(
	client: impl AuthorizedClient,
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
