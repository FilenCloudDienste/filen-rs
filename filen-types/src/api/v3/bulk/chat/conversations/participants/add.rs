use serde::Serialize;

use crate::{
	api::v3::{bulk, chat::conversations::participants::add},
	crypto::rsa::RSAEncryptedString,
	fs::Uuid,
};

pub const ENDPOINT: &str = "v3/bulk/chat/conversations/participants/add";

/// Adds every item's contact to the one conversation `uuid`.
#[derive(Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub items: &'a [Item<'a>],
}

/// `/v3/chat/conversations/participants/add`'s fields without the conversation `uuid`.
#[derive(Serialize, Debug, Clone)]
pub struct Item<'a> {
	#[serde(rename = "contactUUID")]
	pub contact_uuid: Uuid,
	pub metadata: RSAEncryptedString<'a>,
}

/// An existing participant answers the same shape, `timestamp` being when it was added.
pub type Response<'a> = bulk::Response<'a, add::Response>;
