use filen_macros::js_type;

use crate::js::{Dir, File, LinkedDir, NonRootDirTagged};

#[js_type(export)]
pub struct DirSizeResponse {
	pub size: u64,
	pub files: u64,
	pub dirs: u64,
}

#[js_type(export, no_deser)]
pub struct DirsAndFiles {
	pub dirs: Vec<NonRootDirTagged>,
	pub files: Vec<File>,
}

#[js_type(export)]
pub struct NormalDirsAndFiles {
	pub dirs: Vec<Dir>,
	pub files: Vec<File>,
}

#[js_type(export)]
pub struct LinkedDirsAndFiles {
	pub dirs: Vec<LinkedDir>,
	pub files: Vec<File>,
}

#[js_type(export)]
pub struct NormalDirWithPath {
	pub path: String,
	pub dir: Dir,
}

#[js_type(export, no_deser)]
pub struct DirWithPath {
	pub path: String,
	pub dir: NonRootDirTagged,
}

#[js_type(export)]
pub struct FileWithPath {
	pub path: String,
	pub file: File,
}

#[js_type(export, no_deser)]
pub struct DirsAndFilesWithPaths {
	pub dirs: Vec<DirWithPath>,
	pub files: Vec<FileWithPath>,
}

#[js_type(export)]
pub struct NormalDirsAndFilesWithPaths {
	pub dirs: Vec<NormalDirWithPath>,
	pub files: Vec<FileWithPath>,
}
