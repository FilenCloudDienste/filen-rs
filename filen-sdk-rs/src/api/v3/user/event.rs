pub use filen_types::api::v3::user::event::{ENDPOINT, Request};
pub use filen_types::api::v3::user::events::UserEvent;

use crate::{auth::http::AuthClient, error::Error};

pub(crate) async fn post(
	client: &AuthClient,
	request: &Request,
) -> Result<UserEvent<'static>, Error> {
	client.post_auth(ENDPOINT.into(), request).await
}
