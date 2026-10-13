use std::borrow::Cow;

use rsa::{BigUint, RsaPublicKey};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::*;
use crate::{
	api::v3::{dir as single_dir, dir::link::PublicLinkExpiration, file, item},
	crypto::{EncryptedMetaKey, EncryptedString, rsa::RSAEncryptedString},
	error::ResponseError,
	fs::{ObjectType, Uuid},
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

fn enc(s: &str) -> EncryptedString<'_> {
	EncryptedString(Cow::Borrowed(s))
}

fn rsa_enc(s: &str) -> RSAEncryptedString<'_> {
	RSAEncryptedString(Cow::Borrowed(s))
}

/// Codes of the per-item envelopes of an answer whose items carry no data.
fn codes(answer: Value) -> Vec<(Option<bool>, String)> {
	results::<()>(answer)
		.into_iter()
		.map(|result| (result.status, result.code.unwrap().into_owned()))
		.collect()
}

#[test]
fn dir_color_request_matches_the_documented_body() {
	let items = [dir::color::Item {
		uuid: uuid(UUID_A),
		color: single_dir::color::DirColor::Blue,
	}];
	assert_eq!(
		to_json(&dir::color::Request { items: &items }),
		json!({"items": [{"uuid": UUID_A, "color": "blue"}]})
	);
	assert_eq!(
		codes(processed(json!([
			{"status": true, "code": "folder_color_changed", "message": "Folder color changed."},
			{"status": false, "code": "folder_not_found", "message": "Folder not found."},
		]))),
		[
			(Some(true), "folder_color_changed".to_owned()),
			(Some(false), "folder_not_found".to_owned()),
		]
	);
}

