use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/upload/done";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	#[serde(flatten)]
	pub empty_request: super::empty::Request<'a>,
	pub chunks: u64,
	pub rm: Cow<'a, str>,
	pub upload_key: Cow<'a, str>,
}

pub use super::empty::Response;
