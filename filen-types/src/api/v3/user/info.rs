use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub const ENDPOINT: &str = "v3/user/info";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub id: u64,
	pub email: Cow<'a, str>,
	#[serde(with = "crate::serde::boolean::number")]
	pub is_premium: bool,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub max_storage: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub storage_used: u64,
	#[serde(
		rename = "avatarURL",
		with = "crate::serde::option::str_empty_is_none_owned"
	)]
	pub avatar_url: Option<String>,
	#[serde(rename = "baseFolderUUID")]
	pub root_dir_uuid: UuidStr,
}
