use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::{api::v3::user::events::UserEventDeserializeError, fs::UuidStr};

use crate::{
	js::{DirMeta, FileMeta},
	user::events::{
		DecryptedUserEvent as DecryptedUserEventRs,
		DecryptedUserEventKind as DecryptedUserEventKindRs,
	},
};

/// JS-facing result for a single event in a `events()` response — `Ok` for
/// successfully-parsed events, `Err` for events the SDK couldn't decode
/// (unknown variant, missing field, etc).
#[js_type(export, no_deser, tagged)]
pub enum UserEventResult {
	Ok(UserEvent),
	Err(UserEventError),
}

#[js_type(export, no_deser)]
pub struct UserEventError {
	pub message: String,
	pub raw: String,
}

impl From<UserEventDeserializeError> for UserEventError {
	fn from(error: UserEventDeserializeError) -> Self {
		Self {
			message: error.message,
			raw: error.raw,
		}
	}
}

impl From<Result<DecryptedUserEventRs, UserEventDeserializeError>> for UserEventResult {
	fn from(result: Result<DecryptedUserEventRs, UserEventDeserializeError>) -> Self {
		match result {
			Ok(event) => UserEventResult::Ok(event.into()),
			Err(error) => UserEventResult::Err(error.into()),
		}
	}
}

#[js_type(export, no_deser)]
pub struct UserEvent {
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub id: u64,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
	pub uuid: UuidStr,
	pub kind: UserEventKind,
}

#[js_type(export, no_deser, tagged)]
pub enum UserEventKind {
	FileUploaded(UserEventFileInfo),
	FileVersioned(UserEventFileInfo),
	FileRestored(UserEventFileInfo),
	VersionedFileRestored(UserEventFileInfo),
	FileMoved(UserEventFileInfo),
	FileRenamed(UserEventFilePairInfo),
	FileMetadataChanged(UserEventFilePairInfo),
	FileTrash(UserEventFileInfo),
	FileRm(UserEventFileInfo),
	FileShared(UserEventFileSharedInfo),
	FileLinkEdited(UserEventFileInfo),
	DeleteFilePermanently(UserEventFileInfo),

	FolderTrash(UserEventFolderInfo),
	FolderShared(UserEventFolderSharedInfo),
	FolderMoved(UserEventFolderInfo),
	FolderRenamed(UserEventFolderPairInfo),
	FolderMetadataChanged(UserEventFolderPairInfo),
	SubFolderCreated(UserEventFolderInfo),
	BaseFolderCreated(UserEventFolderInfo),
	FolderRestored(UserEventFolderInfo),
	FolderColorChanged(UserEventFolderInfo),
	DeleteFolderPermanently(UserEventFolderInfo),

	Login(UserEventBaseInfo),
	FailedLogin(UserEventBaseInfo),
	PasswordChanged(UserEventBaseInfo),
	TwoFaEnabled(UserEventBaseInfo),
	TwoFaDisabled(UserEventBaseInfo),
	RequestAccountDeletion(UserEventBaseInfo),
	TrashEmptied(UserEventBaseInfo),
	DeleteAll(UserEventBaseInfo),
	DeleteVersioned(UserEventBaseInfo),
	DeleteUnfinished(UserEventBaseInfo),

	CodeRedeemed(UserEventCodeRedeemedInfo),
	EmailChanged(UserEventEmailChangedInfo),
	EmailChangeAttempt(UserEventEmailChangeAttemptInfo),
	RemovedSharedInItems(UserEventRemovedSharedInItemsInfo),
	RemovedSharedOutItems(UserEventRemovedSharedOutItemsInfo),
	FolderLinkEdited(UserEventFolderLinkEditedInfo),
	ItemFavorite(UserEventItemFavoriteInfo),
}

#[js_type(export, no_deser)]
pub struct UserEventBaseInfo {
	pub ip: String,
	pub user_agent: String,
}

#[js_type(export, no_deser)]
pub struct UserEventFileInfo {
	pub ip: String,
	pub user_agent: String,
	pub metadata: FileMeta,
}

#[js_type(export, no_deser)]
pub struct UserEventFilePairInfo {
	pub ip: String,
	pub user_agent: String,
	pub metadata: FileMeta,
	pub old_metadata: FileMeta,
}

#[js_type(export, no_deser)]
pub struct UserEventFileSharedInfo {
	pub ip: String,
	pub user_agent: String,
	pub metadata: FileMeta,
	pub receiver_email: String,
}

#[js_type(export, no_deser)]
pub struct UserEventFolderInfo {
	pub ip: String,
	pub user_agent: String,
	pub name: DirMeta,
}

#[js_type(export, no_deser)]
pub struct UserEventFolderPairInfo {
	pub ip: String,
	pub user_agent: String,
	pub name: DirMeta,
	pub old_name: DirMeta,
}

#[js_type(export, no_deser)]
pub struct UserEventFolderSharedInfo {
	pub ip: String,
	pub user_agent: String,
	pub name: DirMeta,
	pub receiver_email: String,
}

#[js_type(export, no_deser)]
pub struct UserEventCodeRedeemedInfo {
	pub ip: String,
	pub user_agent: String,
	pub code: String,
}

