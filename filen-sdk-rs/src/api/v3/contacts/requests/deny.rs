pub use filen_types::api::v3::contacts::requests::deny::{ENDPOINT, Request};

use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post(client: &impl AuthorizedClient, request: &Request) -> Result<(), Error> {
	client.post_auth(ENDPOINT.into(), request).await
}
