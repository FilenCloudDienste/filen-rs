pub use filen_types::api::v3::user::public_key::{ENDPOINT, Request, Response};

use crate::{auth::http::AuthClient, error::Error};

// Unused for now: kept as a thin wrapper for the public-key endpoint we may want to call later.
#[allow(dead_code)]
pub(crate) async fn post(client: &AuthClient, request: &Request<'_>) -> Result<Response, Error> {
	client.post_auth(ENDPOINT.into(), request).await
}
