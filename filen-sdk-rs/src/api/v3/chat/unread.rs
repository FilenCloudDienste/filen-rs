pub use filen_types::api::v3::chat::unread::{ENDPOINT, Response};

use crate::{api::get_auth_request, auth::http::AuthorizedClient, error::Error};

pub(crate) async fn get(client: impl AuthorizedClient) -> Result<Response, Error> {
	get_auth_request(client, ENDPOINT).await
}
