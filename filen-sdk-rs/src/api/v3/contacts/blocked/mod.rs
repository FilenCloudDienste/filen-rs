pub use filen_types::api::v3::contacts::blocked::{ENDPOINT, Response};

use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) mod add;
pub(crate) mod delete;

pub(crate) async fn get(client: &impl AuthorizedClient) -> Result<Response<'static>, Error> {
	client.get_auth(ENDPOINT.into()).await
}
