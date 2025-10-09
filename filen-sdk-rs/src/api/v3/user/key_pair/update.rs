pub use filen_types::api::v3::user::key_pair::update::{ENDPOINT, Request};

use crate::{api::post_auth_request_empty, auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post(
	client: impl AuthorizedClient,
	request: &Request<'_>,
) -> Result<(), Error> {
	post_auth_request_empty(client, request, ENDPOINT).await
}
