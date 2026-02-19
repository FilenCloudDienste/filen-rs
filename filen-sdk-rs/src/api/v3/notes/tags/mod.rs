pub use filen_types::api::v3::notes::tags::{ENDPOINT, Response};

use crate::{auth::http::AuthClient, error::Error};

pub(crate) mod create;
pub(crate) mod delete;
pub(crate) mod favorite;
pub(crate) mod rename;

pub(crate) async fn post(client: &AuthClient) -> Result<Response<'static>, Error> {
	client.get_auth(ENDPOINT.into()).await
}
