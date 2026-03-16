pub use filen_types::api::v3::register::{ENDPOINT, Request, Response};

use crate::{auth::unauth::UnauthClient, error::Error};

pub(crate) async fn post(client: &UnauthClient, request: &Request<'_>) -> Result<(), Error> {
	client
		.post::<_, Response>(ENDPOINT.into(), request)
		.await
		.map(|_| ())
}
