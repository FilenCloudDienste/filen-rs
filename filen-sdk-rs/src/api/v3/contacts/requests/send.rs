pub use filen_types::api::v3::contacts::requests::send::{ENDPOINT, Request, Response};

use crate::{auth::http::AuthClient, error::Error};

pub(crate) async fn post(client: &AuthClient, request: &Request<'_>) -> Result<Response, Error> {
	client.post_auth(ENDPOINT.into(), request).await
}
