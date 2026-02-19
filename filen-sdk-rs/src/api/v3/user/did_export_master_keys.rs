pub use filen_types::api::v3::user::did_export_master_keys::ENDPOINT;

use crate::{auth::http::AuthClient, error::Error};

pub(crate) async fn post(client: &AuthClient) -> Result<(), Error> {
	client.post_auth(ENDPOINT.into(), &()).await
}
