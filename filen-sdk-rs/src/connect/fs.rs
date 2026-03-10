use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::{
	api::v3::{
		dir::color::DirColor,
		shared::{
			in_root::{SharedRootDirIn, SharedRootFileIn},
			out_root::{SharedRootDirOut, SharedRootFileOut},
		},
	},
	fs::UuidStr,
	traits::CowHelpers,
};
use rsa::RsaPrivateKey;

use crate::{
	crypto::shared::MetaCrypter,
	error::Error,
	fs::{
		HasMeta, HasName, HasParent, HasRemoteInfo, HasUUID,
		dir::{
			RemoteDirectory, RootDirectoryWithMeta,
			meta::DirectoryMeta,
			traits::{HasDirInfo, HasDirMeta},
		},
		file::{
			RemoteRootFile,
			meta::FileMeta,
			traits::{File, HasRemoteFileInfo},
		},
	},
	io::HasFileInfo,
};

#[derive(serde::Deserialize, serde::Serialize)]
#[js_type(wasm_all, no_deser, no_ser)]
pub struct ShareInfo {
	pub email: String,
	pub id: u64,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "type")]
#[js_type(import, export, wasm_all, tagged, no_deser, no_ser)]
pub enum SharingRole {
	Sharer(ShareInfo),
	Receiver(ShareInfo),
}

impl SharingRole {
	pub fn email(&self) -> &str {
		match self {
			SharingRole::Sharer(info) | SharingRole::Receiver(info) => &info.email,
		}
	}