#[test]
fn favorite_request_and_answer_carry_the_value_as_a_number() {
	let items = [favorite::Item {
		uuid: uuid(UUID_A),
		r#type: ObjectType::File,
		value: true,
	}];
	assert_eq!(
		to_json(&favorite::Request { items: &items }),
		json!({"items": [{"uuid": UUID_A, "type": "file", "value": 1}]})
	);

	let results = results::<item::favorite::Response>(processed(json!([
		{
			"status": true,
			"code": "file_favorite_status_changed",
			"message": "File favorite status changed.",
			"data": {"uuid": UUID_A, "type": "file", "value": 1},
		},
		{"status": false, "code": "folder_in_trash", "message": "This folder is in the trash."},
	])));
	let [changed, in_trash] = <[_; 2]>::try_from(results).unwrap();
	let changed = changed.into_data().unwrap();
	assert_eq!(changed.uuid, uuid(UUID_A));
	assert_eq!(changed.r#type, ObjectType::File);
	assert!(changed.value);
	assert_eq!(
		api_error_code(in_trash.into_data().unwrap_err()).as_deref(),
		Some("folder_in_trash")
	);
}

#[test]
fn trash_request_and_answers_match_the_documented_example() {
	let items = [
		FileOrDir::Dir(single_dir::trash::Request { uuid: uuid(UUID_A) }),
		FileOrDir::File(file::trash::Request { uuid: uuid(UUID_B) }),
	];
	assert_eq!(
		to_json(&trash::Request { items: &items }),
		json!({"items": [{"type": "dir", "uuid": UUID_A}, {"type": "file", "uuid": UUID_B}]})
	);
	assert_eq!(
		codes(processed(json!([
			{"status": true, "message": "Folder moved to trash.", "code": "folder_trashed"},
			{"status": true, "message": "File moved to trash.", "code": "file_trashed"},
			{
				"status": true,
				"message": "Cannot delete the default folder.",
				"code": "cannot_delete_default_folder",
			},
		]))),
		[
			(Some(true), "folder_trashed".to_owned()),
			(Some(true), "file_trashed".to_owned()),
			(Some(true), "cannot_delete_default_folder".to_owned()),
		]
	);
}

#[test]
fn delete_permanent_request_matches_the_documented_example() {
	let items = [FileOrDir::File(file::delete::permanent::Request {
		uuid: uuid(UUID_A),
	})];
	assert_eq!(
		to_json(&delete::permanent::Request { items: &items }),
		json!({"items": [{"type": "file", "uuid": UUID_A}]})
	);
	assert_eq!(
		codes(processed(json!([{
			"status": true,
			"message": "File deleted permanently.",
			"code": "file_deleted_permanent",
		}]))),
		[(Some(true), "file_deleted_permanent".to_owned())]
	);
}

#[test]
fn restore_request_and_not_in_trash_answer_match_the_documented_example() {
	let items = [
		FileOrDir::File(file::restore::Request { uuid: uuid(UUID_A) }),
		FileOrDir::Dir(single_dir::restore::Request { uuid: uuid(UUID_B) }),
	];
	assert_eq!(
		to_json(&restore::Request { items: &items }),
		json!({"items": [{"type": "file", "uuid": UUID_A}, {"type": "dir", "uuid": UUID_B}]})
	);
	assert_eq!(
		codes(processed(json!([{
			"status": false,
			"message": "File is not in the trash.",
			"code": "file_not_in_trash",
		}]))),
		[(Some(false), "file_not_in_trash".to_owned())]
	);
}

#[test]
fn rename_sends_metadata_on_file_items_only() {
	let items = [
		FileOrDir::File(file::metadata::Request {
			uuid: uuid(UUID_A),
			name: enc("enc-name"),
			name_hashed: Cow::Borrowed("hash-a"),
			metadata: enc("enc-metadata"),
		}),
		FileOrDir::Dir(single_dir::metadata::Request {
			uuid: uuid(UUID_B),
			name_hashed: Cow::Borrowed("hash-b"),
			metadata: enc("enc-dir-name"),
		}),
	];
	assert_eq!(
		to_json(&rename::Request { items: &items }),
		json!({"items": [
			{
				"type": "file",
				"uuid": UUID_A,
				"name": "enc-name",
				"nameHashed": "hash-a",
				"metadata": "enc-metadata",
			},
			{"type": "dir", "uuid": UUID_B, "name": "enc-dir-name", "nameHashed": "hash-b"},
		]})
	);
	assert_eq!(
		codes(processed(json!([
			{"status": true, "message": "File renamed.", "code": "file_renamed"},
			{"status": true, "message": "File not found.", "code": "file_not_found"},
			{"status": true, "message": "Folder renamed.", "code": "folder_renamed"},
		]))),
		[
			(Some(true), "file_renamed".to_owned()),
			(Some(true), "file_not_found".to_owned()),
			(Some(true), "folder_renamed".to_owned()),
		]
	);
}

#[test]
fn metadata_request_matches_the_documented_example() {
	let items = [FileOrDir::Dir(single_dir::metadata::Request {
		uuid: uuid(UUID_A),
		name_hashed: Cow::Borrowed("hash"),
		metadata: enc("enc"),
	})];
	assert_eq!(
		to_json(&metadata::Request { items: &items }),
		json!({"items": [{"type": "dir", "uuid": UUID_A, "name": "enc", "nameHashed": "hash"}]})
	);
	assert_eq!(
		codes(processed(json!([{
			"status": true,
			"message": "Folder metadata changed.",
			"code": "folder_metadata_changed",
		}]))),
		[(Some(true), "folder_metadata_changed".to_owned())]
	);
}

#[test]
fn move_request_and_answers_match_the_documented_example() {
	let items = [
		FileOrDir::File(file::r#move::Request {
			uuid: uuid(UUID_A),
			new_parent: uuid(UUID_B),
		}),
		FileOrDir::Dir(single_dir::r#move::Request {
			uuid: uuid(UUID_B),
			to: uuid(UUID_B),
		}),
	];
	assert_eq!(
		to_json(&r#move::Request { items: &items }),
		json!({"items": [
			{"type": "file", "uuid": UUID_A, "to": UUID_B},
			{"type": "dir", "uuid": UUID_B, "to": UUID_B},
		]})
	);
	assert_eq!(
		codes(processed(json!([
			{"status": true, "code": "file_moved", "message": "File moved."},
			{
				"status": false,
				"code": "cannot_move_folder_into_itself",
				"message": "Cannot move a folder into itself.",
			},
			{"status": false, "code": "rate_limited", "message": "Rate limited."},
		]))),
		[
			(Some(true), "file_moved".to_owned()),
			(Some(false), "cannot_move_folder_into_itself".to_owned()),
			(Some(false), "rate_limited".to_owned()),
		]
	);
}

