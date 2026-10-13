pub use filen_types::api::v3::bulk::linked::{ENDPOINT, Item, Request};
use filen_types::api::v3::{bulk::MAX_READ_ITEMS, item::linked};

use crate::{
	api::v3::bulk::{Body, Sent, send},
	auth::http::AuthClient,
	error::Error,
};

pub(crate) async fn post<'a>(
	client: &AuthClient,
	request: &Request<'a>,
) -> Result<Sent<'a, Item, linked::Response<'static>>, Error> {
	send(
		request.items,
		MAX_READ_ITEMS,
		Body::ItemsOnly,
		client.post_auth(ENDPOINT.into(), request),
	)
	.await
}
