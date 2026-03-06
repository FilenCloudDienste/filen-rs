pub use filen_types::api::v3::register::{ENDPOINT, Request};

use crate::{auth::unauth::UnauthClient, error::Error};

pub(crate) async fn post(client: &UnauthClient, request: &Request<'_>) -> Result<(), Error> {
	client.post(ENDPOINT.into(), request).await
}
