use std::{borrow::Cow, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{crypto::rsa::RSAEncryptedString, fs::Uuid};

pub const ENDPOINT: &str = "v3/notes/participants/add";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	#[serde(rename = "contactUUID")]
	pub contact_uuid: ContactUuid,
	pub metadata: RSAEncryptedString<'a>,
	pub permissions_write: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub avatar: Option<Cow<'a, str>>,
	pub nick_name: Cow<'a, str>,
}

#[derive(Debug, Clone)]
pub enum ContactUuid {
	Uuid(Uuid),
	Owner,
}

impl FromStr for ContactUuid {
	type Err = uuid::Error;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		if s == "owner" {
			Ok(Self::Owner)
		} else {
			Ok(Self::Uuid(Uuid::from_str(s)?))
		}
	}
}

impl Serialize for ContactUuid {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match self {
			Self::Uuid(uuid) => uuid.serialize(serializer),
			Self::Owner => serializer.serialize_str("owner"),
		}
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
