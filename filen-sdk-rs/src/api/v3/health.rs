pub use filen_types::api::v3::health::{ENDPOINT, Response};

use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post(client: &impl AuthorizedClient) -> Result<Response, Error> {
	client.get_auth(ENDPOINT.into()).await
}
