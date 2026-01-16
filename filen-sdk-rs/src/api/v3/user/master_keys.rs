pub use filen_types::api::v3::user::master_keys::{ENDPOINT, Request, Response};

use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post(
	client: &impl AuthorizedClient,
	request: &Request<'_>,
) -> Result<Response<'static>, Error> {
	client.post_auth(ENDPOINT.into(), request).await
}
