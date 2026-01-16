pub use filen_types::api::v3::notes::history::{ENDPOINT, Request, Response};

use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) mod restore;

pub(crate) async fn post(
	client: &impl AuthorizedClient,
	request: &Request,
) -> Result<Response<'static>, Error> {
	client.post_auth(ENDPOINT.into(), request).await
}
