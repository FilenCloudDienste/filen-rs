pub use filen_types::api::v3::contacts::{ENDPOINT, Response};

use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) mod blocked;
pub(crate) mod delete;
pub(crate) mod requests;

pub(crate) async fn get(client: &impl AuthorizedClient) -> Result<Response<'static>, Error> {
	client.get_auth(ENDPOINT.into()).await
}
