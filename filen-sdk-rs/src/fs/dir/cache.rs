use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::dir::color::DirColor, fs::ParentUuid, rkyv::date_time::DateTimeUtcDef,
	traits::CowHelpers,
};
use rkyv::with::Map;
use uuid::Uuid;

use crate::{
	fs::{cache::CacheableConversionError, dir::meta::DirectoryMeta},
	io::RemoteDirectory,
};

#[derive(
	Clone, Debug, PartialEq, Eq, CowHelpers, rkyv::Serialize, rkyv::Deserialize, rkyv::Archive,
)]
pub struct CacheableDir<'a> {
	pub uuid: Uuid,
	pub parent: Uuid,
	pub color: DirColor<'a>,
	pub favorited: bool,
	#[rkyv(with = DateTimeUtcDef)]
	pub timestamp: DateTime<Utc>,

	pub name: Cow<'a, str>,
	#[rkyv(with = Map<DateTimeUtcDef>)]
	pub created: Option<DateTime<Utc>>,
}

impl TryFrom<RemoteDirectory> for CacheableDir<'static> {
	type Error = (RemoteDirectory, CacheableConversionError);

	fn try_from(mut value: RemoteDirectory) -> Result<Self, Self::Error> {
		let parent = match value.parent {
			ParentUuid::Uuid(uuid) => (&uuid).into(),
			other => {
				return Err((value, CacheableConversionError::ParentNotUuid(other)));
			}
		};

		let decrypted_meta = match value.meta {
			DirectoryMeta::Decoded(meta) => meta,
			other => {
				let debug = format!("{:?}", other);
				value.meta = other;
				return Err((value, CacheableConversionError::MetadataNotDecrypted(debug)));
			}
		};

		Ok(Self {
			uuid: (&value.uuid).into(),
			parent,
			color: value.color,
			favorited: value.favorited,
			timestamp: value.timestamp,

			name: decrypted_meta.name,
			created: decrypted_meta.created,
		})
	}
}

impl<'a> TryFrom<&'a RemoteDirectory> for CacheableDir<'a> {
	type Error = CacheableConversionError;

	fn try_from(value: &'a RemoteDirectory) -> Result<Self, Self::Error> {
		let decrypted_meta = match &value.meta {
			DirectoryMeta::Decoded(meta) => meta,
			other => {
				return Err(CacheableConversionError::MetadataNotDecrypted(format!(
					"{:?}",
					other
				)));
			}
		};

		Ok(Self {
			uuid: (&value.uuid).into(),
			parent: match value.parent {
				ParentUuid::Uuid(uuid) => (&uuid).into(),
				other => {
					return Err(CacheableConversionError::ParentNotUuid(other));
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
