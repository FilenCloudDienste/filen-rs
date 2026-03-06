pub use filen_types::api::v3::shared::in_root::{ENDPOINT, Request, Response};

use crate::{auth::http::AuthClient, error::Error};

pub(crate) async fn post_large<F>(
	client: &AuthClient,
	callback: Option<&F>,
) -> Result<Response<'static>, Error>
where
	F: Fn(u64, Option<u64>) + Send + Sync,
{
	client
		.post_large_response_auth(ENDPOINT.into(), &Request, callback)
		.await
}