#[js_type(export, no_deser)]
pub struct UserEventEmailChangedInfo {
	pub ip: String,
	pub user_agent: String,
	pub email: String,
}

#[js_type(export, no_deser)]
pub struct UserEventEmailChangeAttemptInfo {
	pub ip: String,
	pub user_agent: String,
	pub email: String,
	pub new_email: String,
	pub old_email: String,
}

#[js_type(export, no_deser)]
pub struct UserEventRemovedSharedInItemsInfo {
	pub ip: String,
	pub user_agent: String,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub count: u64,
	pub sharer_email: String,
}

#[js_type(export, no_deser)]
pub struct UserEventRemovedSharedOutItemsInfo {
	pub ip: String,
	pub user_agent: String,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub count: u64,
	pub receiver_email: String,
}

#[js_type(export, no_deser)]
pub struct UserEventFolderLinkEditedInfo {
	pub ip: String,
	pub user_agent: String,
	pub link_uuid: UuidStr,
}

#[js_type(export, no_deser)]
pub struct UserEventItemFavoriteInfo {
	pub ip: String,
	pub user_agent: String,
	pub value: bool,
	/// Encrypted blob can hold either a file or a folder name; in practice
	/// `FileMeta::Decoded` for files and `FileMeta::DecryptedUTF8` for
	/// folders (raw JSON, since the folder schema doesn't match the file one).
	pub metadata: FileMeta,
}

impl From<DecryptedUserEventRs> for UserEvent {
	fn from(event: DecryptedUserEventRs) -> Self {
		Self {
			id: event.id,
			timestamp: event.timestamp,
			uuid: event.uuid,
			kind: event.kind.into(),
		}
	}
}

impl From<DecryptedUserEventKindRs> for UserEventKind {
	fn from(kind: DecryptedUserEventKindRs) -> Self {
		match kind {
			DecryptedUserEventKindRs::FileUploaded(info) => {
				UserEventKind::FileUploaded(info.into())
			}
			DecryptedUserEventKindRs::FileVersioned(info) => {
				UserEventKind::FileVersioned(info.into())
			}
			DecryptedUserEventKindRs::FileRestored(info) => {
				UserEventKind::FileRestored(info.into())
			}
			DecryptedUserEventKindRs::VersionedFileRestored(info) => {
				UserEventKind::VersionedFileRestored(info.into())
			}
			DecryptedUserEventKindRs::FileMoved(info) => UserEventKind::FileMoved(info.into()),
			DecryptedUserEventKindRs::FileRenamed(info) => UserEventKind::FileRenamed(info.into()),
			DecryptedUserEventKindRs::FileMetadataChanged(info) => {
				UserEventKind::FileMetadataChanged(info.into())
			}
			DecryptedUserEventKindRs::FileTrash(info) => UserEventKind::FileTrash(info.into()),
			DecryptedUserEventKindRs::FileRm(info) => UserEventKind::FileRm(info.into()),
			DecryptedUserEventKindRs::FileShared(info) => UserEventKind::FileShared(info.into()),
			DecryptedUserEventKindRs::FileLinkEdited(info) => {
				UserEventKind::FileLinkEdited(info.into())
			}
			DecryptedUserEventKindRs::DeleteFilePermanently(info) => {
				UserEventKind::DeleteFilePermanently(info.into())
			}

			DecryptedUserEventKindRs::FolderTrash(info) => UserEventKind::FolderTrash(info.into()),
			DecryptedUserEventKindRs::FolderShared(info) => {
				UserEventKind::FolderShared(info.into())
			}
			DecryptedUserEventKindRs::FolderMoved(info) => UserEventKind::FolderMoved(info.into()),
			DecryptedUserEventKindRs::FolderRenamed(info) => {
				UserEventKind::FolderRenamed(info.into())
			}
			DecryptedUserEventKindRs::FolderMetadataChanged(info) => {
				UserEventKind::FolderMetadataChanged(info.into())
			}
			DecryptedUserEventKindRs::SubFolderCreated(info) => {
				UserEventKind::SubFolderCreated(info.into())
			}
			DecryptedUserEventKindRs::BaseFolderCreated(info) => {
				UserEventKind::BaseFolderCreated(info.into())
			}
			DecryptedUserEventKindRs::FolderRestored(info) => {
				UserEventKind::FolderRestored(info.into())
			}
			DecryptedUserEventKindRs::FolderColorChanged(info) => {
				UserEventKind::FolderColorChanged(info.into())
			}
			DecryptedUserEventKindRs::DeleteFolderPermanently(info) => {
				UserEventKind::DeleteFolderPermanently(info.into())
			}

			DecryptedUserEventKindRs::Login(info) => UserEventKind::Login(info.into()),
			DecryptedUserEventKindRs::FailedLogin(info) => UserEventKind::FailedLogin(info.into()),
			DecryptedUserEventKindRs::PasswordChanged(info) => {
				UserEventKind::PasswordChanged(info.into())
			}
			DecryptedUserEventKindRs::TwoFaEnabled(info) => {
				UserEventKind::TwoFaEnabled(info.into())
			}
			DecryptedUserEventKindRs::TwoFaDisabled(info) => {
				UserEventKind::TwoFaDisabled(info.into())
			}
			DecryptedUserEventKindRs::RequestAccountDeletion(info) => {
				UserEventKind::RequestAccountDeletion(info.into())
			}
			DecryptedUserEventKindRs::TrashEmptied(info) => {
				UserEventKind::TrashEmptied(info.into())
			}
			DecryptedUserEventKindRs::DeleteAll(info) => UserEventKind::DeleteAll(info.into()),
			DecryptedUserEventKindRs::DeleteVersioned(info) => {
				UserEventKind::DeleteVersioned(info.into())
			}
			DecryptedUserEventKindRs::DeleteUnfinished(info) => {
				UserEventKind::DeleteUnfinished(info.into())
			}

			DecryptedUserEventKindRs::CodeRedeemed(info) => {
				UserEventKind::CodeRedeemed(UserEventCodeRedeemedInfo {
					ip: info.ip,
					user_agent: info.user_agent,
					code: info.code,
				})
			}
			DecryptedUserEventKindRs::EmailChanged(info) => {
				UserEventKind::EmailChanged(UserEventEmailChangedInfo {
					ip: info.ip,
					user_agent: info.user_agent,
					email: info.email,
				})
			}
			DecryptedUserEventKindRs::EmailChangeAttempt(info) => {
				UserEventKind::EmailChangeAttempt(UserEventEmailChangeAttemptInfo {
					ip: info.ip,
					user_agent: info.user_agent,
					email: info.email,
					new_email: info.new_email,
					old_email: info.old_email,
				})
			}
			DecryptedUserEventKindRs::RemovedSharedInItems(info) => {
				UserEventKind::RemovedSharedInItems(UserEventRemovedSharedInItemsInfo {
					ip: info.ip,
					user_agent: info.user_agent,
					count: info.count,
					sharer_email: info.sharer_email,
				})
			}
			DecryptedUserEventKindRs::RemovedSharedOutItems(info) => {
				UserEventKind::RemovedSharedOutItems(UserEventRemovedSharedOutItemsInfo {
					ip: info.ip,
					user_agent: info.user_agent,
					count: info.count,
					receiver_email: info.receiver_email,
				})
			}
			DecryptedUserEventKindRs::FolderLinkEdited(info) => {
				UserEventKind::FolderLinkEdited(UserEventFolderLinkEditedInfo {
					ip: info.ip,
					user_agent: info.user_agent,
					link_uuid: info.link_uuid,
				})
			}
			DecryptedUserEventKindRs::ItemFavorite(info) => {
				UserEventKind::ItemFavorite(UserEventItemFavoriteInfo {
					ip: info.ip,
					user_agent: info.user_agent,
					value: info.value,
					metadata: info.metadata.into(),
				})
			}
		}
	}
}

