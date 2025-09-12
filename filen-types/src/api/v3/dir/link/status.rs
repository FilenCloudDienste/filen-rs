use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{
	Deserialize, Deserializer, Serialize,
	de::{self, IntoDeserializer},
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
	uuid: Option<UuidStr>,
	key: Option<EncryptedMetaKey<'a>>,
	#[serde(with = "crate::serde::time::optional")]
	expiration: Option<DateTime<Utc>>,
	expiration_text: Option<PublicLinkExpiration>,
	download_btn: Option<u8>,
	pub password: Option<Cow<'a, str>>,
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

		let password = if let Some(p) = raw.password {
			Some(faster_hex::nopfx_ignorecase::deserialize(
				p.into_deserializer(),
			)?)
		} else {
			None
		};

		Ok(Response(Some(LinkStatus {
			uuid,
			key,
			expiration,
			expiration_text,
			download_btn: download_btn == 1,
			password,
		})))
	}
}

// impl Serialize for Response {
// 	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
// 	where
// 		S: serde::Serializer,
// 	{
// 		let raw_response = match &self.0 {
// 			Some(link_status) => RawResponse {
// 				exists: true,
// 				uuid: Some(link_status.uuid),
// 				key: Some(link_status.key.clone()),
// 				expiration: Some(link_status.expiration),
// 				expiration_text: Some(link_status.expiration_text),
// 				download_btn: Some(if link_status.download_btn { 1 } else { 0 }),
// 				password: Some(link_status.password),
// 			},
// 			None => RawResponse {
// 				exists: false,
// 				uuid: None,
// 				key: None,
// 				expiration: None,
// 				expiration_text: None,
// 				download_btn: None,
// 				password: None,
// 			},
// 		};

// 		raw_response.serialize(serializer)
// 	}
// }
