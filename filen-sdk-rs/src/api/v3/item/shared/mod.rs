pub(crate) mod r#in;
pub(crate) mod out;
pub(crate) mod rename;

pub use filen_types::api::v3::item::shared::{ENDPOINT, Request, Response};

use crate::{api::v3::bulk::batched, auth::http::AuthClient, error::Error};

pub(crate) async fn post(
	client: &AuthClient,
	request: &Request,
) -> Result<Response<'static>, Error> {
	client
		.post_batched(&batched::SHARED, ENDPOINT, request)
		.await
}
