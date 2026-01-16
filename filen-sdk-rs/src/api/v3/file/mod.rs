pub(crate) mod delete;
pub(crate) mod exists;
pub(crate) mod link;
pub(crate) mod metadata;
pub(crate) mod r#move;
pub(crate) mod restore;
pub(crate) mod trash;
pub(crate) mod version;
pub(crate) mod versions;

pub use filen_types::api::v3::file::{ENDPOINT, Request, Response};

use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post(
	client: &impl AuthorizedClient,
	request: &Request,
) -> Result<Response<'static>, Error> {
	client.post_auth(ENDPOINT.into(), request).await
}
