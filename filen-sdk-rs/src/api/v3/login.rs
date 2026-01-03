pub use filen_types::api::v3::login::{ENDPOINT, Request, Response};

use crate::{api::post_request, auth::http::UnauthorizedClient, error::Error};

pub(crate) async fn post(
	client: &impl UnauthorizedClient,
	request: &Request<'_>,
) -> Result<Response<'static>, Error> {
	post_request(client, request, ENDPOINT).await
}
