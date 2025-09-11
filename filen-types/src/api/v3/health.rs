use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/health";

const OK_STR: &str = "OK";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response(pub OK);

#[derive(Debug, Clone)]
pub struct OK;

impl Serialize for OK {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		crate::serde::cow::serialize(Cow::Borrowed(OK_STR), serializer)
	}
}

impl<'de> Deserialize<'de> for OK {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let cow = crate::serde::cow::deserialize(deserializer)?;
		if cow.as_ref() == OK_STR {
			Ok(OK)
		} else {
			Err(serde::de::Error::custom("expected OK"))
		}
	}
}
