pub use filen_types::api::v3::file::restore::Request;
use filen_types::{api::response::FilenResponse, error::ResponseError};

use crate::{auth::http::AuthorizedClient, consts::gateway_url};

pub(crate) async fn post(
	client: impl AuthorizedClient,
	request: &Request,
) -> Result<(), ResponseError> {
	client
		.post_auth_request(gateway_url("v3/file/restore"))
		.json(request)
		.send()
		.await?
		.json::<FilenResponse<()>>()
		.await?
		.check_status()
}
