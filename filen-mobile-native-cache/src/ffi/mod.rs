use filen_sdk_rs::fs::{
	HasName, HasParent, HasRemoteInfo, HasUUID,
	dir::{RemoteDirectory, traits::HasRemoteDirInfo},
	file::{RemoteFile, traits::HasFileInfo},
};

use crate::sql::ItemType;

#[derive(uniffi::Record, PartialEq, Eq, Debug, Clone)]
pub struct FfiFile {
	// item
	pub uuid: String,
	pub parent: String,
	pub name: String,

	// file
	pub mime: String,
	pub created: i64,
	pub modified: i64,
	pub size: i64,
	pub chunks: i64,
	pub favorited: bool,
}

impl FfiFile {
	pub(crate) fn from_row(
		row: &rusqlite::Row,
		starting_idx: usize,
		uuid: String,
		parent: String,
		name: String,
	) -> Result<Self, rusqlite::Error> {
		Ok(FfiFile {
			uuid,
			parent,
			name,
			mime: row.get(starting_idx)?,
			created: row.get(starting_idx + 1)?,
			modified: row.get(starting_idx + 2)?,
			size: row.get(starting_idx + 3)?,
			chunks: row.get(starting_idx + 4)?,
			favorited: row.get(starting_idx + 5)?,
		})
	}
}

impl From<&RemoteFile> for FfiFile {
	fn from(file: &RemoteFile) -> Self {
		FfiFile {
			uuid: file.uuid().to_string(),
			parent: file.parent().to_string(),
			name: file.name().to_string(),
			mime: file.mime().to_string(),
			created: file.created().timestamp_millis(),
			modified: file.last_modified().timestamp_millis(),
			size: file.size() as i64,
			chunks: file.chunks() as i64,
			favorited: file.favorited(),
		}
	}
}

#[derive(uniffi::Record, PartialEq, Eq, Debug, Clone)]
pub struct FfiDir {
	// item
	pub uuid: String,
	pub parent: String,
	pub name: String,

	// dir
	pub color: Option<String>,
	pub created: Option<i64>,
	pub favorited: bool,

	// cache info
	pub last_listed: i64,
}

impl FfiDir {
	pub(crate) fn from_row(
		row: &rusqlite::Row,
		starting_idx: usize,
		uuid: String,
		parent: String,
		name: String,
	) -> Result<Self, rusqlite::Error> {
		Ok(FfiDir {
			uuid,
			parent,
			name,
			color: row.get(starting_idx)?,
			created: row.get(starting_idx + 1)?,
			favorited: row.get(starting_idx + 2)?,
			last_listed: row.get(starting_idx + 3)?,
		})
	}
}

impl From<&RemoteDirectory> for FfiDir {
	fn from(dir: &RemoteDirectory) -> Self {
		FfiDir {
			uuid: dir.uuid().to_string(),
			parent: dir.parent().to_string(),
			name: dir.name().to_string(),
			color: dir.color().map(|c| c.to_string()),
			created: dir.created().map(|t| t.timestamp_millis()),
			favorited: dir.favorited(),
			last_listed: 0,
		}
	}
}

#[derive(uniffi::Record, PartialEq, Eq, Debug, Clone)]
pub struct FfiRoot {
	pub uuid: String,
	pub storage_used: i64,
	pub max_storage: i64,
	pub last_updated: i64,
	pub last_listed: i64,
}

impl FfiRoot {
	pub(crate) fn from_row(
		row: &rusqlite::Row,
		starting_idx: usize,
		uuid: String,
	) -> Result<Self, rusqlite::Error> {
		Ok(FfiRoot {
			uuid,
			storage_used: row.get(starting_idx)?,
			max_storage: row.get(starting_idx + 1)?,
			last_updated: row.get(starting_idx + 2)?,
			last_listed: row.get(starting_idx + 3)?,
		})
	}
}

#[derive(uniffi::Enum, PartialEq, Eq, Debug, Clone)]
pub enum FfiObject {
	File(FfiFile),
	Dir(FfiDir),
	Root(FfiRoot),
}

impl FfiObject {
	pub(crate) fn from_row(row: &rusqlite::Row) -> Result<Self, rusqlite::Error> {
		let uuid: String = row.get(0)?;
		let type_: ItemType = row.get(3)?;
		match type_ {
			ItemType::Dir => {
				let parent: String = row.get(1)?;
				let name: String = row.get(2)?;
				let dir = FfiDir::from_row(row, 4, uuid, parent, name)?;
				Ok(FfiObject::Dir(dir))
			}
			ItemType::File => {
				let parent: String = row.get(1)?;
				let name: String = row.get(2)?;
				let file = FfiFile::from_row(row, 8, uuid, parent, name)?;
				Ok(FfiObject::File(file))
			}
			ItemType::Root => {
				let root = FfiRoot::from_row(row, 14, uuid)?;
				Ok(FfiObject::Root(root))
			}
		}
	}
}

#[derive(uniffi::Enum, PartialEq, Eq, Debug, Clone)]
pub enum FfiNonRootObject {
	File(FfiFile),
	Dir(FfiDir),
}

impl FfiNonRootObject {
	pub(crate) fn from_row(row: &rusqlite::Row) -> Result<Self, rusqlite::Error> {
		let uuid: String = row.get(0)?;
		let type_: ItemType = row.get(3)?;
		match type_ {
			ItemType::Dir => {
				let parent: String = row.get(1)?;
				let name: String = row.get(2)?;
				let dir = FfiDir::from_row(row, 4, uuid, parent, name)?;
				Ok(FfiNonRootObject::Dir(dir))
			}
			ItemType::File => {
				let parent: String = row.get(1)?;
				let name: String = row.get(2)?;
				let file = FfiFile::from_row(row, 8, uuid, parent, name)?;
				Ok(FfiNonRootObject::File(file))
			}
			_ => Err(rusqlite::Error::InvalidQuery),
		}
	}
}
