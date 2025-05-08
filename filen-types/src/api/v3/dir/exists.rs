use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/dir/exists";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub name_hashed: Cow<'a, str>,
	pub parent: uuid::Uuid,
}

pub use crate::api::v3::file::exists::Response;
