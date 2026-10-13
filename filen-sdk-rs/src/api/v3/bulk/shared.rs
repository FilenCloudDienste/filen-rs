pub use filen_types::api::v3::bulk::shared::{ENDPOINT, Item, Request};
use filen_types::api::v3::{bulk::MAX_READ_ITEMS, item::shared};

use crate::{
	api::v3::bulk::{Body, Sent, send},
	auth::http::AuthClient,
	error::Error,
};

pub(crate) async fn post<'a>(
	client: &AuthClient,
	request: &Request<'a>,
) -> Result<Sent<'a, Item, shared::Response<'static>>, Error> {
	send(
		request.items,
		MAX_READ_ITEMS,
		Body::ItemsOnly,
		client.post_auth(ENDPOINT.into(), request),
	)
	.await
}
