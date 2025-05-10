use std::borrow::Cow;

use chrono::{DateTime, SubsecRound, Utc};
use filen_types::crypto::{EncryptedString, rsa::RSAEncryptedString};
use rsa::RsaPrivateKey;
use serde::{Deserialize, Serialize};

use crate::crypto::{self, shared::MetaCrypter};

#[derive(Debug, Serialize, Deserialize)]
pub struct DirectoryMeta<'a> {
	pub(super) name: Cow<'a, str>,
	#[serde(with = "dir_meta_serde")]
	#[serde(rename = "creation")]
	#[serde(default)]
	pub(super) created: Option<DateTime<Utc>>,
}

impl DirectoryMeta<'static> {
	pub fn from_encrypted(
		encrypted: &EncryptedString,
		decrypter: &impl MetaCrypter,
	) -> Result<Self, crate::error::Error> {
		let decrypted = decrypter.decrypt_meta(encrypted)?;
		let meta = serde_json::from_str(&decrypted)?;
		Ok(meta)
	}

	pub fn from_rsa_encrypted(
		encrypted: &RSAEncryptedString,
		decrypter: &RsaPrivateKey,
	) -> Result<Self, crate::error::Error> {
		let decrypted = crypto::rsa::decrypt_with_private_key(decrypter, encrypted)?;
		let meta = serde_json::from_slice(decrypted.as_ref())?;
		Ok(meta)
	}
}

impl<'a> DirectoryMeta<'a> {
	pub fn name(&self) -> &str {
		&self.name
	}

	pub fn created(&self) -> Option<DateTime<Utc>> {
		self.created
	}

	pub fn set_name(&mut self, name: impl Into<Cow<'a, str>>) {
		self.name = name.into();
	}

	pub fn set_created(&mut self, created: DateTime<Utc>) {
		self.created = Some(created.round_subsecs(3));
	}
}

mod dir_meta_serde {
	use chrono::{DateTime, Utc};
	use serde::de::Visitor;

	pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		struct OptionalDateTimeVisitor;
		impl<'de> Visitor<'de> for OptionalDateTimeVisitor {
			type Value = Option<DateTime<Utc>>;

			fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
				formatter.write_str("an optional timestamp in milliseconds")
			}

			fn visit_none<E>(self) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				Ok(None)
			}

			fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
			where
				D: serde::Deserializer<'de>,
			{
				deserializer.deserialize_i64(self)
			}

			fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				Ok(Some(
					chrono::DateTime::<Utc>::from_timestamp_millis(v)
						.ok_or_else(|| serde::de::Error::custom("Invalid timestamp"))?,
				))
			}

			fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				self.visit_i64(v.try_into().map_err(|_| {
					serde::de::Error::custom("Invalid timestamp: cannot convert u64 to i64")
				})?)
			}
		}
		deserializer.deserialize_option(OptionalDateTimeVisitor)
	}

	pub(super) fn serialize<S>(
		value: &Option<DateTime<Utc>>,
		serializer: S,
	) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match value {
			Some(dt) => serializer.serialize_i64(dt.timestamp_millis()),
			None => serializer.serialize_none(),
		}
	}
}
