use std::{collections::HashMap, str::FromStr};

use filen_sdk_rs::util::PathIteratorExt;
use filen_types::fs::UuidStr;

use crate::{
	CacheError,
	sql::{
		DBDirMeta, DBDirObject, DBFileMeta, DBRoot,
		dir::DBDir,
		file::DBFile,
		object::{DBNonRootObject, DBObject},
	},
};

#[derive(uniffi::Record, PartialEq, Eq, Debug, Clone)]
pub struct FfiFileMeta {
	pub name: String,
	pub mime: String,
	pub created: i64,
	pub modified: i64,
	pub hash: Option<Vec<u8>>,
}

#[derive(uniffi::Record, PartialEq, Eq, Debug, Clone)]
pub struct FfiFile {
	// item
	pub uuid: String,
	pub parent: String,

	// file
	pub meta: Option<FfiFileMeta>,
	pub size: i64,
	pub favorite_rank: i64,

	pub local_data: Option<HashMap<String, String>>,
}

impl From<DBFile> for FfiFile {
	fn from(file: DBFile) -> Self {
		FfiFile {
			uuid: file.uuid.to_string(),
			parent: file.parent.to_string(),
			size: file.size,
			favorite_rank: file.favorite_rank,
			local_data: file.local_data.map(|o| o.to_map()),
			meta: match file.meta {
				DBFileMeta::Decoded(meta) => Some(FfiFileMeta {
					name: meta.name.to_string(),
					mime: meta.mime.to_string(),
					created: meta.created.unwrap_or_default(),
					modified: meta.modified,
					hash: meta.hash.map(|h| h.to_vec()),
				}),
				_ => None,
			},
		}
	}
}

#[derive(uniffi::Record, PartialEq, Eq, Debug, Clone)]
pub struct FfiDirMeta {
	pub name: String,
	pub created: Option<i64>,
}

#[derive(uniffi::Record, PartialEq, Eq, Debug, Clone)]
pub struct FfiDir {
	// item
	pub uuid: String,
	pub parent: String,

	// dir
	pub meta: Option<FfiDirMeta>,
	pub color: Option<String>,
	pub favorite_rank: i64,

	// cache info
	pub last_listed: i64,

	pub local_data: Option<HashMap<String, String>>,
}

impl From<DBDir> for FfiDir {
	fn from(dir: DBDir) -> Self {
		FfiDir {
			uuid: dir.uuid.to_string(),
			parent: dir.parent.to_string(),
			color: dir.color.into(),
			favorite_rank: dir.favorite_rank,
			last_listed: dir.last_listed,
			local_data: dir.local_data.map(|o| o.to_map()),
			meta: if let DBDirMeta::Decoded(meta) = dir.meta {
				Some(FfiDirMeta {
					name: meta.name,
					created: meta.created,
				})
			} else {
				None
			},
		}
	}
}

