use filen_types::api::v3::trash::empty::ENDPOINT;

use crate::{api::post_auth_request_no_body_empty, auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post(client: impl AuthorizedClient) -> Result<(), Error> {
	post_auth_request_no_body_empty(client, ENDPOINT).await
}
