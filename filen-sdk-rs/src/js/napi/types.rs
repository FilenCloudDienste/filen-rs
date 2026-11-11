//! napi mirrors of the filen-types items that the file and directory types hold: filen-types
//! derives their uniffi types itself and has no napi build, so they are written here, in the
//! shape ubrn prints them.

use std::borrow::Cow;

use filen_types::{
	auth::FileEncryptionVersion,
	crypto::{EncryptedStringStatic, MaybeEncrypted, MaybeEncryptedStatic},
	fs::{ParentUuid, Uuid},
};
use napi_derive::napi;

use super::{FromWire, IntoWire, Wire};
use crate::Error;

/// Where an item sits: a directory, the trash, or one of the drive's virtual listings.
#[napi(discriminant = "tag", js_name = "ParentUuid")]
pub enum ParentUuidNapi {
	Uuid {
		#[napi(ts_type = "[string]")]
		inner: (String,),
	},
	/// A trashed item's parent. Carries the item's *original* parent so a trashed file/dir
	/// remembers where it came from.
	Trash {
		#[napi(ts_type = "[string]")]
		inner: (String,),
	},
	Recents,
	Favorites,
	Links,
}

#[napi(string_enum, js_name = "ParentUuid_Tags")]
pub enum ParentUuidTags {
	Uuid,
	Trash,
	Recents,
	Favorites,
	Links,
}

#[deny(unsafe_code)]
impl Wire for ParentUuid {
	type Wire = ParentUuidNapi;
}

#[deny(unsafe_code)]
impl FromWire for ParentUuid {
	fn from_wire(wire: ParentUuidNapi) -> Result<Self, Error> {
		Ok(match wire {
			ParentUuidNapi::Uuid { inner: (uuid,) } => Self::Uuid(Uuid::from_wire(uuid)?),
			ParentUuidNapi::Trash { inner: (uuid,) } => Self::Trash(Uuid::from_wire(uuid)?),
			ParentUuidNapi::Recents => Self::Recents,
			ParentUuidNapi::Favorites => Self::Favorites,
			ParentUuidNapi::Links => Self::Links,
		})
	}
}

#[deny(unsafe_code)]
impl IntoWire for ParentUuid {
	fn into_wire(self) -> ParentUuidNapi {
		match self {
			Self::Uuid(uuid) => ParentUuidNapi::Uuid {
				inner: (uuid.into_wire(),),
			},
			Self::Trash(uuid) => ParentUuidNapi::Trash {
				inner: (uuid.into_wire(),),
			},
			Self::Recents => ParentUuidNapi::Recents,
			Self::Favorites => ParentUuidNapi::Favorites,
			Self::Links => ParentUuidNapi::Links,
		}
	}
}

/// The encryption version of a file's contents, numbered as the API numbers it.
#[napi(js_name = "FileEncryptionVersion")]
pub enum FileEncryptionVersionNapi {
	V1 = 1,
	V2 = 2,
	V3 = 3,
}

#[deny(unsafe_code)]
impl Wire for FileEncryptionVersion {
	type Wire = FileEncryptionVersionNapi;
}

#[deny(unsafe_code)]
impl FromWire for FileEncryptionVersion {
	fn from_wire(wire: FileEncryptionVersionNapi) -> Result<Self, Error> {
		Ok(match wire {
			FileEncryptionVersionNapi::V1 => Self::V1,
			FileEncryptionVersionNapi::V2 => Self::V2,
			FileEncryptionVersionNapi::V3 => Self::V3,
		})
	}
}

#[deny(unsafe_code)]
impl IntoWire for FileEncryptionVersion {
	fn into_wire(self) -> FileEncryptionVersionNapi {
		match self {
			Self::V1 => FileEncryptionVersionNapi::V1,
			Self::V2 => FileEncryptionVersionNapi::V2,
			Self::V3 => FileEncryptionVersionNapi::V3,
		}
	}
}

/// A string that is decrypted, or still encrypted where it could not be: uniffi's
/// `MaybeEncryptedUniffi`, which it crosses a `MaybeEncryptedStatic` as.
#[napi(discriminant = "tag", js_name = "MaybeEncryptedUniffi")]
pub enum MaybeEncryptedUniffiNapi {
	Decrypted {
		#[napi(ts_type = "[string]")]
		inner: (String,),
	},
	Encrypted {
		#[napi(ts_type = "[string]")]
		inner: (String,),
	},
}

