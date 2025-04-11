pub use filen_types::api::v3::dir::content::{Request, Response};
use filen_types::{api::response::FilenResponse, error::ResponseError};

use crate::{auth::http::AuthorizedClient, consts::gateway_url};

pub(crate) async fn post(
	client: impl AuthorizedClient,
	request: Request,
) -> Result<Response, ResponseError> {
	client
		.post_auth_request_json(gateway_url("v3/dir/content"))
		.body(request)
		.send()
		.await?
		.json::<FilenResponse<Response>>()
		.await?
		.into_data()
}
