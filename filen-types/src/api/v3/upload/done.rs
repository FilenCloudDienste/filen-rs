use std::borrow::Cow;

use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	#[serde(flatten)]
	pub empty_request: super::empty::Request,
	pub chunks: u64,
	pub rm: String,
	pub upload_key: Cow<'a, str>,
}

pub use super::empty::Response;
