pub mod color;
pub mod content;
pub mod create;
pub mod delete;
pub mod download;
pub mod exists;
pub mod link;
pub mod metadata;
pub mod r#move;
pub mod restore;
pub mod size;
pub mod trash;

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/dir";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: UuidStr,
}

use crate::{
	api::v3::dir::color::DirColor,
	crypto::EncryptedString,
	fs::{ParentUuid, UuidStr},
};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub uuid: UuidStr,
	#[serde(rename = "nameEncrypted")]
	pub metadata: Cow<'a, EncryptedString>,
	pub name_hashed: Cow<'a, str>,
	pub parent: ParentUuid,
	pub trash: bool,
	pub favorited: bool,
	pub color: DirColor<'a>,
}
