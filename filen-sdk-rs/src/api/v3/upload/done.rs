pub use filen_types::api::v3::upload::done::{Request, Response};
use filen_types::{api::response::FilenResponse, error::ResponseError};

use crate::{auth::http::AuthorizedClient, consts::gateway_url};

pub(crate) async fn post(
	client: impl AuthorizedClient,
	request: &Request<'_>,
) -> Result<Response, ResponseError> {
	client
		.post_auth_request(gateway_url("v3/upload/done"))
		.json(request)
		.send()
		.await?
		.json::<FilenResponse<Response>>()
		.await?
		.into_data()
}
