use filen_types::api::v3::user::base_folder::{ENDPOINT, Response};

use crate::{auth::http::AuthorizedClient, error::Error};

pub(crate) async fn get(client: &impl AuthorizedClient) -> Result<Response, Error> {
	client.get_auth(ENDPOINT.into()).await
}