	pub(crate) fn id(&self) -> u64 {
		match self {
			SharingRole::Sharer(info) | SharingRole::Receiver(info) => info.id,
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedDirInfo {
	pub(crate) sharing_role: SharingRole,
	pub(crate) write_access: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedDirectory {
	pub(crate) inner: RemoteDirectory,
}

impl HasUUID for SharedDirectory {
	fn uuid(&self) -> &UuidStr {
		self.inner.uuid()
	}
}

impl HasDirInfo for SharedDirectory {
	fn created(&self) -> Option<DateTime<Utc>> {
		self.inner.created()
	}
}

impl HasDirMeta for SharedDirectory {
	fn get_meta(&self) -> &DirectoryMeta<'_> {
		self.inner.get_meta()
	}
}

impl HasName for SharedDirectory {
	fn name(&self) -> Option<&str> {
		self.inner.name()
	}
}

impl HasRemoteInfo for SharedDirectory {
	fn timestamp(&self) -> DateTime<Utc> {
		self.inner.timestamp()
	}

	fn favorited(&self) -> bool {
		false
	}
}

impl HasMeta for SharedDirectory {
	fn get_meta_string(&self) -> Option<Cow<'_, str>> {
		self.inner.get_meta_string()
	}
}

impl HasParent for SharedDirectory {
	fn parent(&self) -> &filen_types::fs::ParentUuid {
		self.inner.parent()
	}
}

pub(crate) struct DirInfo {
	pub(crate) uuid: UuidStr,
	pub(crate) parent: UuidStr,
	pub(crate) color: DirColor<'static>,
	pub(crate) timestamp: DateTime<Utc>,
	pub(crate) metadata: DirectoryMeta<'static>,
}

impl SharedDirectory {
	pub(crate) fn from_dir_info(dir_info: DirInfo) -> Self {
		Self {
			inner: RemoteDirectory::from_meta(
				dir_info.uuid,
				dir_info.parent.into(),
				dir_info.color,
				false,
				dir_info.timestamp,
				dir_info.metadata,
			),
		}
	}

	pub fn get_dir(&self) -> &RemoteDirectory {
		&self.inner
	}
}

struct RootDirInfo {
	uuid: UuidStr,
	color: DirColor<'static>,
	timestamp: DateTime<Utc>,
	metadata: DirectoryMeta<'static>,
	write_access: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedRootDirectory {
	pub(crate) info: SharedDirInfo,
	pub(crate) dir: RootDirectoryWithMeta,
}

impl SharedRootDirectory {
	fn inner_from_share(dir_info: RootDirInfo, sharing_role: SharingRole) -> Self {
		let dir = RootDirectoryWithMeta::from_meta(
			dir_info.uuid,
			dir_info.color,
			dir_info.timestamp,
			dir_info.metadata,
		);

		Self {
			dir,
			info: SharedDirInfo {
				sharing_role,
				write_access: dir_info.write_access,
			},
		}
	}

	pub fn blocking_from_shared_in(
		shared_dir: SharedRootDirIn<'_>,
		private_key: &RsaPrivateKey,
	) -> Result<Self, Error> {
		let sharing_role = SharingRole::Sharer(ShareInfo {
			email: shared_dir.sharer_email.into_owned(),
			id: shared_dir.sharer_id,
		});

		Ok(Self::inner_from_share(
			RootDirInfo {
				uuid: shared_dir.uuid,
				color: shared_dir.color.into_owned_cow(),
				metadata: DirectoryMeta::blocking_from_rsa_encrypted(
					shared_dir.metadata,
					private_key,
				)
				.into_owned_cow(),
				timestamp: shared_dir.timestamp,
				write_access: shared_dir.write_access,
			},
			sharing_role,
		))
	}

	pub fn blocking_from_shared_out(
		shared_dir: SharedRootDirOut<'_>,
		crypter: &impl MetaCrypter,
	) -> Result<Self, Error> {
		let sharing_role = SharingRole::Receiver(ShareInfo {
			email: shared_dir.receiver_email.into_owned(),
			id: shared_dir.receiver_id,
		});
		Ok(Self::inner_from_share(
			RootDirInfo {
				uuid: shared_dir.uuid,
				color: shared_dir.color.into_owned_cow(),
				metadata: DirectoryMeta::blocking_from_encrypted(shared_dir.metadata, crypter)
					.into_owned_cow(),
				timestamp: shared_dir.timestamp,
				write_access: shared_dir.write_access,
			},
			sharing_role,
		))
	}

	pub fn get_dir(&self) -> &RootDirectoryWithMeta {
		&self.dir
	}

	pub fn sharing_role(&self) -> &SharingRole {
		&self.info.sharing_role
	}

	pub fn get_source_id(&self) -> u64 {
		match &self.info.sharing_role {
			SharingRole::Sharer(info) | SharingRole::Receiver(info) => info.id,
		}
	}
}

impl HasUUID for SharedRootDirectory {
	fn uuid(&self) -> &UuidStr {
		self.dir.uuid()
	}
}

impl HasDirInfo for SharedRootDirectory {
	fn created(&self) -> Option<DateTime<Utc>> {
		self.dir.created()
	}
}

impl HasDirMeta for SharedRootDirectory {
	fn get_meta(&self) -> &DirectoryMeta<'_> {
		self.dir.get_meta()
	}
}

impl HasName for SharedRootDirectory {
	fn name(&self) -> Option<&str> {
		self.dir.name()
	}
}

impl HasRemoteInfo for SharedRootDirectory {
	fn timestamp(&self) -> DateTime<Utc> {
		self.dir.timestamp()
	}

	fn favorited(&self) -> bool {
		false
	}
}

impl HasMeta for SharedRootDirectory {
	fn get_meta_string(&self) -> Option<Cow<'_, str>> {
		self.dir.get_meta_string()
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedRootFile {
	pub(crate) file: RemoteRootFile,
	pub(crate) sharing_role: SharingRole,
}

impl HasUUID for SharedRootFile {
	fn uuid(&self) -> &UuidStr {
		self.file.uuid()
	}
}

impl HasName for SharedRootFile {
	fn name(&self) -> Option<&str> {
		self.file.name()
	}
}

impl HasFileInfo for SharedRootFile {
	fn mime(&self) -> Option<&str> {
		self.file.mime()
	}

	fn created(&self) -> Option<DateTime<Utc>> {
		self.file.created()
	}

	fn last_modified(&self) -> Option<DateTime<Utc>> {
		self.file.last_modified()
	}

	fn size(&self) -> u64 {
		self.file.size()
	}

	fn chunks(&self) -> u64 {
		self.file.chunks()
	}

	fn key(&self) -> Option<&crate::crypto::file::FileKey> {
		self.file.key()
	}
}

impl HasRemoteInfo for SharedRootFile {
	fn timestamp(&self) -> DateTime<Utc> {
		self.file.timestamp()
	}

	fn favorited(&self) -> bool {
		false
	}
}

impl HasRemoteFileInfo for SharedRootFile {
	fn region(&self) -> &str {
		self.file.region()
	}

	fn bucket(&self) -> &str {
		self.file.bucket()
	}

	fn hash(&self) -> Option<filen_types::crypto::Blake3Hash> {
		self.file.hash()
	}
}

impl HasMeta for SharedRootFile {
	fn get_meta_string(&self) -> Option<Cow<'_, str>> {
		self.file.get_meta_string()
	}
}

impl File for SharedRootFile {}

struct FileInfo<'a> {
	uuid: UuidStr,
	parent: UuidStr,
	size: u64,
	chunks: u64,
	region: Cow<'a, str>,
	bucket: Cow<'a, str>,
	timestamp: DateTime<Utc>,
	metadata: FileMeta<'a>,
}

impl SharedRootFile {
	pub(crate) fn blocking_from_shared_in(
		shared_file: SharedRootFileIn<'_>,
		private_key: &RsaPrivateKey,
	) -> Result<Self, Error> {
		let meta = FileMeta::blocking_from_rsa_encrypted(
			shared_file.metadata,
			private_key,
			shared_file.version,
		)
		.into_owned_cow();

		let file = RemoteRootFile::from_meta(
			shared_file.uuid,
			shared_file.size,
			shared_file.chunks,
			shared_file.region.into_owned(),
			shared_file.bucket.into_owned(),
			shared_file.timestamp,
			meta,
		);

		let sharer = SharingRole::Sharer(ShareInfo {
			email: shared_file.sharer_email.into_owned(),
			id: shared_file.sharer_id,
		});

		Ok(Self {
			file,
			sharing_role: sharer,
		})
	}

	pub(crate) fn blocking_from_shared_out(
		shared_file: SharedRootFileOut<'_>,
		crypter: &impl MetaCrypter,
	) -> Result<Self, Error> {
		let meta =
			FileMeta::blocking_from_encrypted(shared_file.metadata, crypter, shared_file.version)
				.into_owned_cow();
		let file = RemoteRootFile::from_meta(
			shared_file.uuid,
			shared_file.size,
			shared_file.chunks,
			shared_file.region.into_owned(),
			shared_file.bucket.into_owned(),
			shared_file.timestamp,
			meta,
		);

		let receiver = SharingRole::Receiver(ShareInfo {
			email: shared_file.receiver_email.into_owned(),
			id: shared_file.receiver_id,
		});

		Ok(Self {
			file,
			sharing_role: receiver,
		})
	}

	pub fn get_file(&self) -> &RemoteRootFile {
		&self.file
	}

	pub fn get_source_id(&self) -> u64 {
		match &self.sharing_role {
			SharingRole::Sharer(info) | SharingRole::Receiver(info) => info.id,
		}
	}
}
