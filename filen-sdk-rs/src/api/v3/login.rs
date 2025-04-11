pub use filen_types::api::v3::login::{Request, Response};
use filen_types::{api::response::FilenResponse, error::ResponseError};

use crate::{auth::http::UnauthorizedClient, consts::gateway_url};

pub(crate) async fn post(
	client: impl UnauthorizedClient,
	request: Request<'_>,
) -> Result<Response, ResponseError> {
	client
		.post_request_json(gateway_url("v3/login"))
		.body(request)
		.send()
		.await?
		.json::<FilenResponse<Response>>()
		.await?
		.into_data()
}
