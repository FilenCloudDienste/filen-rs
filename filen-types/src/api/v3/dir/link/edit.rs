use serde::{Deserialize, Serialize};

use crate::{
	api::v3::dir::link::{PublicLinkExpiration, info::LinkPasswordSalt},
	crypto::LinkHashedPassword,
	fs::UuidStr,
};

pub const ENDPOINT: &str = "v3/dir/link/edit";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub expiration: PublicLinkExpiration,
	#[serde(with = "crate::serde::boolean::empty_notempty")]
	pub password: bool,
	pub password_hashed: LinkHashedPassword<'a>,
	pub salt: LinkPasswordSalt<'a>,
	pub download_btn: bool,
}
