use chrono::{DateTime, Utc};
use filen_types::fs::ParentUuid;
use uuid::Uuid;

use crate::fs::traits::HasUUID;

use super::DirectoryMeta;

pub trait HasContents: Send + Sync {
	fn uuid_as_parent(&self) -> ParentUuid;
}

impl HasContents for Uuid {
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
	fn color(&self) -> Option<&str>;
}

pub trait HasDirInfo {
	fn created(&self) -> Option<DateTime<Utc>>;
}

pub trait HasDirMeta {
	fn borrow_meta(&self) -> DirectoryMeta<'_>;
	fn get_meta(&self) -> DirectoryMeta<'static>;
}

pub(crate) trait SetDirMeta {
	fn set_meta(&mut self, meta: DirectoryMeta<'_>);
}
