use filen_types::{api::response::FilenResponse, error::ResponseError};

use crate::{auth::http::AuthorizedClient, consts::gateway_url};

pub(crate) async fn post(client: impl AuthorizedClient) -> Result<(), ResponseError> {
	client
		.post_auth_request(gateway_url("v3/trash/empty"))
		.send()
		.await?
		.json::<FilenResponse<()>>()
		.await?
		.check_status()
}
