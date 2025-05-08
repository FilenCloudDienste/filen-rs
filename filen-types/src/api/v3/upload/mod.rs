use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub mod done;
pub mod empty;

pub const ENDPOINT: &str = "v3/upload";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
}
