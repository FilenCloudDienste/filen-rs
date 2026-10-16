pub use filen_types::api::v3::chat::conversations::participants::add::{
	ENDPOINT, Request, Response,
};

use crate::{api::v3::bulk::batched, auth::http::AuthClient, error::Error};

pub(crate) async fn post(client: &AuthClient, request: &Request<'_>) -> Result<Response, Error> {
	client
		.post_batched(&batched::CHAT_PARTICIPANTS_ADD, ENDPOINT, request)
		.await
}
