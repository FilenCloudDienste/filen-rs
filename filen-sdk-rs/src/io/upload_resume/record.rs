//! What a journal records about an upload in progress, and how it is sealed under the account's
//! metadata crypter before a store sees it.

use std::{borrow::Cow, fmt};

use chrono::{DateTime, SubsecRound as _, Utc};
use filen_types::{
	auth::FileEncryptionVersion, crypto::EncryptedString, fs::Uuid, serde::str::Base64EncodedBytes,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
	policy::SourceStamp,
	store::{SealedUploadJournal, UploadJournalId},
};
use crate::{
	Error, ErrorKind,
	crypto::{error::ConversionError, file::FileKey, shared::MetaCrypter},
	fs::file::{BaseFile, write::RemoteFileInfo},
};

/// The record format this module writes and reads.
const RECORD_VERSION: u32 = 1;

/// Which chunks of an upload the server acknowledged: one bit per chunk, set once the chunk's
/// response arrived. Chunks finish out of order, so this is a set, not a count.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AckedChunks {
	/// Bit `i % 8` of byte `i / 8` is chunk `i`. Exactly `chunks.div_ceil(8)` bytes, with no bit
	/// set past the last chunk.
	bits: Vec<u8>,
	chunks: u64,
}

/// A bitmap that cannot be the acknowledged chunks of an upload of its size.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum BitmapError {
	#[error("a bitmap of {chunks} chunks takes {expected} bytes, not {actual}")]
	WrongLength {
		chunks: u64,
		expected: u64,
		actual: usize,
	},
	#[error("a bitmap of {chunks} chunks has a bit set past the last chunk")]
	BitPastLastChunk { chunks: u64 },
	#[error("a bitmap of {chunks} chunks does not fit in memory")]
	TooLarge { chunks: u64 },
}

impl AckedChunks {
	/// No chunk acknowledged, of an upload of `chunks` chunks.
	pub(crate) fn new(chunks: u64) -> Result<Self, BitmapError> {
		let len =
			usize::try_from(chunks.div_ceil(8)).map_err(|_| BitmapError::TooLarge { chunks })?;
		Ok(Self {
			bits: vec![0; len],
			chunks,
		})
	}

	/// The acknowledged chunks of an upload of `chunks` chunks from their bitmap.
	fn from_bytes(bits: Vec<u8>, chunks: u64) -> Result<Self, BitmapError> {
		let expected = chunks.div_ceil(8);
		if u64::try_from(bits.len()).ok() != Some(expected) {
			return Err(BitmapError::WrongLength {
				chunks,
				expected,
				actual: bits.len(),
			});
		}
		// The bits of the last byte past the last chunk. A whole last byte has none.
		let used_in_last = chunks % 8;
		if used_in_last != 0
			&& let Some(last) = bits.last()
			&& last >> used_in_last != 0
		{
			return Err(BitmapError::BitPastLastChunk { chunks });
		}
		Ok(Self { bits, chunks })
	}

	/// The byte holding chunk `idx`'s bit and the bit's mask, if the upload has that chunk.
	fn slot(&self, idx: u64) -> Option<(usize, u8)> {
		if idx >= self.chunks {
			return None;
		}
		let byte = usize::try_from(idx / 8).ok()?;
		Some((byte, 1 << (idx % 8)))
	}

	/// Records chunk `idx` as acknowledged. An index past the last chunk is a bug in the caller.
	pub(crate) fn insert(&mut self, idx: u64) -> Result<(), Error> {
		let chunks = self.chunks;
		let byte = self
			.slot(idx)
			.and_then(|(byte, mask)| Some((self.bits.get_mut(byte)?, mask)));
		let Some((byte, mask)) = byte else {
			return Err(Error::custom(
				ErrorKind::Internal,
				format!("chunk {idx} is past the last of an upload of {chunks} chunks"),
			));
		};
		*byte |= mask;
		Ok(())
	}

	/// Whether chunk `idx` was acknowledged.
	pub(crate) fn contains(&self, idx: u64) -> bool {
		self.slot(idx)
			.and_then(|(byte, mask)| Some(self.bits.get(byte)? & mask != 0))
			.unwrap_or(false)
	}

