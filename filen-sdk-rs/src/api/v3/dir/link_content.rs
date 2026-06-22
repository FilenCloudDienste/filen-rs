pub use filen_types::api::v3::dir::link_content::{ENDPOINT, Request, Response};

use crate::{auth::http::AuthClient, error::Error};

// Unused for now: kept as a thin wrapper for the link-content endpoint we may want to call later.
#[allow(dead_code)]
pub(crate) async fn post(
	client: &AuthClient,
	request: &Request,
) -> Result<Response<'static>, Error> {
	client.post_auth(ENDPOINT.into(), request).await
}
