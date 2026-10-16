pub use filen_types::api::v3::file::delete::permanent::{ENDPOINT, Request};

use crate::{api::v3::bulk::batched, auth::http::AuthClient, error::Error};

pub(crate) async fn post(client: &AuthClient, request: &Request) -> Result<(), Error> {
	client
		.post_batched(&batched::FILE_DELETE_PERMANENT, ENDPOINT, request)
		.await
}
