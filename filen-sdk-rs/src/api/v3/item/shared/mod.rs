pub(crate) mod r#in;
pub(crate) mod out;
pub(crate) mod rename;

pub use filen_types::api::v3::item::shared::{ENDPOINT, Request, Response};

use crate::{api::post_auth_request, auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post(
	client: impl AuthorizedClient,
	request: &Request,
) -> Result<Response<'static>, Error> {
	post_auth_request(client, request, ENDPOINT).await
}