	/// How many chunks were acknowledged.
	pub(crate) fn count(&self) -> u64 {
		self.bits
			.iter()
			.map(|byte| u64::from(byte.count_ones()))
			.sum()
	}

	/// How many bytes of a source of `size` bytes, in chunks of `chunk_size`, the acknowledged
	/// chunks hold: whole chunks, and the last one as long as it is. `None` when `size` and
	/// `chunk_size` do not make this many chunks, or the sum overflows.
	pub(crate) fn acked_bytes(&self, size: u64, chunk_size: u64) -> Option<u64> {
		if chunk_count(size, chunk_size)? != self.chunks {
			return None;
		}
		let Some(last) = self.chunks.checked_sub(1) else {
			return Some(0);
		};
		let last_len = size.checked_sub(last.checked_mul(chunk_size)?)?;
		let whole = self.count() - u64::from(self.contains(last));
		let last_acked = if self.contains(last) { last_len } else { 0 };
		whole.checked_mul(chunk_size)?.checked_add(last_acked)
	}

	fn as_bytes(&self) -> &[u8] {
		&self.bits
	}
}

impl fmt::Debug for AckedChunks {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("AckedChunks")
			.field("acked", &self.count())
			.field("chunks", &self.chunks)
			.finish()
	}
}

/// The journal of one upload in progress: everything needed to finish it in another process,
/// together with what the source looked like, so that a changed source is not resumed.
///
/// It is the resuming upload's own state, updated as chunks are acknowledged, and only reaches a
/// store sealed ([`seal`]). Decoding goes through [`open`], which checks it.
#[derive(Clone)]
pub(crate) struct UploadJournalRecord {
	id: UploadJournalId,
	uuid: Uuid,
	parent: Uuid,
	/// As given: the sync engine uploads names it holds without validating them.
	name: String,
	mime: String,
	key: FileKey,
	upload_key: String,
	chunk_size: u64,
	source: SourceStamp,
	/// The times sent in the file's metadata, before EXIF, to the millisecond.
	created: DateTime<Utc>,
	modified: DateTime<Utc>,
	acked: AckedChunks,
	/// When the first chunk was acknowledged, to the millisecond. `None` until then.
	first_ack_at: Option<DateTime<Utc>>,
	/// Wall time spent sending chunks, summed over every pass, in milliseconds.
	active_millis: u64,
	/// Where the server stores the chunks, from the first chunk response.
	stored_at: Option<RemoteFileInfo>,
}

impl UploadJournalRecord {
	/// A record of the upload of `file` from a source looking like `source`, under `upload_key`,
	/// in chunks of `chunk_size`, with no chunk acknowledged yet.
	pub(crate) fn new(
		id: UploadJournalId,
		file: &BaseFile,
		upload_key: String,
		source: SourceStamp,
		chunk_size: u64,
	) -> Result<Self, Error> {
		let chunks = chunk_count(source.size, chunk_size).ok_or_else(|| {
			Error::custom(ErrorKind::Internal, "an upload's chunk size cannot be zero")
		})?;
		Ok(Self {
			id,
			uuid: file.uuid(),
			parent: file.parent(),
			name: file.name().to_owned(),
			mime: file.mime().to_owned(),
			key: *file.key(),
			upload_key,
			chunk_size,
			source,
			created: file.created().trunc_subsecs(3),
			modified: file.last_modified().trunc_subsecs(3),
			acked: AckedChunks::new(chunks)
				.map_err(|e| Error::custom_with_source(ErrorKind::Internal, e, None::<&str>))?,
			first_ack_at: None,
			active_millis: 0,
			stored_at: None,
		})
	}

	/// Records chunk `idx` as acknowledged at `now`, stored at `info`.
	pub(crate) fn ack(
		&mut self,
		idx: u64,
		info: RemoteFileInfo,
		now: DateTime<Utc>,
	) -> Result<(), Error> {
		self.acked.insert(idx)?;
		self.first_ack_at.get_or_insert(now.trunc_subsecs(3));
		self.stored_at.get_or_insert(info);
		Ok(())
	}

