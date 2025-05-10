pub use filen_types::api::v3::contacts::{ENDPOINT, Response};

use crate::api::get_auth_request;
use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) mod delete;
pub(crate) mod requests;

pub(crate) async fn get(client: impl AuthorizedClient) -> Result<Response<'static>, Error> {
	get_auth_request(client, ENDPOINT).await
}
