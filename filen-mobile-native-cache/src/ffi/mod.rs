use crate::{
	DBDir, DBFile, DBObject,
	sql::{DBDirObject, DBNonRootObject, DBRoot},
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
	pub favorited: bool,
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
			favorited: file.favorited,
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

impl From<DBDir> for FfiDir {
	fn from(dir: DBDir) -> Self {
		FfiDir {
			uuid: dir.uuid.to_string(),
			parent: dir.parent.to_string(),
			name: dir.name,
			color: dir.color,
			created: dir.created,
			favorited: dir.favorited,
			last_listed: dir.last_listed,
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
				favorited: false,
				last_listed: 0,
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

#[derive(Clone)]
pub struct PathWithRoot(pub String);

impl From<String> for PathWithRoot {
	fn from(path: String) -> Self {
		PathWithRoot(path)
	}
}

impl From<&str> for PathWithRoot {
	fn from(path: &str) -> Self {
		PathWithRoot(path.to_string())
	}
}

impl std::fmt::Display for PathWithRoot {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

uniffi::custom_type!(PathWithRoot, String, {
	lower: |s| s.0,
});
