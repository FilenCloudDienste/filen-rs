pub(crate) mod color;
pub(crate) mod content;
pub(crate) mod create;
pub(crate) mod delete;
pub(crate) mod download;
pub(crate) mod exists;
pub(crate) mod link;
pub(crate) mod metadata;
pub(crate) mod r#move;
pub(crate) mod restore;
pub(crate) mod size;
pub(crate) mod trash;

pub use filen_types::api::v3::dir::{ENDPOINT, Request, Response};

use crate::{api::post_auth_request, auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post(
	client: impl AuthorizedClient,
	request: &Request,
) -> Result<Response<'static>, Error> {
	post_auth_request(client, request, ENDPOINT).await
}