	/// Adds `millis` of wall time spent sending.
	pub(crate) fn add_active_millis(&mut self, millis: u64) {
		self.active_millis = self.active_millis.saturating_add(millis);
	}

	pub(crate) fn uuid(&self) -> Uuid {
		self.uuid
	}

	pub(crate) fn parent(&self) -> Uuid {
		self.parent
	}

	pub(crate) fn name(&self) -> &str {
		&self.name
	}

	pub(crate) fn mime(&self) -> &str {
		&self.mime
	}

	pub(crate) fn key(&self) -> &FileKey {
		&self.key
	}

	pub(crate) fn upload_key(&self) -> &str {
		&self.upload_key
	}

	pub(crate) fn chunk_size(&self) -> u64 {
		self.chunk_size
	}

	pub(crate) fn source(&self) -> SourceStamp {
		self.source
	}

	pub(crate) fn times(&self) -> (DateTime<Utc>, DateTime<Utc>) {
		(self.created, self.modified)
	}

	pub(crate) fn acked(&self) -> &AckedChunks {
		&self.acked
	}

	pub(crate) fn first_ack_at(&self) -> Option<DateTime<Utc>> {
		self.first_ack_at
	}

	pub(crate) fn active_millis(&self) -> u64 {
		self.active_millis
	}

	pub(crate) fn stored_at(&self) -> Option<&RemoteFileInfo> {
		self.stored_at.as_ref()
	}

	/// The bytes of the source the acknowledged chunks hold, `None` if the record's sizes do not
	/// add up.
	pub(crate) fn acked_bytes(&self) -> Option<u64> {
		self.acked.acked_bytes(self.source.size, self.chunk_size)
	}
}

// Prints neither key: the upload key and the file key are what another client needs to finish
// or read the upload.
impl fmt::Debug for UploadJournalRecord {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("UploadJournalRecord")
			.field("id", &self.id)
			.field("uuid", &self.uuid)
			.field("parent", &self.parent)
			.field("chunk_size", &self.chunk_size)
			.field("source", &self.source)
			.field("acked", &self.acked)
			.field("first_ack_at", &self.first_ack_at)
			.field("active_millis", &self.active_millis)
			.finish_non_exhaustive()
	}
}

/// A record as it is serialized inside the encryption: `{"v":1,...}`, camelCase. Unvalidated;
/// [`open`] turns it into an [`UploadJournalRecord`].
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireRecord<'a> {
	v: u32,
	#[serde(borrow)]
	id: Cow<'a, str>,
	uuid: Uuid,
	parent: Uuid,
	#[serde(borrow)]
	name: Cow<'a, str>,
	#[serde(borrow)]
	mime: Cow<'a, str>,
	#[serde(borrow)]
	file_key: Cow<'a, str>,
	file_key_version: FileEncryptionVersion,
	#[serde(borrow)]
	upload_key: Cow<'a, str>,
	chunk_size: u64,
	source_size: u64,
	source_mtime_millis: i64,
	source_inode: Option<u64>,
	created_millis: i64,
	modified_millis: i64,
	#[serde(borrow)]
	acked: Base64EncodedBytes<'a>,
	first_ack_at_millis: i64,
	active_millis: u64,
	#[serde(borrow)]
	region: Option<Cow<'a, str>>,
	#[serde(borrow)]
	bucket: Option<Cow<'a, str>>,
}

/// Only the version, read first so that a record of another version is told apart from a
/// malformed one.
#[derive(Deserialize)]
struct WireVersion {
	v: u32,
}

