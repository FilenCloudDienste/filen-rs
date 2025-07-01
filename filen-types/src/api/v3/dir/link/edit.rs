use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{api::v3::dir::link::PublicLinkExpiration, fs::UuidStr};

pub const ENDPOINT: &str = "v3/dir/link/edit";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub expiration: PublicLinkExpiration,
	#[serde(with = "crate::serde::boolean::empty_notempty")]
	pub password: bool,
	#[serde(with = "faster_hex::nopfx_ignorecase")]
	pub password_hashed: Cow<'a, [u8]>,
	#[serde(with = "faster_hex::nopfx_ignorecase")]
	pub salt: Cow<'a, [u8]>,
	pub download_btn: bool,
}
