pub use filen_types::api::v3::health::{ENDPOINT, Response};

use crate::{auth::http::AuthClient, error::Error};

/// Ask the gateway whether it is up: what the sync engine probes it with when a request to it
/// failed and nothing else showed it answering (see `sync_engine::apply::reach`).
#[cfg_attr(
	not(feature = "sync-engine"),
	expect(dead_code, reason = "only the sync engine probes the gateway")
)]
pub(crate) async fn post(client: &AuthClient) -> Result<Response, Error> {
	client.get_auth(ENDPOINT.into()).await
}
