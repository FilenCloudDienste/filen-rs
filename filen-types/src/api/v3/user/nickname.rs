use std::borrow::Cow;

use serde::Serialize;

pub const ENDPOINT: &str = "v3/user/nickname";

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	#[serde(with = "crate::serde::option::str_empty_is_none_borrowed")]
	pub nickname: Option<Cow<'a, str>>,
}
