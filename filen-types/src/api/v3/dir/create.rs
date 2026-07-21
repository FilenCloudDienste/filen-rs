use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::Uuid};

pub const ENDPOINT: &str = "v3/dir/create";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	#[serde(rename = "name")]
	pub meta: EncryptedString<'a>,
	pub name_hashed: Cow<'a, str>,
	pub parent: Uuid,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	pub uuid: Uuid,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
}