impl From<DBDirObject> for FfiDir {
	fn from(dir: DBDirObject) -> Self {
		match dir {
			DBDirObject::Dir(dir) => dir.into(),
			DBDirObject::Root(root) => FfiDir {
				uuid: root.uuid.to_string(),
				parent: String::new(),
				color: None,
				favorite_rank: 0,
				last_listed: root.last_listed,
				local_data: None,
				meta: None,
			},
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

impl From<DBRoot> for FfiRoot {
	fn from(root: DBRoot) -> Self {
		FfiRoot {
			uuid: root.uuid.to_string(),
			storage_used: root.storage_used,
			max_storage: root.max_storage,
			last_updated: root.last_updated,
			last_listed: root.last_listed,
		}
	}
}

#[derive(uniffi::Enum, PartialEq, Eq, Debug, Clone)]
pub enum FfiObject {
	File(FfiFile),
	Dir(FfiDir),
	Root(FfiRoot),
}

impl From<DBObject> for FfiObject {
	fn from(obj: DBObject) -> Self {
		match obj {
			DBObject::File(file) => FfiObject::File(file.into()),
			DBObject::Dir(dir) => FfiObject::Dir(dir.into()),
			DBObject::Root(root) => FfiObject::Root(root.into()),
		}
	}
}

#[derive(uniffi::Enum, PartialEq, Eq, Debug, Clone)]
pub enum FfiNonRootObject {
	File(FfiFile),
	Dir(FfiDir),
}

impl From<DBNonRootObject> for FfiNonRootObject {
	fn from(obj: DBNonRootObject) -> Self {
		match obj {
			DBNonRootObject::File(file) => FfiNonRootObject::File(file.into()),
			DBNonRootObject::Dir(dir) => FfiNonRootObject::Dir(dir.into()),
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FfiId(pub String);

impl FfiId {
	pub fn join(&self, other: &str) -> Self {
		let mut new = String::with_capacity(self.0.len() + other.len() + 1);
		self.0.clone_into(&mut new);
		if !new.ends_with('/') {
			new.push('/');
		}
		new.push_str(other);
		FfiId(new)
	}

	pub fn parent(&self) -> Self {
		let mut new = self.0.clone();
		if let Some(last_slash) = new.rfind('/') {
			new.truncate(last_slash);
		} else {
			new.clear(); // If no slash found, return empty path
		}
		FfiId(new)
	}
}

impl From<String> for FfiId {
	fn from(path: String) -> Self {
		FfiId(path)
	}
}

impl From<&str> for FfiId {
	fn from(path: &str) -> Self {
		FfiId(path.to_string())
	}
}

impl std::fmt::Display for FfiId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

#[derive(Debug)]
pub struct UuidFfiId<'a> {
	pub full_path: &'a str,
	pub uuid: Option<UuidStr>,
}

#[derive(Debug)]
pub struct PathFfiId<'a> {
	pub full_path: &'a str,
	pub root_uuid: UuidStr,
	pub inner_path: &'a str,
	pub name_or_uuid: &'a str,
}

#[derive(Debug)]
pub enum ParsedFfiId<'a> {
	Trash(UuidFfiId<'a>),
	Path(PathFfiId<'a>),
	Recents(UuidFfiId<'a>),
}

impl FfiId {
	pub fn as_path(&self) -> Result<PathFfiId<'_>, CacheError> {
		match self.as_parsed()? {
			ParsedFfiId::Trash(_) | ParsedFfiId::Recents(_) => Err(CacheError::conversion(
				format!("Expected PathFfiId, got: {}", self.0),
			)),
			ParsedFfiId::Path(path_ffi_id) => Ok(path_ffi_id),
		}
	}

	pub(crate) fn as_parsed(&self) -> Result<ParsedFfiId<'_>, CacheError> {
		let mut iter = self.0.path_iter();
		let (root, remaining) = iter
			.next()
			.ok_or_else(|| CacheError::conversion("Path must not be empty"))?;

		match root {
			"trash" => Ok(ParsedFfiId::Trash(UuidFfiId {
				full_path: self.0.as_str(),
				uuid: iter.last().map(|(s, _)| UuidStr::from_str(s)).transpose()?,
			})),
			"recents" => Ok(ParsedFfiId::Recents(UuidFfiId {
				full_path: self.0.as_str(),
				uuid: iter.last().map(|(s, _)| UuidStr::from_str(s)).transpose()?,
			})),
			_ => Ok(ParsedFfiId::Path(PathFfiId {
				full_path: self.0.as_str(),
				root_uuid: UuidStr::from_str(root).map_err(|e| {
					CacheError::conversion(format!("Invalid root UUID: {root} error: {e} "))
				})?,
				inner_path: remaining,
				name_or_uuid: iter.last().unwrap_or_default().0,
			})),
		}
	}
}

uniffi::custom_type!(FfiId, String, {
	lower: |s| s.0,
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FfiTrashPath(pub String);

impl From<String> for FfiTrashPath {
	fn from(path: String) -> Self {
		FfiTrashPath(path)
	}
}
impl std::fmt::Display for FfiTrashPath {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

uniffi::custom_type!(FfiTrashPath, String, {
	lower: |s| s.0,
});

impl FfiTrashPath {
	pub fn uuid(&self) -> Option<&str> {
		self.0.split('/').next_back()
	}
}

#[derive(uniffi::Record, Debug)]
pub struct QueryChildrenResponse {
	pub objects: Vec<FfiNonRootObject>,
	pub parent: FfiDir,
}

#[derive(uniffi::Record, Debug)]
pub struct QueryNonDirChildrenResponse {
	pub objects: Vec<FfiNonRootObject>,
	pub millis_since_updated: Option<u64>,
}

#[derive(uniffi::Record)]
pub struct DownloadResponse {
	pub path: String,
	pub file: FfiFile,
}

#[derive(uniffi::Record)]
pub struct CreateFileResponse {
	pub path: String,
	pub file: FfiFile,
	pub id: FfiId,
}

#[derive(uniffi::Record, Debug)]
pub struct FileWithPathResponse {
	pub file: FfiFile,
	pub id: FfiId,
}

#[derive(uniffi::Record, Debug)]
pub struct DirWithPathResponse {
	pub dir: FfiDir,
	pub id: FfiId,
}

#[derive(uniffi::Record, Debug)]
pub struct ObjectWithPathResponse {
	pub object: FfiObject,
	pub id: FfiId,
}

#[derive(uniffi::Record, Debug)]
pub struct UploadFileInfo {
	pub name: String,
	pub creation: Option<i64>,
	pub modification: Option<i64>,
	pub mime: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
#[repr(i8)]
pub enum ItemType {
	Root,
	Dir,
	File,
}

#[derive(uniffi::Record, Debug)]
pub struct SearchQueryArgs {
	pub name: Option<String>,
	pub item_type: Option<ItemType>,
	pub exclude_media_on_device: bool, // currently ignored
	pub mime_types: Vec<String>,
	pub file_size_min: Option<u64>,
	pub last_modified_min: Option<u64>,
}

#[derive(uniffi::Record, Debug)]
pub struct SearchQueryResponseEntry {
	pub object: FfiNonRootObject,
	pub path: String,
}
