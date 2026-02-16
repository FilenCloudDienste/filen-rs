pub use filen_types::api::v3::dir::download::link::{ENDPOINT, Request, Response};

use crate::{auth::http::UnauthorizedClient, error::Error};

pub(crate) async fn post_large<F>(
	client: &impl UnauthorizedClient,
	request: &Request<'_>,
	callback: Option<&F>,
) -> Result<Response<'static>, Error>
where
	F: Fn(u64, Option<u64>) + Send + Sync,
{
	client
		.post_large_response(ENDPOINT.into(), request, callback)
		.await
}
