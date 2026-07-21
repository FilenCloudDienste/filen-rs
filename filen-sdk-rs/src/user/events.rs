use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::user::events::{
		BaseInfo, FileMetadataInfo, FileMetadataPairInfo, FileSharedInfo, FolderNameInfo,
		FolderNamePairInfo, FolderSharedInfo, ItemFavoriteInfo, UserEvent, UserEventKind,
	},
	auth::FileEncryptionVersion,
	fs::Uuid,
	traits::CowHelpers,
};

use crate::{
	crypto::shared::MetaCrypter,
	fs::{dir::meta::DirectoryMeta, file::meta::FileMeta},
};

// The user-events endpoint does not carry an encryption version, so we follow
// the same convention as socket events (e.g. `FileRename`) and default to V2.
const DEFAULT_FILE_ENCRYPTION_VERSION: FileEncryptionVersion = FileEncryptionVersion::V2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecryptedUserEvent {
	pub id: u64,
	pub timestamp: DateTime<Utc>,
	pub uuid: Uuid,
	pub kind: DecryptedUserEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecryptedUserEventKind {
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

impl DecryptedUserEventKind {
	pub fn event_type(&self) -> &'static str {
		match self {
			Self::FileUploaded(_) => "fileUploaded",
			Self::FileVersioned(_) => "fileVersioned",
			Self::FileRestored(_) => "fileRestored",
			Self::VersionedFileRestored(_) => "versionedFileRestored",
			Self::FileMoved(_) => "fileMoved",
			Self::FileRenamed(_) => "fileRenamed",
			Self::FileMetadataChanged(_) => "fileMetadataChanged",
			Self::FileTrash(_) => "fileTrash",
			Self::FileRm(_) => "fileRm",
			Self::FileShared(_) => "fileShared",
			Self::FileLinkEdited(_) => "fileLinkEdited",
			Self::DeleteFilePermanently(_) => "deleteFilePermanently",
			Self::FolderTrash(_) => "folderTrash",
			Self::FolderShared(_) => "folderShared",
			Self::FolderMoved(_) => "folderMoved",
			Self::FolderRenamed(_) => "folderRenamed",
			Self::FolderMetadataChanged(_) => "folderMetadataChanged",
			Self::SubFolderCreated(_) => "subFolderCreated",
			Self::BaseFolderCreated(_) => "baseFolderCreated",
			Self::FolderRestored(_) => "folderRestored",
			Self::FolderColorChanged(_) => "folderColorChanged",
			Self::DeleteFolderPermanently(_) => "deleteFolderPermanently",
			Self::Login(_) => "login",
			Self::FailedLogin(_) => "failedLogin",
			Self::PasswordChanged(_) => "passwordChanged",
			Self::TwoFaEnabled(_) => "2faEnabled",
			Self::TwoFaDisabled(_) => "2faDisabled",
			Self::RequestAccountDeletion(_) => "requestAccountDeletion",
			Self::TrashEmptied(_) => "trashEmptied",
			Self::DeleteAll(_) => "deleteAll",
			Self::DeleteVersioned(_) => "deleteVersioned",
			Self::DeleteUnfinished(_) => "deleteUnfinished",
			Self::CodeRedeemed(_) => "codeRedeemed",
			Self::EmailChanged(_) => "emailChanged",
			Self::EmailChangeAttempt(_) => "emailChangeAttempt",
			Self::RemovedSharedInItems(_) => "removedSharedInItems",
			Self::RemovedSharedOutItems(_) => "removedSharedOutItems",
			Self::FolderLinkEdited(_) => "folderLinkEdited",
			Self::ItemFavorite(_) => "itemFavorite",
		}
	}
}

impl DecryptedUserEvent {
	pub fn event_type(&self) -> &'static str {
		self.kind.event_type()
	}

	pub(crate) fn blocking_from_encrypted(
		crypter: &impl MetaCrypter,
		event: UserEvent<'_>,
	) -> Self {
		let uuid = event.uuid;
		let kind = match event.kind {
			UserEventKind::FileUploaded(info) => DecryptedUserEventKind::FileUploaded(
				UserEventFileInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::FileVersioned(info) => DecryptedUserEventKind::FileVersioned(
				UserEventFileInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::FileRestored(info) => DecryptedUserEventKind::FileRestored(
				UserEventFileInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::VersionedFileRestored(info) => {
				DecryptedUserEventKind::VersionedFileRestored(
					UserEventFileInfo::blocking_from_encrypted(crypter, info),
				)
			}
			UserEventKind::FileMoved(info) => DecryptedUserEventKind::FileMoved(
				UserEventFileInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::FileRenamed(info) => DecryptedUserEventKind::FileRenamed(
				UserEventFilePairInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::FileMetadataChanged(info) => {
				DecryptedUserEventKind::FileMetadataChanged(
					UserEventFilePairInfo::blocking_from_encrypted(crypter, info),
				)
			}
			UserEventKind::FileTrash(info) => DecryptedUserEventKind::FileTrash(
				UserEventFileInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::FileRm(info) => DecryptedUserEventKind::FileRm(
				UserEventFileInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::FileShared(info) => DecryptedUserEventKind::FileShared(
				UserEventFileSharedInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::FileLinkEdited(info) => DecryptedUserEventKind::FileLinkEdited(
				UserEventFileInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::DeleteFilePermanently(info) => {
				DecryptedUserEventKind::DeleteFilePermanently(
					UserEventFileInfo::blocking_from_encrypted(crypter, info),
				)
			}

			UserEventKind::FolderTrash(info) => DecryptedUserEventKind::FolderTrash(
				UserEventFolderInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::FolderShared(info) => DecryptedUserEventKind::FolderShared(
				UserEventFolderSharedInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::FolderMoved(info) => DecryptedUserEventKind::FolderMoved(
				UserEventFolderInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::FolderRenamed(info) => DecryptedUserEventKind::FolderRenamed(
				UserEventFolderPairInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::FolderMetadataChanged(info) => {
				DecryptedUserEventKind::FolderMetadataChanged(
					UserEventFolderPairInfo::blocking_from_encrypted(crypter, info),
				)
			}
			UserEventKind::SubFolderCreated(info) => DecryptedUserEventKind::SubFolderCreated(
				UserEventFolderInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::BaseFolderCreated(info) => DecryptedUserEventKind::BaseFolderCreated(
				UserEventFolderInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::FolderRestored(info) => DecryptedUserEventKind::FolderRestored(
				UserEventFolderInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::FolderColorChanged(info) => DecryptedUserEventKind::FolderColorChanged(
				UserEventFolderInfo::blocking_from_encrypted(crypter, info),
			),
			UserEventKind::DeleteFolderPermanently(info) => {
				DecryptedUserEventKind::DeleteFolderPermanently(
					UserEventFolderInfo::blocking_from_encrypted(crypter, info),
				)
			}

			UserEventKind::Login(info) => {
				DecryptedUserEventKind::Login(UserEventBaseInfo::from(info))
			}
			UserEventKind::FailedLogin(info) => {
				DecryptedUserEventKind::FailedLogin(UserEventBaseInfo::from(info))
			}
			UserEventKind::PasswordChanged(info) => {
				DecryptedUserEventKind::PasswordChanged(UserEventBaseInfo::from(info))
			}
			UserEventKind::TwoFaEnabled(info) => {
				DecryptedUserEventKind::TwoFaEnabled(UserEventBaseInfo::from(info))
			}
			UserEventKind::TwoFaDisabled(info) => {
				DecryptedUserEventKind::TwoFaDisabled(UserEventBaseInfo::from(info))
			}
			UserEventKind::RequestAccountDeletion(info) => {
				DecryptedUserEventKind::RequestAccountDeletion(UserEventBaseInfo::from(info))
			}
			UserEventKind::TrashEmptied(info) => {
				DecryptedUserEventKind::TrashEmptied(UserEventBaseInfo::from(info))
			}
			UserEventKind::DeleteAll(info) => {
				DecryptedUserEventKind::DeleteAll(UserEventBaseInfo::from(info))
			}
			UserEventKind::DeleteVersioned(info) => {
				DecryptedUserEventKind::DeleteVersioned(UserEventBaseInfo::from(info))
			}
			UserEventKind::DeleteUnfinished(info) => {
				DecryptedUserEventKind::DeleteUnfinished(UserEventBaseInfo::from(info))
			}

			UserEventKind::CodeRedeemed(info) => {
				DecryptedUserEventKind::CodeRedeemed(UserEventCodeRedeemedInfo {
					ip: info.ip.into_owned(),
					user_agent: info.user_agent.into_owned(),
					code: info.code.into_owned(),
				})
			}
			UserEventKind::EmailChanged(info) => {
				DecryptedUserEventKind::EmailChanged(UserEventEmailChangedInfo {
					ip: info.ip.into_owned(),
					user_agent: info.user_agent.into_owned(),
					email: info.email.into_owned(),
				})
			}
			UserEventKind::EmailChangeAttempt(info) => {
				DecryptedUserEventKind::EmailChangeAttempt(UserEventEmailChangeAttemptInfo {
					ip: info.ip.into_owned(),
					user_agent: info.user_agent.into_owned(),
					email: info.email.into_owned(),
					new_email: info.new_email.into_owned(),
					old_email: info.old_email.into_owned(),
				})
			}
			UserEventKind::RemovedSharedInItems(info) => {
				DecryptedUserEventKind::RemovedSharedInItems(UserEventRemovedSharedInItemsInfo {
					ip: info.ip.into_owned(),
					user_agent: info.user_agent.into_owned(),
					count: info.count,
					sharer_email: info.sharer_email.into_owned(),
				})
			}
			UserEventKind::RemovedSharedOutItems(info) => {
				DecryptedUserEventKind::RemovedSharedOutItems(UserEventRemovedSharedOutItemsInfo {
					ip: info.ip.into_owned(),
					user_agent: info.user_agent.into_owned(),
					count: info.count,
					receiver_email: info.receiver_email.into_owned(),
				})
			}
			UserEventKind::FolderLinkEdited(info) => {
				DecryptedUserEventKind::FolderLinkEdited(UserEventFolderLinkEditedInfo {
					ip: info.ip.into_owned(),
					user_agent: info.user_agent.into_owned(),
					link_uuid: info.link_uuid,
				})
			}
			UserEventKind::ItemFavorite(info) => DecryptedUserEventKind::ItemFavorite(
				UserEventItemFavoriteInfo::blocking_from_encrypted(crypter, info),
			),
		};
		Self {
			id: event.id,
			timestamp: event.timestamp,
			uuid,
			kind,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventBaseInfo {
	pub ip: String,
	pub user_agent: String,
}

impl From<BaseInfo<'_>> for UserEventBaseInfo {
	fn from(info: BaseInfo<'_>) -> Self {
		Self {
			ip: info.ip.into_owned(),
			user_agent: info.user_agent.into_owned(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventFileInfo {
	pub ip: String,
	pub user_agent: String,
	pub metadata: FileMeta<'static>,
}

impl UserEventFileInfo {
	fn blocking_from_encrypted(crypter: &impl MetaCrypter, info: FileMetadataInfo<'_>) -> Self {
		Self {
			ip: info.ip.into_owned(),
			user_agent: info.user_agent.into_owned(),
			metadata: FileMeta::blocking_from_encrypted(
				info.metadata,
				crypter,
				DEFAULT_FILE_ENCRYPTION_VERSION,
			)
			.into_owned_cow(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventFilePairInfo {
	pub ip: String,
	pub user_agent: String,
	pub metadata: FileMeta<'static>,
	pub old_metadata: FileMeta<'static>,
}

impl UserEventFilePairInfo {
	fn blocking_from_encrypted(crypter: &impl MetaCrypter, info: FileMetadataPairInfo<'_>) -> Self {
		Self {
			ip: info.ip.into_owned(),
			user_agent: info.user_agent.into_owned(),
			metadata: FileMeta::blocking_from_encrypted(
				info.metadata,
				crypter,
				DEFAULT_FILE_ENCRYPTION_VERSION,
			)
			.into_owned_cow(),
			old_metadata: FileMeta::blocking_from_encrypted(
				info.old_metadata,
				crypter,
				DEFAULT_FILE_ENCRYPTION_VERSION,
			)
			.into_owned_cow(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventFileSharedInfo {
	pub ip: String,
	pub user_agent: String,
	pub metadata: FileMeta<'static>,
	pub receiver_email: String,
}

impl UserEventFileSharedInfo {
	fn blocking_from_encrypted(crypter: &impl MetaCrypter, info: FileSharedInfo<'_>) -> Self {
		Self {
			ip: info.ip.into_owned(),
			user_agent: info.user_agent.into_owned(),
			metadata: FileMeta::blocking_from_encrypted(
				info.metadata,
				crypter,
				DEFAULT_FILE_ENCRYPTION_VERSION,
			)
			.into_owned_cow(),
			receiver_email: info.receiver_email.into_owned(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventFolderInfo {
	pub ip: String,
	pub user_agent: String,
	pub name: DirectoryMeta<'static>,
}

impl UserEventFolderInfo {
	fn blocking_from_encrypted(crypter: &impl MetaCrypter, info: FolderNameInfo<'_>) -> Self {
		Self {
			ip: info.ip.into_owned(),
			user_agent: info.user_agent.into_owned(),
			name: DirectoryMeta::blocking_from_encrypted(info.name, crypter).into_owned_cow(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventFolderPairInfo {
	pub ip: String,
	pub user_agent: String,
	pub name: DirectoryMeta<'static>,
	pub old_name: DirectoryMeta<'static>,
}

impl UserEventFolderPairInfo {
	fn blocking_from_encrypted(crypter: &impl MetaCrypter, info: FolderNamePairInfo<'_>) -> Self {
		Self {
			ip: info.ip.into_owned(),
			user_agent: info.user_agent.into_owned(),
			name: DirectoryMeta::blocking_from_encrypted(info.name, crypter).into_owned_cow(),
			old_name: DirectoryMeta::blocking_from_encrypted(info.old_name, crypter)
				.into_owned_cow(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventFolderSharedInfo {
	pub ip: String,
	pub user_agent: String,
	pub name: DirectoryMeta<'static>,
	pub receiver_email: String,
}

impl UserEventFolderSharedInfo {
	fn blocking_from_encrypted(crypter: &impl MetaCrypter, info: FolderSharedInfo<'_>) -> Self {
		Self {
			ip: info.ip.into_owned(),
			user_agent: info.user_agent.into_owned(),
			name: DirectoryMeta::blocking_from_encrypted(info.name, crypter).into_owned_cow(),
			receiver_email: info.receiver_email.into_owned(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventCodeRedeemedInfo {
	pub ip: String,
	pub user_agent: String,
	pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventEmailChangedInfo {
	pub ip: String,
	pub user_agent: String,
	pub email: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventEmailChangeAttemptInfo {
	pub ip: String,
	pub user_agent: String,
	pub email: String,
	pub new_email: String,
	pub old_email: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventRemovedSharedInItemsInfo {
	pub ip: String,
	pub user_agent: String,
	pub count: u64,
	pub sharer_email: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventRemovedSharedOutItemsInfo {
	pub ip: String,
	pub user_agent: String,
	pub count: u64,
	pub receiver_email: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventFolderLinkEditedInfo {
	pub ip: String,
	pub user_agent: String,
	pub link_uuid: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEventItemFavoriteInfo {
	pub ip: String,
	pub user_agent: String,
	pub value: bool,
	/// The encrypted blob can hold either a file or a folder name. We try
	/// decoding as a file (richer schema) first; for plain folder favourites
	/// the result will be `FileMeta::DecryptedUTF8` with the raw JSON.
	pub metadata: FileMeta<'static>,
}

impl UserEventItemFavoriteInfo {
	fn blocking_from_encrypted(crypter: &impl MetaCrypter, info: ItemFavoriteInfo<'_>) -> Self {
		Self {
			ip: info.ip.into_owned(),
			user_agent: info.user_agent.into_owned(),
			value: info.value,
			metadata: FileMeta::blocking_from_encrypted(
				info.metadata,
				crypter,
				DEFAULT_FILE_ENCRYPTION_VERSION,
			)
			.into_owned_cow(),
		}
	}
}
