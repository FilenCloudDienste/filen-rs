use filen_types::api::v3::bulk::MAX_WRITE_ITEMS;
pub use filen_types::api::v3::bulk::share::{ENDPOINT, Item, Request};

use crate::{
	api::v3::bulk::{Body, Sent, send},
	auth::http::AuthClient,
	error::Error,
};

pub(crate) async fn post<'a>(
	client: &AuthClient,
	request: &Request<'a>,
) -> Result<Sent<'a, Item<'a>, ()>, Error> {
	send(
		request.items,
		MAX_WRITE_ITEMS,
		Body::WithTopLevelFields,
		client.post_auth(ENDPOINT.into(), request),
	)
	.await
}
