pub use filen_types::api::v3::health::ENDPOINT;

use crate::{
	auth::unauth::UnauthClient,
	error::{Error, ErrorKind},
};

/// Asks the gateway whether it is up, once (no retry): a reachability probe that reports the
/// state now (`UnauthClient::health`).
pub(crate) async fn get_once(client: &UnauthClient) -> Result<(), Error> {
	get(client, false).await
}

/// Asks the gateway whether it is up, retried as any other request: what the sync engine probes
/// the gateway with when a request to it failed and nothing else showed it answering (see
/// `sync_engine::apply::reach`), which rides out a short connect failure.
#[cfg_attr(
	not(feature = "sync-engine"),
	expect(
		dead_code,
		reason = "only the sync engine probes the gateway with retries"
	)
)]
pub(crate) async fn get_retried(client: &UnauthClient) -> Result<(), Error> {
	get(client, true).await
}

async fn get(client: &UnauthClient, retry: bool) -> Result<(), Error> {
	healthy(&client.get_body(ENDPOINT.into(), retry).await?)
}

/// Whether `body` is the gateway's answer when it is up: a bare `OK`, not the API's JSON
/// envelope.
fn healthy(body: &[u8]) -> Result<(), Error> {
	if body.trim_ascii() == b"OK" {
		return Ok(());
	}
	Err(Error::custom(
		ErrorKind::Response,
		format!(
			"the gateway's health answered {:?}, not OK",
			String::from_utf8_lossy(&body[..body.len().min(64)])
		),
	))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_ok_is_healthy() {
		healthy(b"OK").unwrap();
		healthy(b"OK\n").unwrap();
		for body in [
			&b""[..],
			b"\"OK\"",
			b"{\"status\":true}",
			b"<html>502</html>",
		] {
			let error = healthy(body).unwrap_err();
			assert_eq!(error.kind(), ErrorKind::Response, "{error}");
		}
	}
}
