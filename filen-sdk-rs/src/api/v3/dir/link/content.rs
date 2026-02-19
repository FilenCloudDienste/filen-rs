pub use filen_types::api::v3::dir::link::content::{ENDPOINT, Request, Response};

use crate::{auth::unauth::UnauthClient, error::Error};

pub(crate) async fn post_large<F>(
	client: &UnauthClient,
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
