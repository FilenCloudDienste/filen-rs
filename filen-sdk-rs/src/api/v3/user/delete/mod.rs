pub use filen_types::api::v3::user::delete::{ENDPOINT, Request};

use crate::{auth::http::AuthClient, error::Error};

pub(crate) async fn post(client: &AuthClient, request: &Request<'_>) -> Result<(), Error> {
	client.post_auth(ENDPOINT.into(), request).await
}
