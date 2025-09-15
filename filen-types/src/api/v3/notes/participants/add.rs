use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{crypto::rsa::RSAEncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/notes/participants/add";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	#[serde(rename = "contactUUID")]
	pub contact_uuid: ContactUuid,
	pub metadata: RSAEncryptedString<'a>,
	pub permissions_write: bool,
}

#[derive(Debug, Clone)]
pub enum ContactUuid {
	Uuid(UuidStr),
	Owner,
}

impl AsRef<str> for ContactUuid {
	fn as_ref(&self) -> &str {
		match self {
			Self::Uuid(uuid) => uuid.as_ref(),
			Self::Owner => "owner",
		}
	}
}

impl FromStr for ContactUuid {
	type Err = uuid::Error;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		if s == "owner" {
			Ok(Self::Owner)
		} else {
			Ok(Self::Uuid(UuidStr::from_str(s)?))
		}
	}
}

impl Serialize for ContactUuid {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		serializer.serialize_str(self.as_ref())
	}
}

impl<'de> Deserialize<'de> for ContactUuid {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let str = crate::serde::cow::deserialize(deserializer)?;
		Self::from_str(&str).map_err(serde::de::Error::custom)
	}
}
