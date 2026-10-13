pub use filen_types::api::v3::bulk::dir::create::{ENDPOINT, Item, Request};
use filen_types::api::v3::{bulk::MAX_WRITE_ITEMS, dir::create};

use crate::{
	api::v3::bulk::{Body, Sent, send},
	auth::http::AuthClient,
	error::Error,
};

pub(crate) async fn post<'a>(
	client: &AuthClient,
	request: &Request<'a>,
) -> Result<Sent<'a, Item<'a>, create::Response>, Error> {
	send(
		request.items,
		MAX_WRITE_ITEMS,
		Body::ItemsOnly,
		client.post_auth(ENDPOINT.into(), request),
	)
	.await
}
