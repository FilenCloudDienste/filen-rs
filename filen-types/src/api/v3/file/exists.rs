use serde::{
	Deserialize, Deserializer, Serialize, Serializer,
	de::{self},
};

use crate::fs::{ParentUuid, UuidStr};

pub const ENDPOINT: &str = "v3/file/exists";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub name_hashed: String,
	pub parent: ParentUuid,
}

#[derive(Debug, Clone)]
pub struct Response(pub Option<UuidStr>);

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawResponse {
	pub(crate) exists: bool,
	#[serde(with = "crate::serde::uuid::optional")]
	pub(crate) uuid: Option<UuidStr>,
}

impl Serialize for Response {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		RawResponse {
			exists: self.0.is_some(),
			uuid: self.0,
		}
		.serialize(serializer)
	}
}

impl<'de> Deserialize<'de> for Response {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let raw_response = RawResponse::deserialize(deserializer)?;
		if raw_response.exists {
			match raw_response.uuid {
				Some(uuid) => Ok(Response(Some(uuid))),
				None => Err(de::Error::missing_field("uuid")),
			}
		} else {
			Ok(Response(None))
		}
	}
}
