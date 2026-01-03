pub use filen_types::api::v3::health::{ENDPOINT, Response};

use crate::{api::get_auth_request, auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post(client: &impl AuthorizedClient) -> Result<Response, Error> {
	get_auth_request(client, ENDPOINT).await
}
