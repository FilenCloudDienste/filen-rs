pub use filen_types::api::v3::dir::download::{ENDPOINT, Request, Response};

use crate::{
	api::{post_auth_request, post_large_auth_request},
	auth::http::AuthorizedClient,
	error::Error,
	util::{MaybeSend, MaybeSendCallback},
};

pub(crate) async fn post(
	client: impl AuthorizedClient,
	request: &Request,
) -> Result<Response<'static>, Error> {
	post_auth_request(client, request, ENDPOINT).await
}

fn send_fut<F: Future + Send>(fut: F) -> impl Future<Output = F::Output> + Send {
	fut
}
pub(crate) async fn post_large(
	client: impl AuthorizedClient,
	request: &Request,
	callback: Option<MaybeSendCallback<'_, (u64, Option<u64>)>>,
) -> Result<Response<'static>, Error> {
	post_large_auth_request(client, request, ENDPOINT, callback).await
}
