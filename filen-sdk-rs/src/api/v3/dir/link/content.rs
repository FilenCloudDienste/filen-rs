pub use filen_types::api::v3::dir::link::content::{ENDPOINT, Request, Response};

use crate::{api::post_auth_request, auth::http::AuthorizedClient, error::Error};

const PATH: &str = "v3/dir/link/content";

pub(crate) async fn post(
	client: &impl AuthorizedClient,
	request: &Request<'_>,
) -> Result<Response<'static>, Error> {
	post_auth_request(client, request, ENDPOINT).await
}
