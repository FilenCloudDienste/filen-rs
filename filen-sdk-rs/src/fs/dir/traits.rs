use chrono::{DateTime, Utc};
pub use filen_macros::{HasContents, HasDirInfo, HasDirMeta, HasRemoteDirInfo};
use filen_types::{
	api::v3::dir::color::DirColor,
	fs::{ParentUuid, UuidStr},
};

use crate::{
	error::Error,
	fs::{
		dir::meta::{DirectoryMeta, DirectoryMetaChanges},
		traits::HasUUID,
	},
};

pub trait HasContents: Send + Sync {
	fn uuid_as_parent(&self) -> ParentUuid;
}

impl HasContents for UuidStr {
	fn uuid_as_parent(&self) -> ParentUuid {
		ParentUuid::from(*self)
	}
}

impl HasContents for ParentUuid {
	fn uuid_as_parent(&self) -> ParentUuid {
		*self
	}
}

pub trait HasUUIDContents: HasContents + HasUUID {}

impl<T: HasContents + HasUUID> HasUUIDContents for T {}

pub trait HasRemoteDirInfo {
	fn color(&self) -> DirColor<'_>;
}

pub trait HasDirInfo {
	fn created(&self) -> Option<DateTime<Utc>>;
}

pub trait HasDirMeta {
	fn get_meta(&self) -> &DirectoryMeta<'_>;
}

pub(crate) trait UpdateDirMeta {
	fn update_meta(&mut self, meta: DirectoryMetaChanges) -> Result<(), Error>;
}