#[napi(string_enum, js_name = "MaybeEncryptedUniffi_Tags")]
pub enum MaybeEncryptedUniffiTags {
	Decrypted,
	Encrypted,
}

#[deny(unsafe_code)]
impl Wire for MaybeEncryptedStatic {
	type Wire = MaybeEncryptedUniffiNapi;
}

#[deny(unsafe_code)]
impl FromWire for MaybeEncryptedStatic {
	fn from_wire(wire: MaybeEncryptedUniffiNapi) -> Result<Self, Error> {
		Ok(match wire {
			MaybeEncryptedUniffiNapi::Decrypted {
				inner: (decrypted,),
			} => MaybeEncrypted::Decrypted(Cow::Owned(decrypted)),
			MaybeEncryptedUniffiNapi::Encrypted {
				inner: (encrypted,),
			} => MaybeEncrypted::Encrypted(EncryptedStringStatic::from_wire(encrypted)?),
		})
	}
}

#[deny(unsafe_code)]
impl IntoWire for MaybeEncryptedStatic {
	fn into_wire(self) -> MaybeEncryptedUniffiNapi {
		match self {
			MaybeEncrypted::Decrypted(decrypted) => MaybeEncryptedUniffiNapi::Decrypted {
				inner: (decrypted.into_owned(),),
			},
			MaybeEncrypted::Encrypted(encrypted) => MaybeEncryptedUniffiNapi::Encrypted {
				inner: (encrypted.into_wire(),),
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use chrono::{DateTime, Utc};
	use filen_types::{
		api::v3::dir::{color::DirColor, link::info::LinkPasswordSalt},
		auth::MetaEncryptionVersion,
		crypto::Blake3Hash,
		fs::StableUuid,
	};
	use napi::bindgen_prelude::{BigInt, Uint8Array};

	use super::*;
	use crate::{
		ErrorKind,
		auth::MetaKey,
		connect::{
			PasswordState,
			fs::{ShareInfo, SharedDirectory, SharingRole},
		},
		crypto::file::FileKey,
		fs::{
			dir::{RootDirectoryWithMeta, meta::DirectoryMeta},
			file::meta::{DecryptedFileMeta, FileMeta},
		},
		io::{RemoteDirectory, RemoteFile},
		js::{
			AnyDirWithContext, AnyLinkedDir, AnyLinkedDirWithContext, AnyNormalDir, AnySharedDir,
			AnySharedDirWithContext, Dir, DirPublicLink, File, LinkedRootDir, SharedDir,
		},
	};

	fn date(millis: i64) -> DateTime<Utc> {
		DateTime::from_timestamp_millis(millis).unwrap()
	}

	fn file(meta: FileMeta<'static>) -> File {
		File::from(RemoteFile::from_meta(
			Uuid::from_u128(1),
			StableUuid::new_for_test(Uuid::from_u128(2)),
			ParentUuid::Uuid(Uuid::from_u128(3)),
			u64::MAX,
			u64::MAX,
			"de-1",
			"bucket",
			date(1_700_000_000_123),
			true,
			meta,
		))
	}

	fn round_trip<T: FromWire + IntoWire + Clone>(value: &T) -> T {
		T::from_wire(value.clone().into_wire()).unwrap()
	}

	fn remote_dir() -> RemoteDirectory {
		RemoteDirectory::from_meta(
			Uuid::from_u128(4),
			ParentUuid::Uuid(Uuid::from_u128(5)),
			DirColor::Blue,
			false,
			date(1_700_000_000_000),
			DirectoryMeta::DecryptedUTF8(Cow::Borrowed("not json")),
		)
	}

	#[test]
	fn a_file_round_trips_through_its_wire_form() {
		let file = file(FileMeta::Decoded(DecryptedFileMeta {
			name: "a.txt".into(),
			size: u64::MAX,
			mime: "text/plain".into(),
			key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3)
				.unwrap(),
			last_modified: date(1_600_000_000_000),
			created: Some(date(1_500_000_000_000)),
			hash: Some(Blake3Hash::from([7; 32])),
		}));
		assert_eq!(round_trip(&file), file);
	}

	#[test]
	fn a_file_with_raw_meta_keeps_its_bytes() {
		let file = file(FileMeta::DecryptedRaw(Cow::Owned(vec![0, 1, 255])));
		assert_eq!(round_trip(&file), file);
	}

	#[test]
	fn each_any_dir_with_context_category_round_trips() {
		type Wire = <AnyDirWithContext as super::Wire>::Wire;
		type NormalWire = <AnyNormalDir as super::Wire>::Wire;
		let dir = Dir::from(remote_dir());
		let normal = Wire::Normal {
			inner: (NormalWire::Dir {
				inner: (dir.clone().into_wire(),),
			},),
		};
		assert_eq!(
			AnyDirWithContext::from_wire(normal).unwrap(),
			AnyDirWithContext::Normal(AnyNormalDir::Dir(dir))
		);

		let shared = AnySharedDirWithContext {
			dir: AnySharedDir::Dir(SharedDir::from(SharedDirectory {
				inner: remote_dir(),
			})),
			share_info: SharingRole::Receiver(ShareInfo {
				email: "a@example.com".to_owned(),
				id: u64::MAX,
			}),
		};
		let wire = Wire::Shared {
			inner: (shared.clone().into_wire(),),
		};
		assert_eq!(
			AnyDirWithContext::from_wire(wire).unwrap(),
			AnyDirWithContext::Shared(shared)
		);

		let linked = AnyLinkedDirWithContext {
			dir: AnyLinkedDir::Root(LinkedRootDir::from(RootDirectoryWithMeta::from_meta(
				Uuid::from_u128(6),
				DirColor::Default,
				date(1_700_000_000_000),
				DirectoryMeta::DecryptedRaw(Cow::Owned(vec![9])),
			))),
			link: DirPublicLink::from(crate::connect::DirPublicLink {
				link_uuid: Uuid::from_u128(7),
				link_key: MetaKey::from_str_and_version(
					"0123456789abcdefghijklmnopqrstuv",
					MetaEncryptionVersion::V2,
				)
				.unwrap(),
				password: PasswordState::Known("hunter2".to_owned()),
				enable_download: true,
				salt: LinkPasswordSalt::None,
			}),
		};
		let wire = Wire::Linked {
			inner: (linked.clone().into_wire(),),
		};
		assert_eq!(
			AnyDirWithContext::from_wire(wire).unwrap(),
			AnyDirWithContext::Linked(linked)
		);
	}

	#[test]
	fn a_trashed_parent_keeps_its_original_parent() {
		let parent = ParentUuid::Trash(Uuid::from_u128(8));
		assert!(matches!(
			parent.into_wire(),
			ParentUuidNapi::Trash { inner: (ref uuid,) }
				if uuid == "00000000-0000-0000-0000-000000000008"
		));
		assert_eq!(round_trip(&parent), parent);
	}

	#[test]
	fn file_encryption_version_writes_1_2_3() {
		let numbers = [
			FileEncryptionVersion::V1,
			FileEncryptionVersion::V2,
			FileEncryptionVersion::V3,
		]
		.map(|version| version.into_wire() as i32);
		assert_eq!(numbers, [1, 2, 3]);
	}

	#[test]
	fn a_date_time_past_chrono_range_is_a_conversion_error() {
		assert_eq!(
			DateTime::<Utc>::from_wire(i64::MAX.into_wire()).map_err(|e| e.kind()),
			Err(ErrorKind::Conversion)
		);
		assert_eq!(
			DateTime::<Utc>::from_wire(BigInt::from(1_700_000_000_123_i64)).ok(),
			Some(date(1_700_000_000_123))
		);
	}

	#[test]
	fn a_stable_uuid_that_is_not_a_uuid_is_a_conversion_error() {
		assert_eq!(
			StableUuid::from_wire("not-a-uuid".to_owned()).map_err(|e| e.kind()),
			Err(ErrorKind::Conversion)
		);
	}

	#[test]
	fn a_blake3_hash_of_31_bytes_is_a_conversion_error() {
		assert_eq!(
			Blake3Hash::from_wire(Uint8Array::new(vec![0; 31])).map_err(|e| e.kind()),
			Err(ErrorKind::Conversion)
		);
	}
}
