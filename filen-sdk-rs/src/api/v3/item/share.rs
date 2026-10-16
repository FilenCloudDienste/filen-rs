pub use filen_types::api::v3::item::share::{ENDPOINT, Request};

use crate::{api::v3::bulk::batched, auth::http::AuthClient, error::Error};

pub(crate) async fn post(client: &AuthClient, request: &Request<'_>) -> Result<(), Error> {
	client
		.post_batched(&batched::SHARE, ENDPOINT, request)
		.await
}