/// Why a sealed journal could not be opened as a record. The journal is then not resumed.
#[derive(Debug, Error)]
pub(crate) enum UnreadableJournal {
	#[error("the journal does not decrypt under this account's keys: {0}")]
	Decrypt(#[source] ConversionError),
	#[error("the journal is malformed: {0}")]
	Malformed(#[source] serde_json::Error),
	#[error("unknown journal record version {0}")]
	UnknownVersion(u32),
	#[error("the journal belongs to another upload")]
	WrongId,
	#[error("the journal's file key version {0:?} cannot encrypt an upload")]
	UnsupportedKeyVersion(FileEncryptionVersion),
	#[error("the journal's file key is malformed: {0}")]
	Key(#[source] ConversionError),
	#[error("the journal's acknowledged chunks are malformed: {0}")]
	Bitmap(#[source] BitmapError),
	#[error("time {0} ms is out of range")]
	TimeOutOfRange(i64),
	#[error("the journal's start time differs from its record's")]
	StartedAtMismatch,
	#[error("the journal names a region or a bucket without the other")]
	HalfStoredAt,
	#[error("the journal's chunk size is zero")]
	ZeroChunkSize,
}

/// Encrypts `record` with `crypter`, the account's metadata crypter. A record with no chunk
/// acknowledged yet has nothing worth keeping and is refused.
pub(crate) async fn seal(
	record: &UploadJournalRecord,
	crypter: &impl MetaCrypter,
) -> Result<SealedUploadJournal, Error> {
	let Some(started_at) = record.first_ack_at else {
		return Err(Error::custom(
			ErrorKind::Internal,
			"an upload journal is kept only once a chunk was acknowledged",
		));
	};
	// Built and dropped before the await: the file key's string form is a deeply nested generic
	// type, and a future holding it across an await takes a caller's `Send` proof past the
	// compiler's recursion limit.
	let json = {
		let file_key = record.key.to_str();
		let wire = WireRecord {
			v: RECORD_VERSION,
			id: Cow::Owned(record.id.to_hex()),
			uuid: record.uuid,
			parent: record.parent,
			name: Cow::Borrowed(&record.name),
			mime: Cow::Borrowed(&record.mime),
			file_key: Cow::Borrowed(file_key.as_ref()),
			file_key_version: record.key.version(),
			upload_key: Cow::Borrowed(&record.upload_key),
			chunk_size: record.chunk_size,
			source_size: record.source.size,
			source_mtime_millis: record.source.mtime_millis,
			source_inode: record.source.inode,
			created_millis: record.created.timestamp_millis(),
			modified_millis: record.modified.timestamp_millis(),
			acked: Base64EncodedBytes::from(record.acked.as_bytes()),
			first_ack_at_millis: started_at.timestamp_millis(),
			active_millis: record.active_millis,
			region: record
				.stored_at
				.as_ref()
				.map(|info| Cow::Borrowed(info.region.as_str())),
			bucket: record
				.stored_at
				.as_ref()
				.map(|info| Cow::Borrowed(info.bucket.as_str())),
		};
		serde_json::to_string(&wire)
			.expect("a journal record is strings, numbers and a byte string (should be impossible)")
	};
	let sealed = crypter.encrypt_meta(&json).await;
	Ok(SealedUploadJournal::new(started_at, sealed.0.into_owned()))
}

/// Decrypts `sealed`, kept under `id`, with `crypter`, and checks it is the record of that upload.
pub(crate) async fn open(
	sealed: &SealedUploadJournal,
	id: &UploadJournalId,
	crypter: &impl MetaCrypter,
) -> Result<UploadJournalRecord, UnreadableJournal> {
	let encrypted = EncryptedString(Cow::Borrowed(sealed.sealed()));
	let json = crypter
		.decrypt_meta(&encrypted)
		.await
		.map_err(UnreadableJournal::Decrypt)?;
	let version = serde_json::from_str::<WireVersion>(&json)
		.map_err(UnreadableJournal::Malformed)?
		.v;
	if version != RECORD_VERSION {
		return Err(UnreadableJournal::UnknownVersion(version));
	}
	let wire =
		serde_json::from_str::<WireRecord<'_>>(&json).map_err(UnreadableJournal::Malformed)?;
	let record = from_wire(wire, id)?;
	if record.first_ack_at != Some(sealed.started_at()) {
		return Err(UnreadableJournal::StartedAtMismatch);
	}
	Ok(record)
}

fn from_wire(
	wire: WireRecord<'_>,
	id: &UploadJournalId,
) -> Result<UploadJournalRecord, UnreadableJournal> {
	if wire.id != id.to_hex() {
		return Err(UnreadableJournal::WrongId);
	}
	match wire.file_key_version {
		FileEncryptionVersion::V2 | FileEncryptionVersion::V3 => {}
		// V1 keys only decrypt: nothing is uploaded under one
		FileEncryptionVersion::V1 => {
			return Err(UnreadableJournal::UnsupportedKeyVersion(
				FileEncryptionVersion::V1,
			));
		}
	}
	let key = FileKey::from_str_with_version(&wire.file_key, wire.file_key_version)
		.map_err(UnreadableJournal::Key)?;
	let chunks =
		chunk_count(wire.source_size, wire.chunk_size).ok_or(UnreadableJournal::ZeroChunkSize)?;
	let acked = AckedChunks::from_bytes(wire.acked.into_bytes(), chunks)
		.map_err(UnreadableJournal::Bitmap)?;
	let stored_at = match (wire.region, wire.bucket) {
		(Some(region), Some(bucket)) => Some(RemoteFileInfo {
			region: region.into_owned(),
			bucket: bucket.into_owned(),
		}),
		(None, None) => None,
		(Some(_), None) | (None, Some(_)) => return Err(UnreadableJournal::HalfStoredAt),
	};
	Ok(UploadJournalRecord {
		id: *id,
		uuid: wire.uuid,
		parent: wire.parent,
		name: wire.name.into_owned(),
		mime: wire.mime.into_owned(),
		key,
		upload_key: wire.upload_key.into_owned(),
		chunk_size: wire.chunk_size,
		source: SourceStamp {
			size: wire.source_size,
			mtime_millis: wire.source_mtime_millis,
			inode: wire.source_inode,
		},
		created: time(wire.created_millis)?,
		modified: time(wire.modified_millis)?,
		acked,
		first_ack_at: Some(time(wire.first_ack_at_millis)?),
		active_millis: wire.active_millis,
		stored_at,
	})
}

fn time(millis: i64) -> Result<DateTime<Utc>, UnreadableJournal> {
	DateTime::from_timestamp_millis(millis).ok_or(UnreadableJournal::TimeOutOfRange(millis))
}

/// How many chunks of `chunk_size` bytes a source of `size` bytes takes, the last one short.
/// `None` for a chunk size of zero.
pub(crate) fn chunk_count(size: u64, chunk_size: u64) -> Option<u64> {
	size.checked_div(chunk_size)?;
	Some(size.div_ceil(chunk_size))
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use chrono::TimeZone as _;

	use super::*;
	use crate::{
		consts::CHUNK_SIZE_U64,
		crypto::v2::{self, MasterKey, MasterKeys},
		fs::{file::RootFile, name::ValidatedName},
	};

	const UPLOAD_KEY: &str = "abcdefghijklmnopqrstuvwxyz012345";
	const FILE_KEY: &str = "0123456789abcdef0123456789abcdef";

	fn master_keys(key: &str) -> MasterKeys {
		MasterKeys::new_from_key(MasterKey::try_from(key.to_owned()).unwrap())
	}

	fn crypter() -> MasterKeys {
		master_keys(&"a1".repeat(32))
	}

	fn id() -> UploadJournalId {
		UploadJournalId::new(
			Path::new("/home/user/video.mp4"),
			Uuid::from_u128(1),
			"video.mp4",
		)
	}

	fn file() -> BaseFile {
		BaseFile {
			root: RootFile {
				uuid: Uuid::from_u128(7),
				name: ValidatedName::try_from("video.mp4").unwrap(),
				mime: "video/mp4".to_owned(),
				key: FileKey::V2(FILE_KEY.parse::<v2::FileKey>().unwrap()),
				created: Utc.timestamp_millis_opt(1_500_000_000_123).unwrap(),
				modified: Utc.timestamp_millis_opt(1_600_000_000_456).unwrap(),
			},
			parent: Uuid::from_u128(1),
		}
	}

	/// A record of 11 chunks, of which 0, 3 and 10 were acknowledged.
	fn record() -> UploadJournalRecord {
		let source = SourceStamp {
			size: CHUNK_SIZE_U64 * 10 + 123,
			mtime_millis: 1_600_000_000_456,
			inode: Some(42),
		};
		let mut record =
			UploadJournalRecord::new(id(), &file(), UPLOAD_KEY.to_owned(), source, CHUNK_SIZE_U64)
				.unwrap();
		let at = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
		let info = RemoteFileInfo {
			region: "de-1".to_owned(),
			bucket: "bucket-7".to_owned(),
		};
		for idx in [3, 0, 10] {
			record.ack(idx, info.clone(), at).unwrap();
		}
		record.add_active_millis(1_234);
		record
	}

	/// The JSON of `record()` with `edit` applied, sealed under `crypter()`.
	async fn sealed_with(edit: impl FnOnce(&mut serde_json::Value)) -> SealedUploadJournal {
		let crypter = crypter();
		let sealed = seal(&record(), &crypter).await.unwrap();
		let json = crypter
			.decrypt_meta(&EncryptedString(Cow::Borrowed(sealed.sealed())))
			.await
			.unwrap();
		let mut value = serde_json::from_str::<serde_json::Value>(&json).unwrap();
		edit(&mut value);
		let resealed = crypter.encrypt_meta(&value.to_string()).await;
		SealedUploadJournal::new(sealed.started_at(), resealed.0.into_owned())
	}

	#[tokio::test]
	async fn a_record_round_trips_under_the_master_key() {
		let crypter = crypter();
		let sealed = seal(&record(), &crypter).await.unwrap();
		assert_eq!(
			sealed.started_at(),
			Utc.timestamp_millis_opt(1_700_000_000_000).unwrap()
		);
		assert!(!sealed.sealed().contains(UPLOAD_KEY));
		let opened = open(&sealed, &id(), &crypter).await.unwrap();
		assert_eq!(opened.uuid(), Uuid::from_u128(7));
		assert_eq!(opened.parent(), Uuid::from_u128(1));
		assert_eq!(opened.name(), "video.mp4");
		assert_eq!(opened.mime(), "video/mp4");
		assert_eq!(opened.key(), file().key());
		assert_eq!(opened.upload_key(), UPLOAD_KEY);
		assert_eq!(opened.chunk_size(), CHUNK_SIZE_U64);
		assert_eq!(opened.source(), record().source());
		assert_eq!(opened.times(), (file().created(), file().last_modified()));
		assert_eq!(opened.acked(), record().acked());
		assert_eq!(opened.first_ack_at(), Some(sealed.started_at()));
		assert_eq!(opened.active_millis(), 1_234);
		let stored_at = opened.stored_at().unwrap();
		assert_eq!(
			(stored_at.region.as_str(), stored_at.bucket.as_str()),
			("de-1", "bucket-7")
		);
	}

	#[tokio::test]
	async fn a_record_sealed_under_another_master_key_does_not_open() {
		let sealed = seal(&record(), &crypter()).await.unwrap();
		let other = master_keys(&"b2".repeat(32));
		assert!(matches!(
			open(&sealed, &id(), &other).await,
			Err(UnreadableJournal::Decrypt(_))
		));
	}

	#[tokio::test]
	async fn a_record_of_an_unknown_version_does_not_open() {
		let sealed = sealed_with(|value| value["v"] = 2.into()).await;
		assert!(matches!(
			open(&sealed, &id(), &crypter()).await,
			Err(UnreadableJournal::UnknownVersion(2))
		));
	}

	#[tokio::test]
	async fn a_record_loaded_under_another_id_does_not_open() {
		let sealed = seal(&record(), &crypter()).await.unwrap();
		let other = UploadJournalId::new(Path::new("/other"), Uuid::from_u128(1), "video.mp4");
		assert!(matches!(
			open(&sealed, &other, &crypter()).await,
			Err(UnreadableJournal::WrongId)
		));
	}

	#[tokio::test]
	async fn a_record_of_a_v1_file_key_does_not_open() {
		let sealed = sealed_with(|value| value["fileKeyVersion"] = 1.into()).await;
		assert!(matches!(
			open(&sealed, &id(), &crypter()).await,
			Err(UnreadableJournal::UnsupportedKeyVersion(
				FileEncryptionVersion::V1
			))
		));
	}

	#[tokio::test]
	async fn a_record_whose_start_differs_from_its_envelope_does_not_open() {
		let sealed = seal(&record(), &crypter()).await.unwrap();
		let moved = SealedUploadJournal::new(
			sealed.started_at() + chrono::TimeDelta::milliseconds(1),
			sealed.sealed().to_owned(),
		);
		assert!(matches!(
			open(&moved, &id(), &crypter()).await,
			Err(UnreadableJournal::StartedAtMismatch)
		));
	}

	#[tokio::test]
	async fn a_bitmap_of_the_wrong_length_or_with_bits_past_the_last_chunk_is_refused() {
		// 11 chunks take 2 bytes
		let sealed = sealed_with(|value| value["acked"] = "CQQE".into()).await;
		assert!(matches!(
			open(&sealed, &id(), &crypter()).await,
			Err(UnreadableJournal::Bitmap(BitmapError::WrongLength {
				chunks: 11,
				expected: 2,
				actual: 3
			}))
		));
		// bit 11 of 0..=10
		let sealed = sealed_with(|value| value["acked"] = "AQg=".into()).await;
		assert!(matches!(
			open(&sealed, &id(), &crypter()).await,
			Err(UnreadableJournal::Bitmap(BitmapError::BitPastLastChunk {
				chunks: 11
			}))
		));
		// bit 10, the last chunk, is fine
		let sealed = sealed_with(|value| value["acked"] = "AQQ=".into()).await;
		assert!(open(&sealed, &id(), &crypter()).await.is_ok());
	}

	#[test]
	fn acked_chunks_take_indices_out_of_order_and_count_them() {
		let mut acked = AckedChunks::new(20).unwrap();
		for idx in [19, 3, 8, 3, 0] {
			acked.insert(idx).unwrap();
		}
		assert_eq!(acked.count(), 4);
		assert_eq!(
			(0..20)
				.filter(|idx| acked.contains(*idx))
				.collect::<Vec<_>>(),
			[0, 3, 8, 19]
		);
		assert_eq!(acked.as_bytes(), [0b0000_1001, 0b0000_0001, 0b0000_1000]);
		assert!(!acked.contains(20));
	}

	#[test]
	fn acked_chunks_refuse_an_index_past_the_last_chunk() {
		let mut acked = AckedChunks::new(9).unwrap();
		acked.insert(8).unwrap();
		assert_eq!(acked.insert(9).unwrap_err().kind(), ErrorKind::Internal);
		assert_eq!(
			acked.insert(u64::MAX).unwrap_err().kind(),
			ErrorKind::Internal
		);
		assert_eq!(acked.count(), 1);
	}

	#[test]
	fn acked_bytes_count_a_short_last_chunk_exactly() {
		let mut acked = AckedChunks::new(3).unwrap();
		assert_eq!(acked.acked_bytes(2 * 100 + 7, 100), Some(0));
		acked.insert(2).unwrap();
		assert_eq!(acked.acked_bytes(2 * 100 + 7, 100), Some(7));
		acked.insert(0).unwrap();
		assert_eq!(acked.acked_bytes(2 * 100 + 7, 100), Some(107));
		acked.insert(1).unwrap();
		assert_eq!(acked.acked_bytes(2 * 100 + 7, 100), Some(207));
		// sizes that do not make 3 chunks
		assert_eq!(acked.acked_bytes(300 + 1, 100), None);
		assert_eq!(acked.acked_bytes(200, 100), None);
		assert_eq!(acked.acked_bytes(207, 0), None);
		// the largest sizes add up without overflowing
		assert_eq!(acked.acked_bytes(u64::MAX, u64::MAX / 2), Some(u64::MAX));
	}

	#[test]
	fn the_record_debug_shows_neither_key() {
		let debug = format!("{:?}", record());
		assert!(!debug.contains(UPLOAD_KEY), "{debug}");
		assert!(!debug.contains(FILE_KEY), "{debug}");
		assert!(debug.contains("UploadJournalRecord"), "{debug}");
	}
}
