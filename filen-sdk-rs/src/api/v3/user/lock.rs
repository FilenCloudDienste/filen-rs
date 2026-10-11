use bytes::Bytes;
pub use filen_types::api::v3::user::lock::{ENDPOINT, Request, Response};

use crate::{auth::http::AuthClient, error::Error};

/// Sends `request` to `url`, a gateway's lock endpoint, under the lock requests' own
/// concurrency limit (see [`AuthClient::post_raw_bytes_auth_lock`]).
pub(crate) async fn post(
	client: &AuthClient,
	url: &str,
	request: &Request<'_>,
) -> Result<Response<'static>, Error> {
	let body = Bytes::from_owner(serde_json::to_vec(request)?);
	client
		.post_raw_bytes_auth_lock(body, url, ENDPOINT.into())
		.await
}
