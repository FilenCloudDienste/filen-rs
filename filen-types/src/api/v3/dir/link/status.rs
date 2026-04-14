use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{
	Deserialize, Deserializer, Serialize,
	de::{self},
};

use crate::{crypto::EncryptedMetaKey, fs::UuidStr};

use super::PublicLinkExpiration;

pub const ENDPOINT: &str = "v3/dir/link/status";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: UuidStr,
}

#[derive(Debug, Clone)]
pub struct Response<'a>(pub Option<LinkStatus<'a>>);

#[derive(Debug, Clone)]
pub struct LinkStatus<'a> {
	pub uuid: UuidStr,
	pub key: EncryptedMetaKey<'a>,
	pub expiration: DateTime<Utc>,
	pub expiration_text: PublicLinkExpiration,
	pub download_btn: bool,
	pub password: Option<Cow<'a, [u8]>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct RawResponse<'a> {
	exists: bool,
	#[serde(default)]
	uuid: Option<UuidStr>,
	#[serde(default)]
	key: Option<EncryptedMetaKey<'a>>,
	#[serde(with = "crate::serde::time::optional", default)]
	expiration: Option<DateTime<Utc>>,
	#[serde(default)]
	expiration_text: Option<PublicLinkExpiration>,
	#[serde(default)]
	download_btn: Option<u8>,
	#[serde(with = "crate::serde::hex::optional", default)]
	password: Option<Cow<'a, [u8]>>,
}

impl<'de> Deserialize<'de> for Response<'static> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let raw = RawResponse::deserialize(deserializer)?;

		if !raw.exists {
			return Ok(Response(None));
		}

		// Validate that all required fields are present when exists is true
		let uuid = raw.uuid.ok_or_else(|| de::Error::missing_field("uuid"))?;
		let key = raw.key.ok_or_else(|| de::Error::missing_field("key"))?;
		let expiration = raw
			.expiration
			.ok_or_else(|| de::Error::missing_field("expiration"))?;
		let expiration_text = raw
			.expiration_text
			.ok_or_else(|| de::Error::missing_field("expirationText"))?;
		let download_btn = raw
			.download_btn
			.ok_or_else(|| de::Error::missing_field("downloadBtn"))?;

		// Validate that download_btn is either 0 or 1
		if download_btn != 0 && download_btn != 1 {
			return Err(de::Error::custom("downloadBtn must be either 0 or 1"));
		}

		Ok(Response(Some(LinkStatus {
			uuid,
			key,
			expiration,
			expiration_text,
			download_btn: download_btn == 1,
			password: raw.password,
		})))
	}
}
