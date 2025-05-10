use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::EncryptedString;

pub const ENDPOINT: &str = "v3/item/linked/rename";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	#[serde(rename = "linkUUID")]
	pub link_uuid: Uuid,
	pub metadata: Cow<'a, EncryptedString>,
}
