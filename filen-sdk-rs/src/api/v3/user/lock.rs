pub use filen_types::api::v3::user::lock::{ENDPOINT, Response};

use crate::{auth::http::AuthClient, error::Error, sync::lock::LockPayload};

/// Sends `payload` to `url`, a gateway's lock endpoint, under the lock requests' own
/// concurrency limit (see [`AuthClient::post_raw_bytes_auth_lock`]).
pub(crate) async fn post(
	client: &AuthClient,
	url: &str,
	payload: &LockPayload,
) -> Result<Response<'static>, Error> {
	client
		.post_raw_bytes_auth_lock(payload.body(), url, ENDPOINT.into())
		.await
}
