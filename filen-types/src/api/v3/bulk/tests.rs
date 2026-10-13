use std::borrow::Cow;

use rsa::{BigUint, RsaPublicKey};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::*;
use crate::{
	api::v3::{file, item},
	error::ResponseError,
	fs::Uuid,
	serde::rsa::RsaDerPublicKey,
};

const UUID_A: &str = "3f1c2b4a-5d6e-4f70-8a91-b2c3d4e5f607";
const UUID_B: &str = "c2d3e4f5-a6b7-4c8d-9e0f-1a2b3c4d5e6f";

fn uuid(s: &str) -> Uuid {
	s.parse().unwrap()
}

fn to_json<T: Serialize>(value: &T) -> Value {
	serde_json::to_value(value).unwrap()
}

/// Parses a whole `bulk_processed` answer and returns its per-item envelopes.
fn results<T: DeserializeOwned>(answer: Value) -> Vec<FilenResponse<'static, T>> {
	serde_json::from_value::<FilenResponse<'static, Response<'static, T>>>(answer)
		.unwrap()
		.into_data()
		.unwrap()
		.results
}

fn processed(results: Value) -> Value {
	json!({
		"status": true,
		"message": "Bulk processed.",
		"code": "bulk_processed",
		"data": { "results": results },
	})
}

fn api_error_code(error: ResponseError) -> Option<String> {
	let ResponseError::ApiError { code, .. } = error;
	code
}

#[test]
fn whole_request_errors_parse_like_single_route_errors() {
	for answer in [
		json!({"status": false, "code": "invalid_params", "message": "Invalid items."}),
		json!({"status": false, "message": "Invalid endpoint.", "code": "invalid_endpoint"}),
		json!({"status": false, "message": "API key not found.", "code": "api_key_not_found"}),
	] {
		let expected = answer["code"].as_str().map(str::to_owned);
		let response: FilenResponse<'_, exists::Response<'_>> =
			serde_json::from_value(answer).unwrap();
		assert_eq!(api_error_code(response.into_data().unwrap_err()), expected);
	}
}

#[test]
fn an_item_without_data_keeps_its_status_and_code() {
	let results = results::<()>(processed(json!([
		{"status": true, "message": "File not found.", "code": "file_not_found"},
		{"status": false, "code": "internal_error", "message": "Internal error"},
	])));
	let [not_found, internal] = <[_; 2]>::try_from(results).unwrap();

	assert_eq!(not_found.code.as_deref(), Some("file_not_found"));
	not_found.ignore_data().unwrap();
	assert_eq!(
		api_error_code(internal.ignore_data().unwrap_err()).as_deref(),
		Some("internal_error")
	);
}

#[test]
fn exists_request_matches_the_documented_body() {
	let items = [
		FileOrDir::File(exists::Item {
			name_hashed: Cow::Borrowed("0f6e1c9b8a7d6c5b4a3f2e1d0c9b8a7f"),
		}),
		FileOrDir::Dir(exists::Item {
			name_hashed: Cow::Borrowed("9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d"),
		}),
	];
	let request = exists::Request {
		parent: uuid(UUID_A),
		items: &items,
	};
	assert_eq!(
		to_json(&request),
		json!({
			"parent": UUID_A,
			"items": [
				{ "type": "file", "nameHashed": "0f6e1c9b8a7d6c5b4a3f2e1d0c9b8a7f" },
				{ "type": "dir", "nameHashed": "9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d" },
			],
		})
	);
}

#[test]
fn exists_documented_answer_parses_a_match_and_a_miss() {
	let results = results::<file::exists::Response>(processed(json!([
		{
			"status": true,
			"message": "File exists.",
			"code": "file_exists",
			"data": { "exists": true, "uuid": UUID_B },
		},
		{
			"status": true,
			"message": "Folder does not exist.",
			"code": "folder_does_not_exist",
			"data": { "exists": false, "uuid": "" },
		},
	])));
	let [found, missing] = <[_; 2]>::try_from(results).unwrap();
	assert_eq!(found.into_data().unwrap().0, Some(uuid(UUID_B)));
	assert_eq!(missing.into_data().unwrap().0, None);
}

#[test]
fn present_request_tags_each_item() {
	let items = [
		FileOrDir::File(present::Item { uuid: uuid(UUID_A) }),
		FileOrDir::Dir(present::Item { uuid: uuid(UUID_B) }),
	];
	assert_eq!(
		to_json(&present::Request { items: &items }),
		json!({"items": [{"type": "file", "uuid": UUID_A}, {"type": "dir", "uuid": UUID_B}]})
	);
}

#[test]
fn present_answers_parse_into_file_dir_and_missing() {
	let results = results::<present::Data>(processed(json!([
		{
			"status": true,
			"code": "file_exists",
			"message": "File exists.",
			"data": {
				"present": true,
				"versioned": true,
				"trash": false,
				"uuid": UUID_A,
				"stableUUID": UUID_B,
			},
		},
		{
			"status": true,
			"code": "folder_does_not_exist",
			"message": "Folder does not exist.",
			"data": {"present": false},
		},
		{
			"status": true,
			"code": "folder_exists",
			"message": "Folder exists.",
			"data": {"present": true, "trash": true},
		},
		{
			"status": true,
			"code": "file_does_not_exist",
			"message": "File does not exist.",
			"data": {"present": false},
		},
	])));
	let data = results
		.into_iter()
		.map(|result| result.into_data().unwrap())
		.collect::<Vec<_>>();

	let [file, dir_missing, dir, file_missing] = <[_; 4]>::try_from(data).unwrap();
	let present::Data::File {
		versioned,
		trash,
		uuid: file_uuid,
		stable_uuid,
	} = file
	else {
		panic!("expected a file, got {file:?}");
	};
	assert!(versioned);
	assert!(!trash);
	assert_eq!(file_uuid, uuid(UUID_A));
	assert_eq!(stable_uuid, uuid(UUID_B));
	assert!(matches!(dir_missing, present::Data::Missing));
	assert!(matches!(dir, present::Data::Dir { trash: true }));
	assert!(matches!(file_missing, present::Data::Missing));
}

