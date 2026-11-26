pub use filen_types::api::v3::shared::in_uuid::{ENDPOINT, Request, Response};

use crate::{
	auth::http::{AuthClient, ProgressCallback},
	error::Error,
};

pub(crate) async fn post_large(
	client: &AuthClient,
	request: &Request,
	callback: Option<&ProgressCallback<'_>>,
) -> Result<Response<'static>, Error> {
	client
		.post_large_response_auth(ENDPOINT.into(), request, callback)
		.await
}
