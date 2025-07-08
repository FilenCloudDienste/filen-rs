use std::{collections::HashMap, str::FromStr};

use filen_sdk_rs::util::PathIteratorExt;
use filen_types::fs::UuidStr;

use crate::{
	CacheError,
	sql::{DBDir, DBDirObject, DBFile, DBNonRootObject, DBObject, DBRoot},
};

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
	pub favorite_rank: i64,
	pub hash: Option<Vec<u8>>,

	pub local_data: Option<HashMap<String, String>>,
}

impl From<DBFile> for FfiFile {
	fn from(file: DBFile) -> Self {
		FfiFile {
			uuid: file.uuid.to_string(),
			parent: file.parent.to_string(),
			name: file.name,
			mime: file.mime,
			created: file.created,
			modified: file.modified,
			size: file.size,
			favorite_rank: file.favorite_rank,
			hash: file.hash.map(Vec::from),
			local_data: file.local_data.map(|o| o.to_map()),
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
			name: dir.name,
			color: dir.color,
			created: dir.created,
			favorite_rank: dir.favorite_rank,
			last_listed: dir.last_listed,
			local_data: dir.local_data.map(|o| o.to_map()),
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
				name: String::new(),
				color: None,
				created: None,
				favorite_rank: 0,
				last_listed: root.last_listed,
				local_data: None,
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
pub struct FfiPathWithRoot(pub String);

impl FfiPathWithRoot {
	pub fn join(&self, other: &str) -> Self {
		let mut new = String::with_capacity(self.0.len() + other.len() + 1);
		self.0.clone_into(&mut new);
		if !new.ends_with('/') {
			new.push('/');
		}
		new.push_str(other);
		FfiPathWithRoot(new)
	}

	pub fn parent(&self) -> Self {
		let mut new = self.0.clone();
		if let Some(last_slash) = new.rfind('/') {
			new.truncate(last_slash);
		} else {
			new.clear(); // If no slash found, return empty path
		}
		FfiPathWithRoot(new)
	}
}

impl From<String> for FfiPathWithRoot {
	fn from(path: String) -> Self {
		FfiPathWithRoot(path)
	}
}

impl From<&str> for FfiPathWithRoot {
	fn from(path: &str) -> Self {
		FfiPathWithRoot(path.to_string())
	}
}

impl std::fmt::Display for FfiPathWithRoot {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

#[derive(Debug)]
pub struct PathValues<'a> {
	pub root_uuid: UuidStr,
	pub full_path: &'a str,
	pub inner_path: &'a str,
	pub name: &'a str,
}

#[derive(Debug)]
pub struct TrashValues<'a> {
	pub full_path: &'a str,
	pub inner_path: &'a str,
	pub uuid: UuidStr,
}

#[derive(Debug)]
pub enum MaybeTrashValues<'a> {
	Trash(TrashValues<'a>),
	Path(PathValues<'a>),
}

impl FfiPathWithRoot {
	pub fn as_path_values(&self) -> Result<PathValues, CacheError> {
		let mut iter = self.0.path_iter();
		let (root_uuid_str, remaining) = iter
			.next()
			.ok_or_else(|| CacheError::conversion("Path must start with a root UUID"))?;

		Ok(PathValues {
			root_uuid: UuidStr::from_str(root_uuid_str).map_err(|e| {
				CacheError::conversion(format!("Invalid root UUID: {root_uuid_str} error: {e} "))
			})?,
			full_path: self.0.as_str(),
			inner_path: remaining,
			name: iter.last().unwrap_or_default().0,
		})
	}

	pub fn as_maybe_trash_values(&self) -> Result<MaybeTrashValues, CacheError> {
		let mut iter = self.0.path_iter();
		let (root_uuid_str, remaining) = iter
			.next()
			.ok_or_else(|| CacheError::conversion("Path must start with a root UUID"))?;

		match root_uuid_str {
			"trash" => Ok(MaybeTrashValues::Trash(TrashValues {
				full_path: self.0.as_str(),
				inner_path: remaining,
				uuid: UuidStr::from_str(iter.last().unwrap_or_default().0)?,
			})),
			_ => Ok(MaybeTrashValues::Path(self.as_path_values()?)),
		}
	}
}

uniffi::custom_type!(FfiPathWithRoot, String, {
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

#[derive(uniffi::Record)]
pub struct DownloadResponse {
	pub path: String,
	pub file: FfiFile,
}

#[derive(uniffi::Record)]
pub struct CreateFileResponse {
	pub path: String,
	pub file: FfiFile,
	pub id: FfiPathWithRoot,
}

#[derive(uniffi::Record, Debug)]
pub struct FileWithPathResponse {
	pub file: FfiFile,
	pub id: FfiPathWithRoot,
}

#[derive(uniffi::Record, Debug)]
pub struct DirWithPathResponse {
	pub dir: FfiDir,
	pub id: FfiPathWithRoot,
}

#[derive(uniffi::Record, Debug)]
pub struct ObjectWithPathResponse {
	pub object: FfiObject,
	pub id: FfiPathWithRoot,
}

#[derive(uniffi::Record, Debug)]
pub struct UploadFileInfo {
	pub name: String,
	pub creation: Option<i64>,
	pub modification: Option<i64>,
	pub mime: Option<String>,
}
