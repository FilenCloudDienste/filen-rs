use serde::Serialize;

use crate::{
	fs::NonRootFSObject,
	js::{Dir, File},
};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use tsify::Tsify;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type")]
pub enum NonRootItemTagged {
	#[serde(rename = "dir")]
	Dir(Dir),
	#[serde(rename = "file")]
	File(File),
}

impl From<NonRootFSObject<'_>> for NonRootItemTagged {
	fn from(obj: NonRootFSObject<'_>) -> Self {
		match obj {
			NonRootFSObject::Dir(dir) => NonRootItemTagged::Dir(dir.into_owned().into()),
			NonRootFSObject::File(file) => NonRootItemTagged::File(file.into_owned().into()),
		}
	}
}

#[derive(Serialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DirSizeResponse {
	pub size: u64,
	pub files: u64,
	pub dirs: u64,
}

#[derive(Serialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DirsAndFiles {
	pub dirs: Vec<Dir>,
	pub files: Vec<File>,
}

#[derive(Serialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DirWithPath {
	pub path: String,
	pub dir: Dir,
}

#[derive(Serialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct FileWithPath {
	pub path: String,
	pub file: File,
}

#[derive(Serialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DirsAndFilesWithPaths {
	pub dirs: Vec<DirWithPath>,
	pub files: Vec<FileWithPath>,
}
