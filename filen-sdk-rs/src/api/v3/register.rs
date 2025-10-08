pub use filen_types::api::v3::register::{ENDPOINT, Request};

use crate::{api::post_request_empty, auth::http::UnauthorizedClient, error::Error};

pub(crate) async fn post(
	client: impl UnauthorizedClient,
	request: &Request<'_>,
) -> Result<(), Error> {
	post_request_empty(client, request, ENDPOINT).await
}
