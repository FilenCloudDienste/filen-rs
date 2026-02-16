pub use filen_types::api::v3::file::link::password::{ENDPOINT, Request, Response};

use crate::{auth::http::UnauthorizedClient, error::Error};

pub(crate) async fn post(
	client: &impl UnauthorizedClient,
	request: &Request,
) -> Result<Response<'static>, Error> {
	client.post(ENDPOINT.into(), request).await
}
