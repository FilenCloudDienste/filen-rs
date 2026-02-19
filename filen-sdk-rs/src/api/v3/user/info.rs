use filen_types::api::v3::user::info::{ENDPOINT, Response};

use crate::{auth::http::AuthClient, error::Error};

pub(crate) async fn get(client: &AuthClient) -> Result<Response<'static>, Error> {
	client.get_auth(ENDPOINT.into()).await
}
