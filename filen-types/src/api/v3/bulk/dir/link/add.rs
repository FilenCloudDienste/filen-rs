use serde::Serialize;

use crate::{
	api::v3::{bulk, dir::link::PublicLinkExpiration},
	crypto::{EncryptedMetaKey, EncryptedString},
	fs::{ObjectType, Uuid},
};

pub const ENDPOINT: &str = "v3/bulk/dir/link/add";

/// Adds every item to the one link. `expiration` is evaluated once, so all root items get the
/// same expiry.
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	#[serde(rename = "linkUUID")]
	pub link_uuid: Uuid,
	pub key: EncryptedMetaKey<'a>,
	pub expiration: PublicLinkExpiration,
	pub items: &'a [Item<'a>],
}

/// `/v3/dir/link/add`'s per-item fields. There is no recursion: a folder's descendants are items
/// of their own, each with its direct parent.
#[derive(Serialize, Debug, Clone)]
pub struct Item<'a> {
	pub uuid: Uuid,
	/// `None` adds the item at the link root.
	#[serde(with = "crate::serde::uuid::base")]
	pub parent: Option<Uuid>,
	pub r#type: ObjectType,
	pub metadata: EncryptedString<'a>,
}

pub type Response<'a> = bulk::Response<'a, ()>;
