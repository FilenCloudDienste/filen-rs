pub mod create;
pub mod delete;
pub mod leave;
pub mod name;
pub mod online;
pub mod participants;
pub mod read;
pub mod unread;

pub use filen_types::api::v3::chat::conversations::{ENDPOINT, Response};

use crate::{auth::http::AuthClient, error::Error};

pub(crate) async fn get(client: &AuthClient) -> Result<Response<'static>, Error> {
	client.get_auth(ENDPOINT.into()).await
}