impl From<crate::user::events::UserEventBaseInfo> for UserEventBaseInfo {
	fn from(info: crate::user::events::UserEventBaseInfo) -> Self {
		Self {
			ip: info.ip,
			user_agent: info.user_agent,
		}
	}
}

impl From<crate::user::events::UserEventFileInfo> for UserEventFileInfo {
	fn from(info: crate::user::events::UserEventFileInfo) -> Self {
		Self {
			ip: info.ip,
			user_agent: info.user_agent,
			metadata: info.metadata.into(),
		}
	}
}

impl From<crate::user::events::UserEventFilePairInfo> for UserEventFilePairInfo {
	fn from(info: crate::user::events::UserEventFilePairInfo) -> Self {
		Self {
			ip: info.ip,
			user_agent: info.user_agent,
			metadata: info.metadata.into(),
			old_metadata: info.old_metadata.into(),
		}
	}
}

impl From<crate::user::events::UserEventFileSharedInfo> for UserEventFileSharedInfo {
	fn from(info: crate::user::events::UserEventFileSharedInfo) -> Self {
		Self {
			ip: info.ip,
			user_agent: info.user_agent,
			metadata: info.metadata.into(),
			receiver_email: info.receiver_email,
		}
	}
}

impl From<crate::user::events::UserEventFolderInfo> for UserEventFolderInfo {
	fn from(info: crate::user::events::UserEventFolderInfo) -> Self {
		Self {
			ip: info.ip,
			user_agent: info.user_agent,
			name: info.name.into(),
		}
	}
}

impl From<crate::user::events::UserEventFolderPairInfo> for UserEventFolderPairInfo {
	fn from(info: crate::user::events::UserEventFolderPairInfo) -> Self {
		Self {
			ip: info.ip,
			user_agent: info.user_agent,
			name: info.name.into(),
			old_name: info.old_name.into(),
		}
	}
}

impl From<crate::user::events::UserEventFolderSharedInfo> for UserEventFolderSharedInfo {
	fn from(info: crate::user::events::UserEventFolderSharedInfo) -> Self {
		Self {
			ip: info.ip,
			user_agent: info.user_agent,
			name: info.name.into(),
			receiver_email: info.receiver_email,
		}
	}
}
