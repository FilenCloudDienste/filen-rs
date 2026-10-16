//! The single routes whose concurrent calls the request batcher sends as one bulk request
//! ([`AuthClient::post_batched`](crate::auth::http::AuthClient::post_batched)).
//!
//! Every row must be replay-safe: sending its whole bulk body again must leave the drive as one
//! send would and answer each item as the first send did. A lost response, a 5xx the stack
//! retries, an answer whose result count does not match and a member taking over a dropped
//! sender's flush all re-send the whole body, after some of its items may have run.
//! Reads trivially are. The writes here answer a replayed item with the success code it first
//! got: `trash`, file `delete/permanent`, `share` and `dir/link/add` as a no-op; `dir/color`,
//! `favorite` and dir `delete/permanent` by applying it again, so their events go out again (for
//! a dir delete only until the server has purged the folder, then as a no-op); `dir/create` with
//! the existing folder's uuid; chat `participants/add` with the existing participant. One
//! exception: a `dir/create` replayed after its folder was trashed, renamed or moved creates the
//! same uuid afresh and answers `internal_error`; that call is then made on its own, and the
//! single route answers it the same way.
//!
//! Left out because a replay answers differently: `restore` (`file_not_in_trash` /
//! `folder_not_in_trash`) and `notes/delete` (`note_not_found`).
//!
//! Not opted in yet, for want of concurrent callers, of a single wrapper or of a per-kind tag:
//! `exists`, `present`, `rename`, `metadata`, `move`, `shared/rename`, `linked/rename`, the other
//! `notes/*` routes and `chat/conversations/read`.

use filen_types::api::v3::bulk::{
	MAX_READ_ITEMS, MAX_WRITE_ITEMS, chat, delete, dir, favorite, file, linked, share, shared,
	trash,
};

use crate::auth::http::batch::BulkRoute;

/// The most items one batched write carries: one of the server's lock groups, which hold the
/// user's other drive writes while they run (it runs write items in groups of up to 50).
const AUTO_WRITE_CAP: usize = 50;
const _: () = assert!(AUTO_WRITE_CAP <= MAX_WRITE_ITEMS);

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

/// A write: every call is its own item, since the server runs a duplicate item twice.
const fn write(
	endpoint: &'static str,
	tag: Option<&'static str>,
	top: &'static [&'static str],
) -> BulkRoute {
	BulkRoute {
		endpoint,
		cap: AUTO_WRITE_CAP,
		dedupe: false,
		tag,
		top,
	}
}

/// `v3/item/linked`.
pub(crate) const LINKED: BulkRoute = read(linked::ENDPOINT);
/// `v3/item/shared`.
pub(crate) const SHARED: BulkRoute = read(shared::ENDPOINT);
/// `v3/file/stable`.
pub(crate) const FILE_STABLE: BulkRoute = read(file::stable::ENDPOINT);
/// `v3/dir/create`.
pub(crate) const DIR_CREATE: BulkRoute = write(dir::create::ENDPOINT, None, &[]);
/// `v3/dir/color`.
pub(crate) const DIR_COLOR: BulkRoute = write(dir::color::ENDPOINT, None, &[]);
/// `v3/item/favorite`; its item carries its own `type`.
pub(crate) const FAVORITE: BulkRoute = write(favorite::ENDPOINT, None, &[]);
/// `v3/file/trash`.
pub(crate) const FILE_TRASH: BulkRoute = write(trash::ENDPOINT, Some("file"), &[]);
/// `v3/dir/trash`.
pub(crate) const DIR_TRASH: BulkRoute = write(trash::ENDPOINT, Some("dir"), &[]);
/// `v3/file/delete/permanent`.
pub(crate) const FILE_DELETE_PERMANENT: BulkRoute =
	write(delete::permanent::ENDPOINT, Some("file"), &[]);
/// `v3/dir/delete/permanent`.
pub(crate) const DIR_DELETE_PERMANENT: BulkRoute =
	write(delete::permanent::ENDPOINT, Some("dir"), &[]);
