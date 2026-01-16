pub use filen_types::api::v3::chat::send::{ENDPOINT, Request, Response};

use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post(
	client: &impl AuthorizedClient,
	request: &Request<'_>,
) -> Result<Response, Error> {
	client.post_auth(ENDPOINT.into(), request).await
}
