use filen_types::api::response::FilenResponse;
use reqwest::RequestBuilder;

use crate::{
	auth::http::{AuthorizedClient, UnauthorizedClient},
	consts::gateway_url,
	error::{Error, ErrorExt},
};

pub(crate) mod download;
pub(crate) mod v3;

async fn handle_request<U>(
	request_builder: RequestBuilder,
	endpoint: &'static str,
) -> Result<FilenResponse<'static, U>, Error>
where
	U: serde::de::DeserializeOwned + std::fmt::Debug,
{
	request_builder
		.send()
		.await
		.context(endpoint)?
		.json::<FilenResponse<U>>()
		.await
		.context(endpoint)
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
	println!("{} response: {}", endpoint, text);
	let mut deserializer = serde_json::Deserializer::from_str(&text);
	let response: FilenResponse<U> =
		serde_path_to_error::deserialize(&mut deserializer).map_err(|e| {
			let error_string = e.to_string();
			Error::ErrorWithContext(Box::new(Error::Custom(error_string)), endpoint)
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
		client.post_request(gateway_url(endpoint)).json(request),
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
		client
			.post_auth_request(gateway_url(endpoint))
			.json(request),
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
	handle_request::<()>(client.post_auth_request(gateway_url(endpoint)), endpoint)
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
		client
			.post_auth_request(gateway_url(endpoint))
			.json(request),
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
	handle_request(client.get_auth_request(gateway_url(endpoint)), endpoint)
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