#[test]
fn dir_create_answers_the_remapped_uuid_of_the_documented_example() {
	const ROOT: &str = "00000000-0000-4000-8000-000000000001";
	const X: &str = "00000000-0000-4000-8000-000000000002";
	let items = [
		dir::create::Item {
			uuid: uuid(UUID_A),
			meta: enc("enc-a"),
			name_hashed: Cow::Borrowed("hash-a"),
			parent: uuid(ROOT),
		},
		dir::create::Item {
			uuid: uuid(UUID_B),
			meta: enc("enc-b"),
			name_hashed: Cow::Borrowed("hash-b"),
			parent: uuid(UUID_A),
		},
	];
	assert_eq!(
		to_json(&dir::create::Request { items: &items }),
		json!({"items": [
			{"uuid": UUID_A, "name": "enc-a", "nameHashed": "hash-a", "parent": ROOT},
			{"uuid": UUID_B, "name": "enc-b", "nameHashed": "hash-b", "parent": UUID_A},
		]})
	);

	let results = results::<single_dir::create::Response>(processed(json!([
		{
			"status": true,
			"message": "Folder created.",
			"code": "folder_created",
			"data": {"uuid": X, "timestamp": 1_700_000_000},
		},
		{
			"status": true,
			"message": "Folder created.",
			"code": "folder_created",
			"data": {"uuid": UUID_B, "timestamp": 1_760_270_400},
		},
		{"status": false, "message": "Maximum storage reached.", "code": "max_storage_reached"},
	])));
	let [existing, created, no_storage] = <[_; 3]>::try_from(results).unwrap();

	let existing = existing.into_data().unwrap();
	assert_eq!(existing.uuid, uuid(X));
	assert_eq!(existing.timestamp.timestamp(), 1_700_000_000);
	let created = created.into_data().unwrap();
	assert_eq!(created.uuid, uuid(UUID_B));
	assert_eq!(created.timestamp.timestamp(), 1_760_270_400);
	assert_eq!(
		api_error_code(no_storage.into_data().unwrap_err()).as_deref(),
		Some("max_storage_reached")
	);
}

#[test]
fn share_request_matches_the_documented_body() {
	let items = [
		share::Item {
			uuid: uuid(UUID_A),
			parent: None,
			r#type: ObjectType::Dir,
			metadata: rsa_enc("enc-a"),
		},
		share::Item {
			uuid: uuid(UUID_B),
			parent: Some(uuid(UUID_A)),
			r#type: ObjectType::File,
			metadata: rsa_enc("enc-b"),
		},
	];
	let request = share::Request {
		email: Cow::Borrowed("friend@example.com"),
		items: &items,
	};
	assert_eq!(
		to_json(&request),
		json!({
			"email": "friend@example.com",
			"items": [
				{"uuid": UUID_A, "parent": "none", "type": "folder", "metadata": "enc-a"},
				{"uuid": UUID_B, "parent": UUID_A, "type": "file", "metadata": "enc-b"},
			],
		})
	);
	assert_eq!(
		codes(processed(json!([
			{"status": true, "message": "Item shared.", "code": "item_shared"},
			{"status": false, "message": "File not found.", "code": "file_not_found"},
		]))),
		[
			(Some(true), "item_shared".to_owned()),
			(Some(false), "file_not_found".to_owned()),
		]
	);
}

#[test]
fn dir_link_add_request_matches_the_documented_body() {
	let items = [
		dir::link::add::Item {
			uuid: uuid(UUID_A),
			parent: None,
			r#type: ObjectType::Dir,
			metadata: enc("enc"),
		},
		dir::link::add::Item {
			uuid: uuid(UUID_B),
			parent: Some(uuid(UUID_A)),
			r#type: ObjectType::File,
			metadata: enc("enc-b"),
		},
	];
	let request = dir::link::add::Request {
		link_uuid: uuid(UUID_B),
		key: EncryptedMetaKey(enc("enc-key")),
		expiration: PublicLinkExpiration::Never,
		items: &items,
	};
	assert_eq!(
		to_json(&request),
		json!({
			"linkUUID": UUID_B,
			"key": "enc-key",
			"expiration": "never",
			"items": [
				{"uuid": UUID_A, "parent": "base", "type": "folder", "metadata": "enc"},
				{"uuid": UUID_B, "parent": UUID_A, "type": "file", "metadata": "enc-b"},
			],
		})
	);
	assert_eq!(
		codes(processed(json!([{
			"status": true,
			"message": "Item added to folder link.",
			"code": "item_added_to_link",
		}]))),
		[(Some(true), "item_added_to_link".to_owned())]
	);
}

#[test]
fn shared_rename_request_matches_the_documented_body() {
	let items = [shared::rename::Item {
		uuid: uuid(UUID_A),
		receiver_id: 123,
		metadata: rsa_enc("enc"),
	}];
	assert_eq!(
		to_json(&shared::rename::Request { items: &items }),
		json!({"items": [{"uuid": UUID_A, "receiverId": 123, "metadata": "enc"}]})
	);
	assert_eq!(
		codes(processed(json!([
			{"status": true, "message": "Item renamed.", "code": "item_renamed"},
			{"status": false, "message": "Item not found.", "code": "item_not_found"},
		]))),
		[
			(Some(true), "item_renamed".to_owned()),
			(Some(false), "item_not_found".to_owned()),
		]
	);
}

#[test]
fn linked_rename_request_matches_the_documented_body() {
	let items = [linked::rename::Item {
		uuid: uuid(UUID_A),
		link_uuid: uuid(UUID_B),
		metadata: enc("enc"),
	}];
	assert_eq!(
		to_json(&linked::rename::Request { items: &items }),
		json!({"items": [{"uuid": UUID_A, "linkUUID": UUID_B, "metadata": "enc"}]})
	);
}
