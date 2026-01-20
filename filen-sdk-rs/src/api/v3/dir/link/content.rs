pub use filen_types::api::v3::dir::link::content::{ENDPOINT, Request, Response};

use crate::{auth::http::AuthorizedClient, error::Error};

const PATH: &str = "v3/dir/link/content";

pub(crate) async fn post(
	client: &impl AuthorizedClient,
	request: &Request<'_>,
) -> Result<Response<'static>, Error> {
	client.post_auth(ENDPOINT.into(), request).await
}
