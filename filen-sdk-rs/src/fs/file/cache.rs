use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::{
	auth::FileEncryptionVersion, crypto::Blake3Hash, fs::ParentUuid,
	rkyv::date_time::DateTimeUtcDef, traits::CowHelpers,
};
use rkyv::with::Map;
use serde::{Deserialize, Serialize};
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

#[derive(
	Clone, Debug, PartialEq, Eq, CowHelpers, rkyv::Serialize, rkyv::Deserialize, rkyv::Archive,
)]
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
	#[rkyv(with = DateTimeUtcDef)]
	pub timestamp: DateTime<Utc>,

	pub name: Cow<'a, str>,
	pub size: u64,
	pub mime: Cow<'a, str>,
	pub key: FileKey,
	#[rkyv(with = DateTimeUtcDef)]
	pub last_modified: DateTime<Utc>,
	#[rkyv(with = Map<DateTimeUtcDef>)]
	pub created: Option<DateTime<Utc>>,
	pub hash: Option<Blake3Hash>,
}

#[derive(Serialize, Deserialize)]
struct SerializedCacheableFile<'a> {
	uuid: Uuid,
	parent: Uuid,

	chunks_size: u64,
	chunks: u64,
	favorited: bool,
	// TODO: dedup this
	#[serde(borrow)]
	region: Cow<'a, str>,
	// TODO: maybe dedup this too
	#[serde(borrow)]
	bucket: Cow<'a, str>,
	timestamp: DateTime<Utc>,

	#[serde(borrow)]
	name: Cow<'a, str>,
	size: u64,
	#[serde(borrow)]
	mime: Cow<'a, str>,
	#[serde(borrow)]
	key: Cow<'a, str>,
	file_version: FileEncryptionVersion,
	last_modified: DateTime<Utc>,
	created: Option<DateTime<Utc>>,
	hash: Option<Blake3Hash>,
}

impl<'de> Deserialize<'de> for CacheableFile<'de> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let raw = SerializedCacheableFile::deserialize(deserializer)?;
		Ok(Self {
			uuid: raw.uuid,
			parent: raw.parent,
			chunks_size: raw.chunks_size,
			chunks: raw.chunks,
			favorited: raw.favorited,
			region: raw.region,
			bucket: raw.bucket,
			timestamp: raw.timestamp,
			name: raw.name,
			size: raw.size,
			mime: raw.mime,
			key: FileKey::from_str_with_version(&raw.key, raw.file_version)
				.map_err(serde::de::Error::custom)?,
			last_modified: raw.last_modified,
			created: raw.created,
			hash: raw.hash,
		})
	}
}

impl Serialize for CacheableFile<'_> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let key_str = self.key.to_str();
		SerializedCacheableFile {
			uuid: self.uuid,
			parent: self.parent,
			chunks_size: self.chunks_size,
			chunks: self.chunks,
			favorited: self.favorited,
			region: Cow::Borrowed(&self.region),
			bucket: Cow::Borrowed(&self.bucket),
			timestamp: self.timestamp,
			name: Cow::Borrowed(&self.name),
			size: self.size,
			mime: Cow::Borrowed(&self.mime),
			key: Cow::Borrowed(key_str.as_ref()),
			file_version: self.key.version(),
			last_modified: self.last_modified,
			created: self.created,
			hash: self.hash,
		}
		.serialize(serializer)
	}
}

impl TryFrom<RemoteFile> for CacheableFile<'static> {
	type Error = (RemoteFile, Error);

	fn try_from(mut value: RemoteFile) -> Result<Self, Self::Error> {
		let parent = match value.parent {
			ParentUuid::Uuid(uuid) => (&uuid).into(),
			parent => {
				return Err((
					value,
					Error::custom(
						crate::ErrorKind::InvalidState,
						format!(
							"cannot convert remote file to cacheable file with {:?} parent",
							parent
						),
					),
				));
			}
		};

		let decrypted_meta = match value.meta {
			FileMeta::Decoded(meta) => meta,
			other => {
				value.meta = other;
				return Err((
					value,
					Error::custom(
						crate::ErrorKind::MetadataWasNotDecrypted,
						"cannot convert remote file to cacheable file with encrypted meta",
					),
				));
			}
		};

		Ok(Self {
			uuid: (&value.uuid).into(),
			parent,
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
			key: decrypted_meta.key,
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
	pub key: FileKey,
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
			key: decrypted_meta.key,
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
