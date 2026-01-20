pub use filen_types::api::v3::notes::tags::{ENDPOINT, Response};

use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) mod create;
pub(crate) mod delete;
pub(crate) mod favorite;
pub(crate) mod rename;

pub(crate) async fn post(client: &impl AuthorizedClient) -> Result<Response<'static>, Error> {
	client.get_auth(ENDPOINT.into()).await
}
