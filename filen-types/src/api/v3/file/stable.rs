use crate::fs::StableUuid;
use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/file/stable";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	// `stableUUID`, not camelCase `stableUuid` — the server's spelling for this
	// field everywhere it appears (socket events, listings).
	#[serde(rename = "stableUUID")]
	pub stable_uuid: StableUuid,
}

/// Fetch a file by its whole-life id: unlike `uuid`, a [`StableUuid`] survives
/// content edits, so this is the only way to follow a file across the uuid
/// re-mint an edit causes.
///
/// CLIENT-SIDE CONTRACT, pending server deployment: the endpoint is not live
/// yet, so the response shape is DEFINED here as byte-identical to
/// [`v3/file`](super::Response) — the live head of the lineage, including
/// `stableUUID` — and the server will match it. Keeping it an alias (rather
/// than a copied struct) is what guarantees the two cannot drift.
pub type Response<'a> = super::Response<'a>;

#[cfg(test)]
mod tests {
	use super::*;

	const STABLE: &str = "22222222-2222-2222-2222-222222222222";

	/// The request carries the stable id under `stableUUID` — the server's spelling
	/// (verified live: `stableUuid` is rejected with `invalid_params`).
	#[test]
	fn request_serializes_stable_uuid_the_way_the_server_spells_it() {
		let request: Request =
			serde_json::from_str(&format!(r#"{{"stableUUID":"{STABLE}"}}"#)).unwrap();
		assert_eq!(request.stable_uuid.to_string(), STABLE);
		assert_eq!(
			serde_json::to_string(&request).unwrap(),
			format!(r#"{{"stableUUID":"{STABLE}"}}"#)
		);
	}

	/// The response is the single-file shape, and it MUST carry `stableUUID` — the whole point
	/// of the endpoint is resolving the current head of a lineage.
	#[test]
	fn response_round_trips_the_single_file_shape() {
		let json = format!(
			r#"{{
				"uuid":"11111111-1111-1111-1111-111111111111",
				"stableUUID":"{STABLE}",
				"region":"us-east-1",
				"bucket":"bucket-1",
				"nameEncrypted":"encrypted-name",
				"nameHashed":"hashed-name",
				"sizeEncrypted":"encrypted-size",
				"mimeEncrypted":"encrypted-mime",
				"metadata":"encrypted-meta",
				"timestamp":1700000000000,
				"size":"1024",
				"parent":"33333333-3333-3333-3333-333333333333",
				"versioned":false,
				"trash":false,
				"version":2,
				"favorited":false
			}}"#
		);
		let response: Response = serde_json::from_str(&json).unwrap();
		assert_eq!(response.stable_uuid.to_string(), STABLE);
		// A permissive_u64 field arriving as a string still parses.
		assert_eq!(response.size, 1024);

		let reparsed: Response =
			serde_json::from_str(&serde_json::to_string(&response).unwrap()).unwrap();
		assert_eq!(reparsed.uuid, response.uuid);
		assert_eq!(reparsed.stable_uuid, response.stable_uuid);
		assert_eq!(reparsed.timestamp, response.timestamp);
	}
}
