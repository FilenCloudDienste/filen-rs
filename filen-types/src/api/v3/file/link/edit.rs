use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::v3::dir::link::PublicLinkExpiration;

pub const ENDPOINT: &str = "v3/file/link/edit";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	#[serde(rename = "fileUUID")]
	pub file_uuid: Uuid,
	pub expiration: PublicLinkExpiration,
	#[serde(with = "crate::serde::boolean::empty_notempty")]
	pub password: bool,
	#[serde(with = "faster_hex::nopfx_ignorecase")]
	pub password_hashed: Cow<'a, [u8]>,
	#[serde(with = "faster_hex::nopfx_ignorecase")]
	pub salt: Cow<'a, [u8]>,
	pub download_btn: bool,
	pub r#type: FileLinkAction,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub enum FileLinkAction {
	Enable,
	Disable,
}
