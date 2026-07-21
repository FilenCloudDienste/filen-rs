use serde::{Deserialize, Serialize};

use crate::{crypto::LinkHashedPassword, fs::Uuid};

pub const ENDPOINT: &str = "v3/dir/download/link";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub password: LinkHashedPassword<'a>,
	pub parent: Uuid,
	pub skip_cache: bool,
}

pub use super::Response;
