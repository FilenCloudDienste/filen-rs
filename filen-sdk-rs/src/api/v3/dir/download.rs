pub use filen_types::api::v3::dir::download::{ENDPOINT, Request, Response};

use crate::{api::post_large_auth_request, auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post_large(
	client: &impl AuthorizedClient,
	request: &Request,
	callback: Option<&mut impl FnMut(u64, Option<u64>)>,
) -> Result<Response<'static>, Error> {
	post_large_auth_request(client, request, ENDPOINT, callback).await
}
