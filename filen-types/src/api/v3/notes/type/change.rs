use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{api::v3::notes::NoteType, fs::UuidStr};

pub const ENDPOINT: &str = "v3/notes/type/change";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub r#type: NoteType,
	pub preview: Cow<'a, str>,
	pub content: Cow<'a, str>,
}
