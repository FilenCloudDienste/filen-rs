use std::borrow::Cow;

use filen_types::{
	api::v3::shared::{
		r#in::{SharedDirIn, SharedFileIn},
		out::{SharedDirOut, SharedFileOut},
	},
	fs::UuidStr,
};
use rsa::RsaPrivateKey;

use crate::{
	crypto::shared::MetaCrypter,
	error::Error,
	fs::{
		dir::{DirectoryMeta, DirectoryMetaType, RemoteDirectory, RootDirectoryWithMeta},
		file::{RemoteRootFile, meta::FileMeta},
	},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareInfo {
	pub email: String,
	pub id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SharingRole {
	Sharer(ShareInfo),
	Receiver(ShareInfo),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedDirectory {
	dir: DirectoryMetaType<'static>,
	sharing_role: SharingRole,
	write_access: bool,
}

struct DirInfo {
	uuid: UuidStr,
	parent: Option<UuidStr>,
	color: Option<String>,
	metadata: DirectoryMeta<'static>,
	write_access: bool,
}

impl SharedDirectory {
	fn inner_from_share(dir_info: DirInfo, sharing_role: SharingRole) -> Self {
		let dir = match dir_info.parent {
			Some(parent) => DirectoryMetaType::Dir(Cow::Owned(RemoteDirectory::from_meta(
				dir_info.uuid,
				parent.into(),
				dir_info.color,
				false,
				dir_info.metadata,
			))),
			None => DirectoryMetaType::Root(Cow::Owned(RootDirectoryWithMeta::from_meta(
				dir_info.uuid,
				dir_info.color,
				dir_info.metadata,
			))),
		};

		Self {
			dir,
			sharing_role,
			write_access: dir_info.write_access,
		}
	}

	pub fn from_shared_in(
		shared_dir: SharedDirIn<'_>,
		private_key: &RsaPrivateKey,
	) -> Result<Self, Error> {
		let sharing_role = SharingRole::Sharer(ShareInfo {
			email: shared_dir.sharer_email.into_owned(),
			id: shared_dir.sharer_id,
		});

		Ok(Self::inner_from_share(
			DirInfo {
				uuid: shared_dir.uuid,
				parent: shared_dir.parent,
				color: shared_dir.color.map(|s| s.into_owned()),
				metadata: DirectoryMeta::from_rsa_encrypted(&shared_dir.metadata, private_key)?,
				write_access: shared_dir.write_access,
			},
			sharing_role,
		))
	}

	pub fn from_shared_out(
		shared_dir: SharedDirOut<'_>,
		crypter: &impl MetaCrypter,
	) -> Result<Self, Error> {
		let sharing_role = SharingRole::Receiver(ShareInfo {
			email: shared_dir.receiver_email.into_owned(),
			id: shared_dir.receiver_id,
		});
		Ok(Self::inner_from_share(
			DirInfo {
				uuid: shared_dir.uuid,
				parent: shared_dir.parent,
				color: shared_dir.color.map(|s| s.into_owned()),
				metadata: DirectoryMeta::from_encrypted(&shared_dir.metadata, crypter)?,
				write_access: shared_dir.write_access,
			},
			sharing_role,
		))
	}

	pub fn get_dir(&self) -> &DirectoryMetaType<'_> {
		&self.dir
	}
}

pub struct SharedFile {
	file: RemoteRootFile,
	sharing_role: SharingRole,
}

impl SharedFile {
	pub fn from_shared_in(
		shared_file: SharedFileIn<'_>,
		private_key: &RsaPrivateKey,
	) -> Result<Self, Error> {
		let meta =
			FileMeta::from_rsa_encrypted(&shared_file.metadata, private_key, shared_file.version)?;

		let file = RemoteRootFile::from_meta(
			shared_file.uuid,
			shared_file.size,
			shared_file.chunks,
			shared_file.region.into_owned(),
			shared_file.bucket.into_owned(),
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

	pub fn from_shared_out(
		shared_file: SharedFileOut<'_>,
		crypter: &impl MetaCrypter,
	) -> Result<Self, Error> {
		let meta = FileMeta::from_encrypted(&shared_file.metadata, crypter, shared_file.version)?;
		let file = RemoteRootFile::from_meta(
			shared_file.uuid,
			shared_file.size,
			shared_file.chunks,
			shared_file.region.into_owned(),
			shared_file.bucket.into_owned(),
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
}
