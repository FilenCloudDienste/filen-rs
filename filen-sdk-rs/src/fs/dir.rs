use std::borrow::Cow;

use chrono::{DateTime, SubsecRound, Utc};
use filen_types::crypto::EncryptedString;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::{error::ConversionError, shared::MetaCrypter};

use super::{HasContents, HasMeta, HasParent, HasUUID};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryType<'a> {
	Root(Cow<'a, RootDirectory>),
	Dir(Cow<'a, Directory>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootDirectory {
	uuid: Uuid,
}

impl RootDirectory {
	pub(crate) fn new(uuid: Uuid) -> Self {
		Self { uuid }
	}
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Directory {
	pub(super) uuid: Uuid,
	name: String,
	pub(super) parent: Uuid,

	color: Option<String>, // todo use Color struct
	created: Option<DateTime<Utc>>,
	favorited: bool,
}

impl Directory {
	pub fn from_encrypted(
		dir: filen_types::api::v3::dir::content::Directory,
		decrypter: impl MetaCrypter,
	) -> Result<Self, crate::error::Error> {
		let meta = DirectoryMeta::from_encrypted(&dir.meta, decrypter)?;
		Ok(Self {
			name: meta.name.into_owned(),
			uuid: dir.uuid,
			parent: dir.parent,
			color: dir.color,
			created: meta.created,
			favorited: dir.favorited,
		})
	}

	pub fn try_from_encrypted(
		dir: filen_types::api::v3::dir::download::Directory,
		decrypter: impl MetaCrypter,
	) -> Result<Option<Self>, crate::error::Error> {
		let parent = match dir.parent {
			None => return Ok(None),
			Some(parent) => parent,
		};
		let meta = DirectoryMeta::from_encrypted(&dir.meta, decrypter)?;
		Ok(Some(Self {
			name: meta.name.into_owned(),
			uuid: dir.uuid,
			parent,
			color: dir.color,
			created: meta.created,
			favorited: dir.favorited,
		}))
	}

	pub fn new(name: String, parent: Uuid, created: DateTime<Utc>) -> Self {
		Self {
			uuid: Uuid::new_v4(),
			name,
			parent,
			color: None,
			created: Some(created.round_subsecs(3)),
			favorited: false,
		}
	}
}

// should probably write a macro for this

impl HasUUID for &RootDirectory {
	fn uuid(&self) -> uuid::Uuid {
		self.uuid
	}
}

impl HasUUID for RootDirectory {
	fn uuid(&self) -> uuid::Uuid {
		(&self).uuid()
	}
}

impl HasContents for RootDirectory {}
impl HasContents for &RootDirectory {}

impl HasUUID for Directory {
	fn uuid(&self) -> uuid::Uuid {
		self.uuid
	}
}
impl HasUUID for &Directory {
	fn uuid(&self) -> uuid::Uuid {
		self.uuid
	}
}
impl HasContents for Directory {}
impl HasContents for &Directory {}

impl HasUUID for &DirectoryType<'_> {
	fn uuid(&self) -> uuid::Uuid {
		match self {
			DirectoryType::Root(dir) => dir.uuid(),
			DirectoryType::Dir(dir) => dir.uuid(),
		}
	}
}

impl HasUUID for DirectoryType<'_> {
	fn uuid(&self) -> uuid::Uuid {
		(&self).uuid()
	}
}

impl HasContents for DirectoryType<'_> {}
impl HasContents for &DirectoryType<'_> {}

impl HasParent for Directory {
	fn parent(&self) -> uuid::Uuid {
		self.parent
	}
}

impl HasMeta for Directory {
	fn name(&self) -> &str {
		&self.name
	}

	fn meta(&self, crypter: impl MetaCrypter) -> Result<EncryptedString, ConversionError> {
		// SAFETY if this fails, I want it to panic
		// as this is a logic error
		let string = serde_json::to_string(&DirectoryMeta {
			name: Cow::Borrowed(&self.name),
			created: self.created,
		})
		.unwrap();
		crypter.encrypt_meta(&string)
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

#[derive(Debug, Serialize, Deserialize)]
struct DirectoryMeta<'a> {
	name: Cow<'a, str>,
	#[serde(with = "dir_meta_serde")]
	#[serde(rename = "creation")]
	#[serde(default)]
	created: Option<DateTime<Utc>>,
}

impl DirectoryMeta<'static> {
	pub fn from_encrypted(
		encrypted: &EncryptedString,
		decrypter: impl MetaCrypter,
	) -> Result<Self, crate::error::Error> {
		let decrypted = decrypter.decrypt_meta(encrypted)?;
		let meta = serde_json::from_str(&decrypted)?;
		Ok(meta)
	}
}
