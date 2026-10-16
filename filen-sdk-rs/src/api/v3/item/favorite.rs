pub use filen_types::api::v3::item::favorite::{ENDPOINT, Request, Response};

use crate::{api::v3::bulk::batched, auth::http::AuthClient, error::Error};

pub(crate) async fn post(client: &AuthClient, request: &Request) -> Result<Response, Error> {
	client
		.post_batched(&batched::FAVORITE, ENDPOINT, request)
		.await
}
