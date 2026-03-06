pub use filen_types::api::v3::dir::size::link::{ENDPOINT, Request, Response};

use crate::{auth::unauth::UnauthClient, error::Error};

pub(crate) async fn post(client: &UnauthClient, request: &Request) -> Result<Response, Error> {
	client.post(ENDPOINT.into(), request).await
}
