use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub const ENDPOINT: &str = "v3/dir/download/link";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	#[serde(with = "faster_hex::nopfx_ignorecase")]
	pub password: Cow<'a, [u8]>,
	pub parent: UuidStr,
	pub skip_cache: bool,
}

pub use super::Response;
