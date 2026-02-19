pub use filen_types::api::v3::health::{ENDPOINT, Response};

use crate::{auth::http::AuthClient, error::Error};

pub(crate) async fn post(client: &AuthClient) -> Result<Response, Error> {
	client.get_auth(ENDPOINT.into()).await
}
