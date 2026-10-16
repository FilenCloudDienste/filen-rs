//! The single routes whose concurrent calls the request batcher sends as one bulk request
//! ([`AuthClient::post_batched`](crate::auth::http::AuthClient::post_batched)).
//!
//! Every row must be replay-safe: sending its whole bulk body again must leave the drive as one
//! send would and answer each item as the first send did. A lost response, a 5xx the stack
//! retries, an answer whose result count does not match and a member taking over a dropped
//! sender's flush all re-send the whole body, after some of its items may have run.
//! Reads trivially are.

use filen_types::api::v3::bulk::{MAX_READ_ITEMS, file, linked, shared};

use crate::auth::http::batch::BulkRoute;

/// A read: identical items share one slot and its answer.
const fn read(endpoint: &'static str) -> BulkRoute {
	BulkRoute {
		endpoint,
		cap: MAX_READ_ITEMS,
		dedupe: true,
		tag: None,
		top: &[],
	}
}

/// `v3/item/linked`.
pub(crate) const LINKED: BulkRoute = read(linked::ENDPOINT);
/// `v3/item/shared`.
pub(crate) const SHARED: BulkRoute = read(shared::ENDPOINT);
/// `v3/file/stable`.
pub(crate) const FILE_STABLE: BulkRoute = read(file::stable::ENDPOINT);

#[cfg(test)]
mod tests {
	use filen_types::{
		api::v3::{file::stable, item},
		fs::Uuid,
	};
	use serde::Serialize;
	use serde_json::{Value, json};

	use super::*;
	use crate::auth::http::batch::BulkBody;

	const UUID_A: &str = "3f1c2b4a-5d6e-4f70-8a91-b2c3d4e5f607";

	fn uuid(s: &str) -> Uuid {
		s.parse().unwrap()
	}

	/// The bulk body `route` sends for `request` alone.
	fn body(route: &BulkRoute, request: &impl Serialize) -> Value {
		let split = route.split(request).expect("the request splits");
		serde_json::to_value(BulkBody {
			top: split.top,
			items: vec![split.item],
		})
		.unwrap()
	}

	#[test]
	fn a_linked_call_becomes_the_documented_bulk_item() {
		assert_eq!(
			body(&LINKED, &item::linked::Request { uuid: uuid(UUID_A) }),
			json!({"items": [{"uuid": UUID_A}]})
		);
	}

	#[test]
	fn a_shared_call_becomes_the_documented_bulk_item() {
		assert_eq!(
			body(&SHARED, &item::shared::Request { uuid: uuid(UUID_A) }),
			json!({"items": [{"uuid": UUID_A}]})
		);
	}

	#[test]
	fn a_file_stable_call_becomes_the_documented_bulk_item() {
		let request: stable::Request =
			serde_json::from_value(json!({"stableUUID": UUID_A})).unwrap();
		assert_eq!(
			body(&FILE_STABLE, &request),
			json!({"items": [{"stableUUID": UUID_A}]})
		);
	}
}
