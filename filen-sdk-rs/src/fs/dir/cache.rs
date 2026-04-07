use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::{api::v3::dir::color::DirColor, fs::ParentUuid, traits::CowHelpers};
use uuid::Uuid;

use crate::{Error, fs::dir::meta::DirectoryMeta, io::RemoteDirectory};

#[derive(Clone, Debug, PartialEq, Eq, CowHelpers)]
pub struct CacheableDir<'a> {
	pub uuid: Uuid,
	pub parent: Uuid,
	pub color: DirColor<'a>,
	pub favorited: bool,
	pub timestamp: DateTime<Utc>,

	pub name: Cow<'a, str>,
	pub created: Option<DateTime<Utc>>,
}

impl TryFrom<RemoteDirectory> for CacheableDir<'static> {
	type Error = Error;

	fn try_from(value: RemoteDirectory) -> Result<Self, Self::Error> {
		let decrypted_meta = match value.meta {
			DirectoryMeta::Decoded(meta) => meta,
			_ => {
				return Err(Error::custom(
					crate::ErrorKind::MetadataWasNotDecrypted,
					"cannot convert remote dir to cacheable dir with encrypted meta",
				));
			}
		};

		Ok(Self {
			uuid: (&value.uuid).into(),
			parent: match value.parent {
				ParentUuid::Uuid(uuid) => (&uuid).into(),
				parent => {
					return Err(Error::custom(
						crate::ErrorKind::InvalidState,
						format!(
							"cannot convert remote dir to cacheable dir with {:?} parent",
							parent
						),
					));
				}
			},
			color: value.color,
			favorited: value.favorited,
			timestamp: value.timestamp,

			name: decrypted_meta.name,
			created: decrypted_meta.created,
		})
	}
}

impl<'a> TryFrom<&'a RemoteDirectory> for CacheableDir<'a> {
	type Error = Error;

	fn try_from(value: &'a RemoteDirectory) -> Result<Self, Self::Error> {
		let decrypted_meta = match &value.meta {
			DirectoryMeta::Decoded(meta) => meta,
			_ => {
				return Err(Error::custom(
					crate::ErrorKind::MetadataWasNotDecrypted,
					"cannot convert remote dir to cacheable dir with encrypted meta",
				));
			}
		};

		Ok(Self {
			uuid: (&value.uuid).into(),
			parent: match value.parent {
				ParentUuid::Uuid(uuid) => (&uuid).into(),
				parent => {
					return Err(Error::custom(
						crate::ErrorKind::InvalidState,
						format!(
							"cannot convert remote dir to cacheable dir with {:?} parent",
							parent
						),
					));
				}
			},
			color: value.color.as_borrowed_cow(),
			favorited: value.favorited,
			timestamp: value.timestamp,

			name: Cow::Borrowed(&decrypted_meta.name),
			created: decrypted_meta.created,
		})
	}
}
