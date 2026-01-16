pub use filen_types::api::v3::user::password::forgot::{ENDPOINT, Request};

use crate::{auth::http::UnauthorizedClient, error::Error};

pub(crate) mod reset;

pub(crate) async fn post(
	client: &impl UnauthorizedClient,
	request: &Request<'_>,
) -> Result<(), Error> {
	client.post(ENDPOINT.into(), request).await
}
