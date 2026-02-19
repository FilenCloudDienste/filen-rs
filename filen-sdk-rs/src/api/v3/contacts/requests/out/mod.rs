pub use filen_types::api::v3::contacts::requests::out::{ENDPOINT, Response};

use crate::{auth::http::AuthClient, error::Error};

pub(crate) mod delete;

pub(crate) async fn get(client: &AuthClient) -> Result<Response<'static>, Error> {
	client.get_auth(ENDPOINT.into()).await
}
