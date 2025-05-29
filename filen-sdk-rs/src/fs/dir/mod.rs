use std::borrow::Cow;

use chrono::{DateTime, SubsecRound, Utc};
use filen_types::{crypto::EncryptedString, fs::ObjectType};
use traits::{HasDirInfo, HasDirMeta, HasRemoteDirInfo, SetDirMeta};
use uuid::Uuid;

use crate::crypto::shared::MetaCrypter;

use super::{HasMeta, HasName, HasParent, HasRemoteInfo, HasType, HasUUID};

pub mod client_impl;
pub mod enums;
pub mod meta;
pub mod traits;

pub use enums::*;
pub use meta::DirectoryMeta;
pub use traits::HasContents;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootDirectory {
	uuid: Uuid,
}

impl RootDirectory {
	pub fn new(uuid: Uuid) -> Self {
		Self { uuid }
	}
}

impl HasUUID for RootDirectory {
	fn uuid(&self) -> uuid::Uuid {
		self.uuid
	}
}
impl HasContents for RootDirectory {}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RootDirectoryWithMeta {
	uuid: Uuid,
	name: String,

	color: Option<String>,
	created: Option<DateTime<Utc>>,
}

impl RootDirectoryWithMeta {
	pub fn from_meta(uuid: Uuid, color: Option<String>, meta: DirectoryMeta<'_>) -> Self {
		Self {
			uuid,
			name: meta.name.into_owned(),
			color,
			created: meta.created,
		}
	}
}

impl HasUUID for RootDirectoryWithMeta {
	fn uuid(&self) -> uuid::Uuid {
		self.uuid
	}
}
impl HasContents for RootDirectoryWithMeta {}

impl HasType for RootDirectoryWithMeta {
	fn object_type(&self) -> ObjectType {
		ObjectType::Dir
	}
}

impl HasName for RootDirectoryWithMeta {
	fn name(&self) -> &str {
		&self.name
	}
}

impl HasDirMeta for RootDirectoryWithMeta {
	fn borrow_meta(&self) -> DirectoryMeta<'_> {
		DirectoryMeta {
			name: Cow::Borrowed(&self.name),
			created: self.created,
		}
	}

	fn get_meta(&self) -> DirectoryMeta<'static> {
		DirectoryMeta {
			name: Cow::Owned(self.name.clone()),
			created: self.created,
		}
	}
}

impl SetDirMeta for RootDirectoryWithMeta {
	fn set_meta(&mut self, meta: DirectoryMeta<'_>) {
		self.name = meta.name.into_owned();
		self.created = meta.created;
	}
}

impl HasMeta for RootDirectoryWithMeta {
	fn get_meta_string(&self) -> String {
		serde_json::to_string(&self.borrow_meta()).unwrap()
	}
}

impl HasDirInfo for RootDirectoryWithMeta {
	fn created(&self) -> Option<DateTime<Utc>> {
		self.created
	}
}

impl HasRemoteInfo for RootDirectoryWithMeta {
	fn favorited(&self) -> bool {
		false
	}
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RemoteDirectory {
	pub uuid: Uuid,
	pub name: String,
	pub parent: Uuid,

	pub color: Option<String>, // todo use Color struct
	pub created: Option<DateTime<Utc>>,
	pub favorited: bool,
}

impl RemoteDirectory {
	pub fn from_encrypted(
		uuid: Uuid,
		parent: Uuid,
		color: Option<String>,
		favorited: bool,
		meta: &EncryptedString,
		decrypter: &impl MetaCrypter,
	) -> Result<Self, crate::error::Error> {
		let meta = DirectoryMeta::from_encrypted(meta, decrypter)?;
		Ok(Self {
			name: meta.name.into_owned(),
			uuid,
			parent,
			color,
			created: meta.created,
			favorited,
		})
	}

	pub fn from_meta(
		uuid: Uuid,
		parent: Uuid,
		color: Option<String>,
		favorited: bool,
		meta: DirectoryMeta<'_>,
	) -> Self {
		Self {
			name: meta.name.into_owned(),
			uuid,
			parent,
			color,
			created: meta.created,
			favorited,
		}
	}

	pub fn new(name: String, parent: Uuid, created: DateTime<Utc>) -> Self {
		Self {
			uuid: Uuid::new_v4(),
			name,
			parent,
			color: None,
			created: Some(created.round_subsecs(3)),
			favorited: false,
		}
	}

	pub(crate) fn set_uuid(&mut self, uuid: Uuid) {
		self.uuid = uuid;
	}

	pub(crate) fn set_parent(&mut self, parent: Uuid) {
		self.parent = parent;
	}

	pub fn created(&self) -> Option<DateTime<Utc>> {
		self.created
	}
}

impl HasUUID for RemoteDirectory {
	fn uuid(&self) -> uuid::Uuid {
		self.uuid
	}
}
impl HasContents for RemoteDirectory {}

impl HasType for RemoteDirectory {
	fn object_type(&self) -> ObjectType {
		ObjectType::Dir
	}
}

impl HasName for RemoteDirectory {
	fn name(&self) -> &str {
		&self.name
	}
}

impl HasDirMeta for RemoteDirectory {
	fn borrow_meta(&self) -> DirectoryMeta<'_> {
		DirectoryMeta {
			name: Cow::Borrowed(&self.name),
			created: self.created,
		}
	}

	fn get_meta(&self) -> DirectoryMeta<'static> {
		DirectoryMeta {
			name: Cow::Owned(self.name.clone()),
			created: self.created,
		}
	}
}

impl SetDirMeta for RemoteDirectory {
	fn set_meta(&mut self, meta: DirectoryMeta<'_>) {
		self.name = meta.name.into_owned();
		self.created = meta.created;
	}
}

impl HasMeta for RemoteDirectory {
	fn get_meta_string(&self) -> String {
		serde_json::to_string(&self.borrow_meta()).unwrap()
	}
}

impl HasParent for RemoteDirectory {
	fn parent(&self) -> Uuid {
		self.parent
	}
}

impl HasDirInfo for RemoteDirectory {
	fn created(&self) -> Option<DateTime<Utc>> {
		self.created
	}
}

impl HasRemoteInfo for RemoteDirectory {
	fn favorited(&self) -> bool {
		self.favorited
	}
}

impl HasRemoteDirInfo for RemoteDirectory {
	fn color(&self) -> Option<&str> {
		self.color.as_deref()
	}
}
