use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::{
	auth::FileEncryptionVersion, crypto::Blake3Hash, fs::ParentUuid,
	rkyv::date_time::DateTimeUtcDef, traits::CowHelpers,
};
use rkyv::with::{AsOwned, Map};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
	Error,
	crypto::file::FileKey,
	fs::{
		cache::CacheableConversionError,
		file::{
			FileVersion,
			meta::{DecryptedFileMeta, FileMeta},
		},
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
	#[rkyv(with = AsOwned)]
	pub region: Cow<'a, str>,
	// TODO: maybe dedup this too
	#[rkyv(with = AsOwned)]
	pub bucket: Cow<'a, str>,
	#[rkyv(with = DateTimeUtcDef)]
	pub timestamp: DateTime<Utc>,

	#[rkyv(with = AsOwned)]
	pub name: Cow<'a, str>,
	pub size: u64,
	#[rkyv(with = AsOwned)]
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
	type Error = (RemoteFile, CacheableConversionError);

	fn try_from(mut value: RemoteFile) -> Result<Self, Self::Error> {
		let parent = match value.parent {
			ParentUuid::Uuid(uuid) => (&uuid).into(),
			other => {
				return Err((value, CacheableConversionError::ParentNotUuid(other)));
			}
		};

		let decrypted_meta = match value.meta {
			FileMeta::Decoded(meta) => meta,
			other => {
				let debug = format!("{:?}", other);
				value.meta = other;
				return Err((value, CacheableConversionError::MetadataNotDecrypted(debug)));
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
	type Error = CacheableConversionError;

	fn try_from(value: &'a RemoteFile) -> Result<Self, Self::Error> {
		let decrypted_meta = match &value.meta {
			FileMeta::Decoded(meta) => meta,
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

impl CacheableFile<'_> {
	/// Two `CacheableFile`s hash equal iff they represent the same logical file *content*,
	/// regardless of which path built them (a live socket event versus a recursive listing). The
	/// cache resync diff compares this fingerprint — stored per item — instead of the derived
	/// [`PartialEq`], so a field that differs *by construction path* (rather than because the file
	/// actually changed) does not produce a spurious `Changed` event and resync churn.
	///
	/// Excluded fields:
	/// - `region` / `bucket`: physical storage location, not content identity (a server-side
	///   rebalance must not look like a content change).
	/// - `timestamp`: socket events carry the *event* time while a listing carries the upload/
	///   listing time, so it diverges across paths and is not a content signal.
	/// - `chunks_size`: an exact alias of `size` (both are populated from `RemoteFile::size` in
	///   [`TryFrom`]), so it carries no information beyond `size`.
	pub fn content_fingerprint(&self) -> [u8; 32] {
		use crate::fs::cache::fingerprint::{write_dt_ms, write_opt_dt_ms};

		let mut hasher = blake3::Hasher::new();
		hasher.update(self.uuid.as_bytes());
		hasher.update(self.parent.as_bytes());
		hasher.update(self.name.as_bytes());
		hasher.update(&self.size.to_le_bytes());
		hasher.update(self.mime.as_bytes());
		// Version-tagged: `to_str` is a deterministic, stable encoding for a given key version.
		hasher.update(&[self.key.version() as u8]);
		hasher.update(self.key.to_str().as_ref().as_bytes());
		write_dt_ms(&mut hasher, self.last_modified);
		write_opt_dt_ms(&mut hasher, self.created);
		match &self.hash {
			Some(hash) => {
				hasher.update(&[1]);
				hasher.update(hash.as_sized_str().as_slice());
			}
			None => {
				hasher.update(&[0]);
			}
		}
		hasher.update(&self.chunks.to_le_bytes());
		hasher.update(&[u8::from(self.favorited)]);
		*hasher.finalize().as_bytes()
	}
}

#[cfg(test)]
mod fingerprint_tests {
	use super::*;

	fn dt(ms: i64) -> DateTime<Utc> {
		DateTime::from_timestamp_millis(ms).expect("valid timestamp")
	}

	fn base() -> CacheableFile<'static> {
		CacheableFile {
			uuid: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
			parent: Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222),
			chunks_size: 4096,
			chunks: 1,
			favorited: false,
			region: Cow::Borrowed("de-1"),
			bucket: Cow::Borrowed("bucket-a"),
			timestamp: dt(1_700_000_000_000),
			name: Cow::Borrowed("photo.txt"),
			size: 4096,
			mime: Cow::Borrowed("text/plain"),
			key: FileKey::from_str_with_version(&"c".repeat(32), FileEncryptionVersion::V2)
				.expect("valid v2 key"),
			last_modified: dt(1_700_000_000_000),
			created: Some(dt(1_699_000_000_000)),
			hash: None,
		}
	}

	/// The headline guarantee: fields that differ *only because of which path built the item*
	/// (storage location, event timestamp, the `chunks_size` alias) must NOT count as a content
	/// change. A naive change detector using the derived `PartialEq` would flag them and churn the
	/// resync; the fingerprint must not.
	#[test]
	fn fingerprint_excludes_storage_location_timestamp_and_chunks_size() {
		let a = base();
		let mut b = a.clone();
		b.region = Cow::Borrowed("us-east-2");
		b.bucket = Cow::Borrowed("bucket-z");
		b.timestamp = dt(1_800_000_000_000);
		b.chunks_size = 8192;

		// Derived PartialEq sees a difference (this is what would cause spurious `Changed`s):
		assert_ne!(a, b);
		// The content fingerprint treats them as the same logical content:
		assert_eq!(a.content_fingerprint(), b.content_fingerprint());
	}

	/// Every field that IS part of content identity must change the fingerprint.
	#[test]
	fn fingerprint_changes_with_each_content_field() {
		let base = base();
		let baseline = base.content_fingerprint();
		let fp = |mutate: fn(&mut CacheableFile<'static>)| {
			let mut c = base.clone();
			mutate(&mut c);
			c.content_fingerprint()
		};

		assert_ne!(baseline, fp(|c| c.uuid = Uuid::from_u128(9)));
		assert_ne!(baseline, fp(|c| c.parent = Uuid::from_u128(9)));
		assert_ne!(baseline, fp(|c| c.name = Cow::Borrowed("renamed.txt")));
		assert_ne!(baseline, fp(|c| c.size = 1));
		assert_ne!(baseline, fp(|c| c.mime = Cow::Borrowed("application/json")));
		assert_ne!(baseline, fp(|c| c.last_modified = dt(1_701_000_000_000)));
		assert_ne!(baseline, fp(|c| c.created = None));
		assert_ne!(baseline, fp(|c| c.created = Some(dt(1_698_000_000_000))));
		assert_ne!(baseline, fp(|c| c.favorited = true));
		assert_ne!(baseline, fp(|c| c.chunks = 99));
		assert_ne!(
			baseline,
			fp(|c| c.hash = Some(Blake3Hash::from(blake3::hash(b"data"))))
		);
		assert_ne!(
			baseline,
			fp(|c| c.key =
				FileKey::from_str_with_version(&"d".repeat(32), FileEncryptionVersion::V2)
					.unwrap())
		);
	}

	#[test]
	fn fingerprint_is_deterministic() {
		let a = base();
		assert_eq!(a.content_fingerprint(), a.clone().content_fingerprint());
	}
}
