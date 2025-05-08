use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::EncryptedString;

pub const ENDPOINT: &str = "v3/dir/metadata";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub name_hashed: Cow<'a, str>,
	#[serde(rename = "name")]
	pub metadata: Cow<'a, EncryptedString>,
}
