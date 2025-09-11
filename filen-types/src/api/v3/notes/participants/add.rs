use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/notes/participants/add";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	#[serde(rename = "contactUUID")]
	pub contact_uuid: UuidStr,
	pub metadata: Cow<'a, EncryptedString>,
	pub permissions_write: bool,
}
