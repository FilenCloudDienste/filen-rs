use std::borrow::Cow;

use serde::Serialize;

use crate::{
	api::v3::bulk,
	crypto::rsa::RSAEncryptedString,
	fs::{ObjectType, Uuid},
};

pub const ENDPOINT: &str = "v3/bulk/share";

/// Shares every item with the one receiver `email`.
#[derive(Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub email: Cow<'a, str>,
	pub items: &'a [Item<'a>],
}

/// `/v3/item/share`'s fields without `email`. There is no recursion: a folder's descendants are
/// items of their own, each with its direct parent.
#[derive(Serialize, Debug, Clone)]
pub struct Item<'a> {
	pub uuid: Uuid,
	/// `None` shares the item at the top level.
	#[serde(with = "crate::serde::uuid::none")]
	pub parent: Option<Uuid>,
	pub r#type: ObjectType,
	pub metadata: RSAEncryptedString<'a>,
}

pub type Response<'a> = bulk::Response<'a, ()>;
