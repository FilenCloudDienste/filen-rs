use serde::Serialize;

use crate::{
	api::v3::{
		bulk,
		notes::participants::add::{self, ContactUuid},
	},
	crypto::rsa::RSAEncryptedString,
	fs::Uuid,
};

pub const ENDPOINT: &str = "v3/bulk/notes/participants/add";

/// Adds every item's contact to the one note `uuid`.
#[derive(Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub items: &'a [Item<'a>],
}

/// `/v3/notes/participants/add`'s fields without the note `uuid`.
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Item<'a> {
	#[serde(rename = "contactUUID")]
	pub contact_uuid: ContactUuid,
	pub metadata: RSAEncryptedString<'a>,
	pub permissions_write: bool,
}

/// Data is present only when the contact was added; an existing participant answers
/// `note_participant_added` without data.
pub type Response<'a> = bulk::Response<'a, add::Response<'a>>;
