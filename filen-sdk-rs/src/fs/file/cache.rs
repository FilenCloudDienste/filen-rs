use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::{crypto::Blake3Hash, fs::ParentUuid, traits::CowHelpers};
use uuid::Uuid;

use crate::{
	Error,
	crypto::file::FileKey,
	fs::file::{
		FileVersion,
		meta::{DecryptedFileMeta, FileMeta},
	},
	io::RemoteFile,
};

#[derive(Clone, Debug, PartialEq, Eq, CowHelpers)]
pub struct CacheableFile<'a> {
	pub uuid: Uuid,
	pub parent: Uuid,

	pub chunks_size: u64,
	pub chunks: u64,
	pub favorited: bool,
	// TODO: dedup this
	pub region: Cow<'a, str>,
	// TODO: maybe dedup this too
	pub bucket: Cow<'a, str>,
	pub timestamp: DateTime<Utc>,

	pub name: Cow<'a, str>,
	pub size: u64,
	pub mime: Cow<'a, str>,
	pub key: Cow<'a, FileKey>,
	pub last_modified: DateTime<Utc>,
	pub created: Option<DateTime<Utc>>,
	pub hash: Option<Blake3Hash>,
}

impl TryFrom<RemoteFile> for CacheableFile<'static> {
	type Error = Error;

	fn try_from(value: RemoteFile) -> Result<Self, Self::Error> {
		let decrypted_meta = match value.meta {
			FileMeta::Decoded(meta) => meta,
			_ => {
				return Err(Error::custom(
					crate::ErrorKind::MetadataWasNotDecrypted,
					"cannot convert remote file to cacheable file with encrypted meta",
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
							"cannot convert remote file to cacheable file with {:?} parent",
							parent
						),
					));
				}
			},
			chunks_size: value.size,
			chunks: value.chunks,
			favorited: value.favorited,
			region: Cow::Owned(value.region),
			bucket: Cow::Owned(value.bucket),
			timestamp: value.timestamp,
			name: decrypted_meta.name,
			size: value.size,
			mime: decrypted_meta.mime,
			key: decrypted_meta.key,
			last_modified: decrypted_meta.last_modified,
			created: decrypted_meta.created,
			hash: decrypted_meta.hash,
		})
	}
}

impl<'a> TryFrom<&'a RemoteFile> for CacheableFile<'a> {
	type Error = Error;

	fn try_from(value: &'a RemoteFile) -> Result<Self, Self::Error> {
		let decrypted_meta = match &value.meta {
			FileMeta::Decoded(meta) => meta,
			_ => {
				return Err(Error::custom(
					crate::ErrorKind::MetadataWasNotDecrypted,
					"cannot convert remote file to cacheable file with encrypted meta",
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
							"cannot convert remote file to cacheable file with {:?} parent",
							parent
						),
					));
				}
			},
			chunks_size: value.size,
			chunks: value.chunks,
			favorited: value.favorited,
			region: Cow::Borrowed(&value.region),
			bucket: Cow::Borrowed(&value.bucket),
			timestamp: value.timestamp,
			name: Cow::Borrowed(&decrypted_meta.name),
			size: value.size,
			mime: Cow::Borrowed(&decrypted_meta.mime),
			key: Cow::Borrowed(&decrypted_meta.key),
			last_modified: decrypted_meta.last_modified,
			created: decrypted_meta.created,
			hash: decrypted_meta.hash,
		})
	}
}

impl From<CacheableFile<'static>> for RemoteFile {
	fn from(value: CacheableFile<'static>) -> Self {
		Self {
			uuid: (&value.uuid).into(),
			parent: ParentUuid::Uuid((&value.parent).into()),
			chunks: value.chunks,
			size: value.chunks_size,
			favorited: value.favorited,
			region: value.region.into_owned(),
			bucket: value.bucket.into_owned(),
			timestamp: value.timestamp,
			meta: FileMeta::Decoded(DecryptedFileMeta {
				size: value.size,
				name: value.name,
				mime: value.mime,
				key: value.key,
				last_modified: value.last_modified,
				created: value.created,
				hash: value.hash,
			}),
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq, CowHelpers)]
pub struct CacheableFileVersion<'a> {
	pub uuid: Uuid,

	pub chunks_size: u64,
	pub chunks: u64,
	// TODO: dedup this
	pub region: Cow<'a, str>,
	// TODO: maybe dedup this too
	pub bucket: Cow<'a, str>,
	pub timestamp: DateTime<Utc>,

	pub name: Cow<'a, str>,
	pub size: u64,
	pub mime: Cow<'a, str>,
	pub key: Cow<'a, FileKey>,
	pub last_modified: DateTime<Utc>,
	pub created: Option<DateTime<Utc>>,
	pub hash: Option<Blake3Hash>,
}

impl<'a> TryFrom<&'a FileVersion> for CacheableFileVersion<'a> {
	type Error = Error;

	fn try_from(value: &'a FileVersion) -> Result<Self, Self::Error> {
		let decrypted_meta = match &value.metadata {
			FileMeta::Decoded(meta) => meta,
			_ => {
				return Err(Error::custom(
					crate::ErrorKind::MetadataWasNotDecrypted,
					"cannot convert file version to cacheable file version with encrypted meta",
				));
			}
		};

		Ok(Self {
			uuid: (&value.uuid).into(),
			chunks_size: value.size,
			chunks: value.chunks,
			region: Cow::Borrowed(&value.region),
			bucket: Cow::Borrowed(&value.bucket),
			timestamp: value.timestamp,
			name: Cow::Borrowed(&decrypted_meta.name),
			size: value.size,
			mime: Cow::Borrowed(&decrypted_meta.mime),
			key: Cow::Borrowed(&decrypted_meta.key),
			last_modified: decrypted_meta.last_modified,
			created: decrypted_meta.created,
			hash: decrypted_meta.hash,
		})
	}
}

impl From<CacheableFileVersion<'static>> for FileVersion {
	fn from(value: CacheableFileVersion<'static>) -> Self {
		Self {
			uuid: (&value.uuid).into(),
			chunks: value.chunks,
			size: value.chunks_size,
			region: value.region.into_owned(),
			bucket: value.bucket.into_owned(),
			timestamp: value.timestamp,
			metadata: FileMeta::Decoded(DecryptedFileMeta {
				size: value.size,
				name: value.name,
				mime: value.mime,
				key: value.key,
				last_modified: value.last_modified,
				created: value.created,
				hash: value.hash,
			}),
		}
	}
}
