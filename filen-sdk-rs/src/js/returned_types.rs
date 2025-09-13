use serde::Serialize;

use crate::{
	fs::NonRootFSObject,
	js::{Dir, File},
};

use tsify::Tsify;

#[derive(Serialize, Tsify)]
#[tsify(into_wasm_abi)]
#[serde(tag = "type")]
#[cfg_attr(test, derive(Clone, Debug, PartialEq, Eq))]
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

#[derive(Serialize, Tsify)]
#[tsify(into_wasm_abi, large_number_types_as_bigints)]
pub struct DirSizeResponse {
	pub size: u64,
	pub files: u64,
	pub dirs: u64,
}
