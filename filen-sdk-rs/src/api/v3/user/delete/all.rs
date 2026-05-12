pub use filen_types::api::v3::user::delete::all::ENDPOINT;

use crate::{auth::http::AuthClient, error::Error};

pub(crate) async fn post(client: &AuthClient) -> Result<(), Error> {
	client.post_auth(ENDPOINT.into(), &()).await
}
