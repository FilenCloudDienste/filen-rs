use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/user/info";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub id: u64,
	pub email: Cow<'a, str>,
	#[serde(with = "crate::serde::boolean::number")]
	pub is_premium: bool,
	pub max_storage: u64,
	pub storage_used: u64,
	#[serde(rename = "avatarURL")]
	pub avatar_url: Cow<'a, str>,
	#[serde(rename = "baseFolderUUID")]
	pub root_dir_uuid: Cow<'a, str>,
}
