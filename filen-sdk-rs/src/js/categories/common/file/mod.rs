use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::fs::{ParentUuid, StableUuid, Uuid};

pub(crate) mod meta;
pub(crate) mod version;

use meta::FileMeta;

use crate::{
	crypto::error::ConversionError,
	io::{AnonymousRemoteFile, RemoteFile},
	thumbnail::is_supported_thumbnail_mime,
};

#[js_type(import, export, wasm_all)]
pub struct File {
	pub(crate) uuid: Uuid,
	/// Server-minted whole-life file id — survives content edits and version
	/// restores, unlike `uuid` which identifies the current version. Persist
	/// this as the file's identity, never `uuid`.
	///
	/// Absent (`undefined` in the generated TS, not `null`) for files listed
	/// from a public link or from a shared-in folder:
	/// those surfaces do not report a stable id at all (see
	/// [`AnonymousRemoteFile`](crate::fs::file::AnonymousRemoteFile)). Such a
	/// file can be read (download, zip, thumbnail, media URL) but cannot be
	/// handed back to an operation on the normal drive, which needs the
	/// identity.
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		serde(rename = "stableUUID")
	)]
	pub(crate) stable_uuid: Option<StableUuid>,
	pub(crate) meta: FileMeta,

	parent: ParentUuid,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	size: u64,
	favorited: bool,

	region: String,
	bucket: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint"),
		serde(with = "chrono::serde::ts_milliseconds")
	)]
	timestamp: DateTime<Utc>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	chunks: u64,
	// JS only field, indicates if the file can have a thumbnail generated
	// this is here to avoid having to call into WASM to check mime types
	can_make_thumbnail: bool,
}

impl From<RemoteFile> for File {
	fn from(file: RemoteFile) -> Self {
		Self::new(Some(file.stable_uuid), file)
	}
}

impl From<AnonymousRemoteFile> for File {
	fn from(file: AnonymousRemoteFile) -> Self {
		Self::new(None, file)
	}
}

impl File {
	fn new<Id>(stable_uuid: Option<StableUuid>, file: RemoteFile<Id>) -> Self {
		let meta = file.meta.into();
		File {
			can_make_thumbnail: if let FileMeta::Decoded(meta) = &meta {
				is_supported_thumbnail_mime(&meta.mime)
			} else {
				false
			},
			uuid: file.uuid,
			stable_uuid,
			meta,
			parent: file.parent,
			size: file.size,
			favorited: file.favorited,
			region: file.region,
			bucket: file.bucket,
			timestamp: file.timestamp,
			chunks: file.chunks,
		}
	}

	fn into_remote<Id>(self, stable_uuid: Id) -> Result<RemoteFile<Id>, ConversionError> {
		Ok(RemoteFile {
			uuid: self.uuid,
			stable_uuid,
			meta: self.meta.try_into()?,
			parent: self.parent,
			size: self.size,
			favorited: self.favorited,
			region: self.region,
			bucket: self.bucket,
			timestamp: self.timestamp,
			chunks: self.chunks,
		})
	}
}

impl TryFrom<File> for RemoteFile {
	type Error = ConversionError;
	fn try_from(file: File) -> Result<Self, Self::Error> {
		// Drive operations act on a file's identity, so a file that came from a
		// surface which reports none cannot be one of their targets. Refusing
		// here is the whole point of the split: there is no uuid to stand in.
		let stable_uuid = file.stable_uuid.ok_or(ConversionError::MissingStableUuid)?;
		file.into_remote(stable_uuid)
	}
}

impl TryFrom<File> for AnonymousRemoteFile {
	type Error = ConversionError;
	fn try_from(file: File) -> Result<Self, Self::Error> {
		file.into_remote(())
	}
}

#[cfg(test)]
mod tests {
	use chrono::Utc;
	use filen_types::auth::FileEncryptionVersion;

	use super::*;
	use crate::crypto::{error::ConversionError, file::FileKey};
	use crate::fs::file::meta::{DecryptedFileMeta, FileMeta as RsFileMeta};

	fn file_with_id<Id>(id: Id) -> RemoteFile<Id> {
		let now = Utc::now();
		RemoteFile::from_meta(
			Uuid::new_v4(),
			id,
			ParentUuid::Uuid(Uuid::new_v4()),
			1,
			1,
			"eu-central-1",
			"bucket",
			now,
			false,
			RsFileMeta::Decoded(DecryptedFileMeta {
				name: "a.txt".into(),
				size: 1,
				mime: "text/plain".into(),
				key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3)
					.unwrap(),
				last_modified: now,
				created: Some(now),
				hash: None,
			}),
		)
	}

	/// The single runtime guard between an anonymous (link/shared-listed) file
	/// and a normal-drive operation. A regression to `expect`/`unwrap` here
	/// would turn a catchable FFI error into a mobile crash.
	#[test]
	fn an_anonymous_file_cannot_become_a_drive_file() {
		let file = File::from(file_with_id(()));
		let converted: Result<RemoteFile, _> = file.try_into();
		match converted {
			Err(ConversionError::MissingStableUuid) => {}
			other => panic!("expected MissingStableUuid, got {other:?}"),
		}
	}

	#[test]
	fn a_drive_file_round_trips_through_the_js_shape() {
		let drive = file_with_id(StableUuid::new_for_test(Uuid::new_v4()));
		let back = RemoteFile::try_from(File::from(drive.clone())).unwrap();
		assert_eq!(back, drive);
		// The anonymous read path stays open for anonymous files.
		AnonymousRemoteFile::try_from(File::from(file_with_id(()))).unwrap();
	}
}
