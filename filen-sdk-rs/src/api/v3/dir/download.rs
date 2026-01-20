use std::sync::Arc;

pub use filen_types::api::v3::dir::download::{ENDPOINT, Request, Response};

use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post_large<F>(
	client: &impl AuthorizedClient,
	request: &Request,
	callback: Option<Arc<F>>,
) -> Result<Response<'static>, Error>
where
	F: Fn(u64, Option<u64>) + Send + Sync + 'static,
{
	client
		.post_large_response_auth(ENDPOINT.into(), request, callback)
		.await
}
