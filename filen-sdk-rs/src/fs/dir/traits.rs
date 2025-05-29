use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::fs::traits::HasUUID;

use super::DirectoryMeta;

pub trait HasContents: HasUUID {}

impl HasContents for Uuid {}

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
