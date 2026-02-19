pub use filen_types::api::v3::chat::unread::{ENDPOINT, Response};

use crate::{auth::http::AuthClient, error::Error};

pub(crate) async fn get(client: &AuthClient) -> Result<Response, Error> {
	client.get_auth(ENDPOINT.into()).await
}
