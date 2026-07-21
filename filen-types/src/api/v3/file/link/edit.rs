use serde::{Deserialize, Serialize};

use crate::{
	api::v3::dir::link::{PublicLinkExpiration, info::LinkPasswordSalt},
	crypto::LinkHashedPassword,
	fs::Uuid,
};

pub const ENDPOINT: &str = "v3/file/link/edit";

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	#[serde(rename = "fileUUID")]
	pub file_uuid: Uuid,
	pub expiration: PublicLinkExpiration,
	#[serde(with = "crate::serde::boolean::empty_notempty")]
	pub password: bool,
	pub password_hashed: LinkHashedPassword<'a>,
	pub salt: &'a LinkPasswordSalt,
	pub download_btn: bool,
	pub r#type: FileLinkAction,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub enum FileLinkAction {
	Enable,
	Disable,
}
