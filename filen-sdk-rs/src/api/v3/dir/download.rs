pub use filen_types::api::v3::dir::download::{ENDPOINT, Request, Response};

use crate::{
	api::{post_auth_request, post_large_auth_request},
	auth::http::AuthorizedClient,
	error::Error,
};

pub(crate) async fn post(
	client: impl AuthorizedClient,
	request: &Request,
) -> Result<Response<'static>, Error> {
	post_auth_request(client, request, ENDPOINT).await
}

pub(crate) async fn post_large<F>(
	client: impl AuthorizedClient,
	request: &Request,
	callback: &mut F,
) -> Result<Response<'static>, Error>
where
	F: FnMut(u64, Option<u64>),
{
	post_large_auth_request(client, request, ENDPOINT, callback).await
}
