use filen_types::api::v3::trash::empty::ENDPOINT;

use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) async fn post(client: &impl AuthorizedClient) -> Result<(), Error> {
	client.post_auth(ENDPOINT.into(), &()).await
}
