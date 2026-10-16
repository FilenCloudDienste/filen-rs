pub use filen_types::api::v3::file::stable::{ENDPOINT, Request, Response};

use crate::{api::v3::bulk::batched, auth::http::AuthClient, error::Error};

pub(crate) async fn post(
	client: &AuthClient,
	request: &Request,
) -> Result<Response<'static>, Error> {
	client
		.post_batched(&batched::FILE_STABLE, ENDPOINT, request)
		.await
}
