pub use filen_types::api::v3::health::{ENDPOINT, Response};

use crate::{auth::http::AuthClient, error::Error};

// Unused for now: kept as a thin wrapper for the health endpoint we may want to call later.
#[allow(dead_code)]
pub(crate) async fn post(client: &AuthClient) -> Result<Response, Error> {
	client.get_auth(ENDPOINT.into()).await
}
