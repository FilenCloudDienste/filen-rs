pub use filen_types::api::v3::dir::size::{ENDPOINT, Request, Response};

use crate::{api::post_auth_request, auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post(
	client: impl AuthorizedClient,
	request: &Request,
) -> Result<Response, Error> {
	post_auth_request(client, request, ENDPOINT).await
}
