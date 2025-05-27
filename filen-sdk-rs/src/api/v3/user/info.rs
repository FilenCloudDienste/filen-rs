use filen_types::api::v3::user::info::{ENDPOINT, Response};

use crate::{api::get_auth_request, auth::http::AuthorizedClient, error::Error};

pub(crate) async fn get(client: impl AuthorizedClient) -> Result<Response<'static>, Error> {
	get_auth_request(client, ENDPOINT).await
}