/// `v3/item/share`: calls share one window per receiver.
pub(crate) const SHARE: BulkRoute = write(share::ENDPOINT, None, &["email"]);
/// `v3/dir/link/add`: calls share one window per link.
pub(crate) const DIR_LINK_ADD: BulkRoute = write(
	dir::link::add::ENDPOINT,
	None,
	&["linkUUID", "key", "expiration"],
);
/// `v3/chat/conversations/participants/add`: calls share one window per conversation.
pub(crate) const CHAT_PARTICIPANTS_ADD: BulkRoute = write(
	chat::conversations::participants::add::ENDPOINT,
	None,
	&["uuid"],
);

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use filen_types::{
		api::v3::{
			chat::conversations::participants,
			dir::{self as single_dir, color::DirColor, link::PublicLinkExpiration},
			file::{self as single_file, stable},
			item,
		},
		crypto::{EncryptedMetaKey, EncryptedString, rsa::RSAEncryptedString},
		fs::{ObjectType, Uuid},
	};
	use serde::Serialize;
	use serde_json::{Value, json};

	use super::*;
	use crate::auth::http::batch::{BulkBody, Split};

	const UUID_A: &str = "3f1c2b4a-5d6e-4f70-8a91-b2c3d4e5f607";
	const UUID_B: &str = "c2d3e4f5-a6b7-4c8d-9e0f-1a2b3c4d5e6f";

	fn uuid(s: &str) -> Uuid {
		s.parse().unwrap()
	}

	fn enc(s: &str) -> EncryptedString<'_> {
		EncryptedString(Cow::Borrowed(s))
	}

	fn rsa_enc(s: &str) -> RSAEncryptedString<'_> {
		RSAEncryptedString(Cow::Borrowed(s))
	}

	fn split(route: &BulkRoute, request: &impl Serialize) -> Split {
		route.split(request).expect("the request splits")
	}

	/// The bulk body that carries `splits` together, after checking that they share their
	/// top-level fields and so one window.
	fn bulk_body(splits: impl IntoIterator<Item = Split>) -> Value {
		let mut splits = splits.into_iter();
		let first = splits.next().expect("at least one split");
		let mut items = vec![first.item];
		for split in splits {
			assert_eq!(split.top, first.top);
			items.push(split.item);
		}
		serde_json::to_value(BulkBody {
			top: first.top,
			items,
		})
		.unwrap()
	}

	/// The bulk body `route` sends for `request` alone.
	fn body(route: &BulkRoute, request: &impl Serialize) -> Value {
		bulk_body([split(route, request)])
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

	#[test]
	fn dir_create_calls_become_the_documented_bulk_items() {
		const ROOT: &str = "00000000-0000-4000-8000-000000000001";
		let a = single_dir::create::Request {
			uuid: uuid(UUID_A),
			meta: enc("enc-a"),
			name_hashed: Cow::Borrowed("hash-a"),
			parent: uuid(ROOT),
		};
		let b = single_dir::create::Request {
			uuid: uuid(UUID_B),
			meta: enc("enc-b"),
			name_hashed: Cow::Borrowed("hash-b"),
			parent: uuid(UUID_A),
		};
		assert_eq!(
			bulk_body([split(&DIR_CREATE, &a), split(&DIR_CREATE, &b)]),
			json!({"items": [
				{"uuid": UUID_A, "name": "enc-a", "nameHashed": "hash-a", "parent": ROOT},
				{"uuid": UUID_B, "name": "enc-b", "nameHashed": "hash-b", "parent": UUID_A},
			]})
		);
	}

	#[test]
	fn a_dir_color_call_becomes_the_documented_bulk_item() {
		let request = single_dir::color::Request {
			uuid: uuid(UUID_A),
			color: DirColor::Blue,
		};
		assert_eq!(
			body(&DIR_COLOR, &request),
			json!({"items": [{"uuid": UUID_A, "color": "blue"}]})
		);
	}

	#[test]
	fn a_favorite_call_keeps_its_type_and_sends_the_value_as_a_number() {
		let request = item::favorite::Request {
			uuid: uuid(UUID_A),
			r#type: ObjectType::File,
			value: true,
		};
		assert_eq!(
			body(&FAVORITE, &request),
			json!({"items": [{"uuid": UUID_A, "type": "file", "value": 1}]})
		);
	}

	#[test]
	fn dir_and_file_trash_calls_share_one_body_tagged_by_kind() {
		let dir = single_dir::trash::Request { uuid: uuid(UUID_A) };
		let file = single_file::trash::Request { uuid: uuid(UUID_B) };
		assert_eq!(DIR_TRASH.endpoint, FILE_TRASH.endpoint);
		assert_eq!(
			bulk_body([split(&DIR_TRASH, &dir), split(&FILE_TRASH, &file)]),
			json!({"items": [{"type": "dir", "uuid": UUID_A}, {"type": "file", "uuid": UUID_B}]})
		);
	}

	#[test]
	fn file_and_dir_delete_permanent_calls_share_one_body_tagged_by_kind() {
		let file = single_file::delete::permanent::Request { uuid: uuid(UUID_A) };
		let dir = single_dir::delete::permanent::Request { uuid: uuid(UUID_B) };
		assert_eq!(
			FILE_DELETE_PERMANENT.endpoint,
			DIR_DELETE_PERMANENT.endpoint
		);
		assert_eq!(
			body(&FILE_DELETE_PERMANENT, &file),
			json!({"items": [{"type": "file", "uuid": UUID_A}]})
		);
		assert_eq!(
			bulk_body([
				split(&FILE_DELETE_PERMANENT, &file),
				split(&DIR_DELETE_PERMANENT, &dir)
			]),
			json!({"items": [{"type": "file", "uuid": UUID_A}, {"type": "dir", "uuid": UUID_B}]})
		);
	}

	#[test]
	fn share_calls_to_one_receiver_carry_the_email_once() {
		let a = item::share::Request {
			uuid: uuid(UUID_A),
			parent: None,
			email: Cow::Borrowed("friend@example.com"),
			r#type: ObjectType::Dir,
			metadata: rsa_enc("enc-a"),
		};
		let b = item::share::Request {
			uuid: uuid(UUID_B),
			parent: Some(uuid(UUID_A)),
			email: Cow::Borrowed("friend@example.com"),
			r#type: ObjectType::File,
			metadata: rsa_enc("enc-b"),
		};
		assert_eq!(
			bulk_body([split(&SHARE, &a), split(&SHARE, &b)]),
			json!({
				"email": "friend@example.com",
				"items": [
					{"uuid": UUID_A, "parent": "none", "type": "folder", "metadata": "enc-a"},
					{"uuid": UUID_B, "parent": UUID_A, "type": "file", "metadata": "enc-b"},
				],
			})
		);
	}

	#[test]
	fn dir_link_add_calls_to_one_link_carry_the_link_once() {
		let request =
			|item: &str, parent: Option<Uuid>, r#type, metadata| single_dir::link::add::Request {
				uuid: uuid(item),
				parent,
				link_uuid: uuid(UUID_B),
				r#type,
				metadata: enc(metadata),
				key: EncryptedMetaKey(enc("enc-key")),
				expiration: PublicLinkExpiration::Never,
			};
		let a = request(UUID_A, None, ObjectType::Dir, "enc");
		let b = request(UUID_B, Some(uuid(UUID_A)), ObjectType::File, "enc-b");
		assert_eq!(
			bulk_body([split(&DIR_LINK_ADD, &a), split(&DIR_LINK_ADD, &b)]),
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
	}

	#[test]
	fn chat_participants_add_calls_to_one_conversation_carry_the_conversation_once() {
		const CONTACT: &str = "00000000-0000-4000-8000-000000000002";
		let a = participants::add::Request {
			uuid: uuid(UUID_A),
			contact_uuid: uuid(UUID_B),
			metadata: rsa_enc("enc-a"),
		};
		let b = participants::add::Request {
			uuid: uuid(UUID_A),
			contact_uuid: uuid(CONTACT),
			metadata: rsa_enc("enc-b"),
		};
		assert_eq!(
			bulk_body([
				split(&CHAT_PARTICIPANTS_ADD, &a),
				split(&CHAT_PARTICIPANTS_ADD, &b)
			]),
			json!({
				"uuid": UUID_A,
				"items": [
					{"contactUUID": UUID_B, "metadata": "enc-a"},
					{"contactUUID": CONTACT, "metadata": "enc-b"},
				],
			})
		);
	}
}
