use std::borrow::Cow;

use chrono::{DateTime, SubsecRound, Utc};
use filen_types::{
	api::v3::dir::color::DirColor,
	crypto::EncryptedString,
	fs::{ObjectType, ParentUuid, UuidStr},
};
use traits::{HasDirInfo, HasDirMeta, HasRemoteDirInfo, UpdateDirMeta};

use crate::{
	crypto::shared::MetaCrypter,
	error::{Error, InvalidNameError},
	fs::{
		SetRemoteInfo,
		dir::meta::{DirectoryMeta, DirectoryMetaChanges},
	},
};

use super::{HasMeta, HasName, HasParent, HasRemoteInfo, HasType, HasUUID};

pub mod client_impl;
pub mod enums;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub mod js_impl;
pub mod meta;
pub mod traits;

pub use enums::*;
pub use meta::DecryptedDirectoryMeta;
pub use traits::{HasContents, HasUUIDContents};

#[derive(Clone, Debug, PartialEq, Eq)]

pub struct RootDirectory {
	uuid: UuidStr,
}

impl RootDirectory {
	pub fn new(uuid: UuidStr) -> Self {
		Self { uuid }
	}
}

impl HasUUID for RootDirectory {
	fn uuid(&self) -> &UuidStr {
		&self.uuid
	}
}
impl HasContents for RootDirectory {
	fn uuid_as_parent(&self) -> ParentUuid {
		self.uuid.into()
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootDirectoryWithMeta {
	pub(crate) uuid: UuidStr,

	pub(crate) color: DirColor<'static>,
	pub(crate) timestamp: DateTime<Utc>,

	pub(crate) meta: DirectoryMeta<'static>,
}

impl RootDirectoryWithMeta {
	pub fn from_meta(
		uuid: UuidStr,
		color: DirColor<'static>,
		timestamp: DateTime<Utc>,
		meta: DirectoryMeta<'static>,
	) -> Self {
		Self {
			uuid,
			color,
			timestamp,
			meta,
		}
	}
}

impl HasUUID for RootDirectoryWithMeta {
	fn uuid(&self) -> &UuidStr {
		&self.uuid
	}
}
impl HasContents for RootDirectoryWithMeta {
	fn uuid_as_parent(&self) -> ParentUuid {
		self.uuid.into()
	}
}

impl HasType for RootDirectoryWithMeta {
	fn object_type(&self) -> ObjectType {
		ObjectType::Dir
	}
}

impl HasName for RootDirectoryWithMeta {
	fn name(&self) -> Option<&str> {
		self.meta.name()
	}
}

impl HasDirMeta for RootDirectoryWithMeta {
	fn get_meta(&self) -> &DirectoryMeta<'_> {
		&self.meta
	}
}

impl UpdateDirMeta for RootDirectoryWithMeta {
	fn update_meta(&mut self, meta: DirectoryMetaChanges) -> Result<(), Error> {
		self.meta.apply_changes(meta)
	}
}

impl HasMeta for RootDirectoryWithMeta {
	fn get_meta_string(&self) -> Option<Cow<'_, str>> {
		self.meta.try_to_string()
	}
}

impl HasDirInfo for RootDirectoryWithMeta {
	fn created(&self) -> Option<DateTime<Utc>> {
		self.meta.created()
	}
}

impl HasRemoteInfo for RootDirectoryWithMeta {
	fn favorited(&self) -> bool {
		false
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDirectory {
	pub uuid: UuidStr,
	pub parent: ParentUuid,

	pub color: DirColor<'static>,
	pub favorited: bool,
	pub timestamp: DateTime<Utc>,

	pub meta: DirectoryMeta<'static>,
}

impl RemoteDirectory {
	pub fn blocking_from_encrypted(
		uuid: UuidStr,
		parent: ParentUuid,
		color: DirColor<'static>,
		favorited: bool,
		timestamp: DateTime<Utc>,
		meta: EncryptedString<'_>,
		decrypter: &impl MetaCrypter,
	) -> Self {
		let meta = DirectoryMeta::blocking_from_encrypted(meta, decrypter).into_owned();
		Self {
			uuid,
			parent,
			color,
			favorited,
			timestamp,
			meta,
		}
	}

	pub fn from_meta(
		uuid: UuidStr,
		parent: ParentUuid,
		color: DirColor<'static>,
		favorited: bool,
		timestamp: DateTime<Utc>,
		meta: DirectoryMeta<'static>,
	) -> Self {
		Self {
			uuid,
			parent,
			color,
			favorited,
			timestamp,
			meta,
		}
	}

	pub fn make_parts(
		name: String,
		created: DateTime<Utc>,
	) -> Result<(UuidStr, DecryptedDirectoryMeta<'static>), Error> {
		if name.is_empty() {
			return Err(InvalidNameError(name).into());
		}
		Ok((
			UuidStr::new_v4(),
			DecryptedDirectoryMeta {
				name: Cow::Owned(name),
				created: Some(created.round_subsecs(3)),
			},
		))
	}

	pub fn new_from_parts(
		uuid: UuidStr,
		meta: DecryptedDirectoryMeta<'static>,
		parent: ParentUuid,
		timestamp: DateTime<Utc>,
	) -> Self {
		Self {
			uuid,
			parent,
			color: DirColor::Default,
			favorited: false,
			timestamp,
			meta: DirectoryMeta::Decoded(meta),
		}
	}

	pub(crate) fn set_uuid(&mut self, uuid: UuidStr) {
		self.uuid = uuid;
	}

	pub(crate) fn set_parent(&mut self, parent: ParentUuid) {
		self.parent = parent;
	}

	pub fn created(&self) -> Option<DateTime<Utc>> {
		self.meta.created()
	}
}

impl HasUUID for RemoteDirectory {
	fn uuid(&self) -> &UuidStr {
		&self.uuid
	}
}
impl HasContents for RemoteDirectory {
	fn uuid_as_parent(&self) -> ParentUuid {
		self.uuid.into()
	}
}

impl HasType for RemoteDirectory {
	fn object_type(&self) -> ObjectType {
		ObjectType::Dir
	}
}

impl HasName for RemoteDirectory {
	fn name(&self) -> Option<&str> {
		self.meta.name()
	}
}

impl HasDirMeta for RemoteDirectory {
	fn get_meta(&self) -> &DirectoryMeta<'_> {
		&self.meta
	}
}

impl UpdateDirMeta for RemoteDirectory {
	fn update_meta(&mut self, changes: DirectoryMetaChanges) -> Result<(), Error> {
		self.meta.apply_changes(changes)
	}
}

impl HasMeta for RemoteDirectory {
	fn get_meta_string(&self) -> Option<Cow<'_, str>> {
		self.meta.try_to_string()
	}
}

impl HasParent for RemoteDirectory {
	fn parent(&self) -> &ParentUuid {
		&self.parent
	}
}

impl HasDirInfo for RemoteDirectory {
	fn created(&self) -> Option<DateTime<Utc>> {
		self.meta.created()
	}
}

impl HasRemoteInfo for RemoteDirectory {
	fn favorited(&self) -> bool {
		self.favorited
	}
}

impl SetRemoteInfo for RemoteDirectory {
	fn set_favorited(&mut self, value: bool) {
		self.favorited = value;
	}
}

impl HasRemoteDirInfo for RemoteDirectory {
	fn color(&self) -> DirColor<'_> {
		self.color.borrow_clone()
	}
}
