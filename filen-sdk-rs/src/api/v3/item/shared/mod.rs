pub(crate) mod r#in;
pub(crate) mod out;
pub(crate) mod rename;

pub use filen_types::api::v3::item::shared::{ENDPOINT, Request, Response};

use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post(
	client: &impl AuthorizedClient,
	request: &Request,
) -> Result<Response<'static>, Error> {
	client.post_auth(ENDPOINT.into(), request).await
}
