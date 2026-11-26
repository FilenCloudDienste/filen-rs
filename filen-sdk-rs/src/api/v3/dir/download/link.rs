pub use filen_types::api::v3::dir::download::link::{ENDPOINT, Request, Response};

use crate::{
	auth::{http::ProgressCallback, unauth::UnauthClient},
	error::Error,
};

pub(crate) async fn post_large(
	client: &UnauthClient,
	request: &Request<'_>,
	callback: Option<&ProgressCallback<'_>>,
) -> Result<Response<'static>, Error> {
	client
		.post_large_response(ENDPOINT.into(), request, callback)
		.await
}
