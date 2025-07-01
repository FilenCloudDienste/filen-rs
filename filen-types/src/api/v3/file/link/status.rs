use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{api::v3::dir::link::PublicLinkExpiration, fs::UuidStr};

pub const ENDPOINT: &str = "v3/file/link/status";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: UuidStr,
}

#[derive(Debug, Clone)]
pub struct Response<'a>(pub Option<LinkStatus<'a>>);

#[derive(Debug, Clone)]
pub struct LinkStatus<'a> {
	pub uuid: UuidStr,
	pub expiration: DateTime<Utc>,
	pub expiration_text: PublicLinkExpiration,
	pub download_btn: bool,
	pub password: Option<Cow<'a, [u8]>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct RawResponse<'a> {
	enabled: bool,
	uuid: Option<UuidStr>,
	#[serde(with = "crate::serde::time::optional")]
	expiration: Option<DateTime<Utc>>,
	expiration_text: Option<PublicLinkExpiration>,
	download_btn: Option<u8>,
	#[serde(with = "crate::serde::hex::optional")]
	password: Option<Cow<'a, [u8]>>,
}

impl<'de> Deserialize<'de> for Response<'static> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let raw = RawResponse::deserialize(deserializer)?;
		if !raw.enabled {
			return Ok(Response(None));
		}
		let uuid = raw
			.uuid
			.ok_or_else(|| serde::de::Error::missing_field("uuid"))?;
		let expiration = raw
			.expiration
			.ok_or_else(|| serde::de::Error::missing_field("expiration"))?;
		let expiration_text = raw
			.expiration_text
			.ok_or_else(|| serde::de::Error::missing_field("expiration_text"))?;
		let download_btn = raw
			.download_btn
			.ok_or_else(|| serde::de::Error::missing_field("download_btn"))?;

		// Validate that download_btn is either 0 or 1
		if download_btn != 0 && download_btn != 1 {
			return Err(serde::de::Error::custom(
				"downloadBtn must be either 0 or 1",
			));
		}
		Ok(Response(Some(LinkStatus {
			uuid,
			expiration,
			expiration_text,
			download_btn: download_btn == 1,
			password: raw.password,
		})))
	}
}

impl Serialize for Response<'_> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let raw = match &self.0 {
			Some(link_status) => RawResponse {
				enabled: true,
				uuid: Some(link_status.uuid),
				expiration: Some(link_status.expiration),
				expiration_text: Some(link_status.expiration_text),
				download_btn: Some(if link_status.download_btn { 1 } else { 0 }),
				password: link_status
					.password
					.as_ref()
					.map(|c| Cow::Borrowed(c.as_ref())),
			},
			None => RawResponse {
				enabled: false,
				uuid: None,
				expiration: None,
				expiration_text: None,
				download_btn: None,
				password: None,
			},
		};
		raw.serialize(serializer)
	}
}