#[test]
fn present_data_with_a_partial_file_shape_is_rejected() {
	let answer = processed(json!([{
		"status": true,
		"code": "file_exists",
		"message": "File exists.",
		"data": {"present": true, "trash": false, "uuid": UUID_A},
	}]));
	assert!(serde_json::from_value::<FilenResponse<'_, present::Response<'_>>>(answer).is_err());
}

#[test]
fn file_stable_request_carries_the_server_spelling() {
	let item: file::stable::Request =
		serde_json::from_value(json!({"stableUUID": UUID_A})).unwrap();
	let items = [item];
	assert_eq!(
		to_json(&super::file::stable::Request { items: &items }),
		json!({"items": [{"stableUUID": UUID_A}]})
	);
}

#[test]
fn file_stable_answers_parse_found_and_not_found() {
	let results = results::<file::stable::Response<'static>>(processed(json!([
		{
			"status": true,
			"message": "File info fetched.",
			"code": "file_info_fetched",
			"data": {
				"uuid": UUID_A,
				"stableUUID": UUID_B,
				"region": "de-1",
				"bucket": "filen-1",
				"nameEncrypted": "enc-name",
				"nameHashed": "hashed-name",
				"sizeEncrypted": "enc-size",
				"mimeEncrypted": "enc-mime",
				"metadata": "enc-meta",
				"size": 1024,
				"parent": UUID_B,
				"versioned": false,
				"trash": false,
				"favorited": true,
				"version": 2,
				"timestamp": 1_760_270_400_000_u64,
				"chunks": 1,
			},
		},
		{"status": false, "message": "File not found.", "code": "file_not_found"},
	])));
	let [found, not_found] = <[_; 2]>::try_from(results).unwrap();

	let found = found.into_data().unwrap();
	assert_eq!(found.uuid, uuid(UUID_A));
	assert_eq!(found.stable_uuid, uuid(UUID_B));
	assert_eq!(found.size, 1024);
	assert!(found.favorited);
	assert_eq!(
		api_error_code(not_found.into_data().unwrap_err()).as_deref(),
		Some("file_not_found")
	);
}

#[test]
fn linked_request_and_answers_match_the_documented_shapes() {
	let items = [item::linked::Request { uuid: uuid(UUID_A) }];
	assert_eq!(
		to_json(&linked::Request { items: &items }),
		json!({"items": [{"uuid": UUID_A}]})
	);

	let results = results::<item::linked::Response<'static>>(processed(json!([
		{
			"status": true,
			"message": "Item link status fetched.",
			"code": "item_link_status_fetched",
			"data": {"link": false, "links": []},
		},
		{
			"status": true,
			"message": "Item link status fetched.",
			"code": "item_link_status_fetched",
			"data": {"link": true, "links": [{"linkUUID": UUID_B, "linkKey": "enc-key"}]},
		},
	])));
	let [unlinked, linked] = <[_; 2]>::try_from(results).unwrap();

	let unlinked = unlinked.into_data().unwrap();
	assert!(!unlinked.link);
	assert!(unlinked.links.is_empty());
	let linked = linked.into_data().unwrap();
	assert!(linked.link);
	assert_eq!(linked.links.len(), 1);
	assert_eq!(linked.links[0].link_uuid, uuid(UUID_B));
}

#[test]
fn shared_request_and_answers_match_the_documented_shapes() {
	let items = [item::shared::Request { uuid: uuid(UUID_A) }];
	assert_eq!(
		to_json(&shared::Request { items: &items }),
		json!({"items": [{"uuid": UUID_A}]})
	);

	let key = RsaPublicKey::new(
		BigUint::from_bytes_be(&[0xc5; 256]),
		BigUint::from(65_537_u32),
	)
	.unwrap();
	let public_key = RsaDerPublicKey(Cow::Borrowed(&key)).to_string();
	let results = results::<item::shared::Response<'static>>(processed(json!([
		{
			"status": true,
			"message": "Item shared status fetched.",
			"code": "item_shared_status_fetched",
			"data": {
				"sharing": true,
				"users": [{"id": 123, "email": "a@example.com", "publicKey": public_key}],
			},
		},
		{
			"status": true,
			"message": "Item shared status fetched.",
			"code": "item_shared_status_fetched",
			"data": {"sharing": false, "users": []},
		},
	])));
	let [shared, unshared] = <[_; 2]>::try_from(results).unwrap();

	let shared = shared.into_data().unwrap();
	assert!(shared.sharing);
	assert_eq!(shared.users.len(), 1);
	assert_eq!(shared.users[0].id, 123);
	assert_eq!(shared.users[0].email, "a@example.com");
	assert_eq!(shared.users[0].public_key, key);
	let unshared = unshared.into_data().unwrap();
	assert!(!unshared.sharing);
	assert!(unshared.users.is_empty());
}
