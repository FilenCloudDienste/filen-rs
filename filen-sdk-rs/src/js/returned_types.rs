use serde::Serialize;

use crate::{
	fs::NonRootFSObject,
	js::{Dir, File},
};

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use tsify::Tsify;

#[derive(Serialize)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), derive(Tsify))]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	tsify(into_wasm_abi)
)]
#[serde(tag = "type")]
#[cfg_attr(test, derive(Clone, Debug, PartialEq, Eq))]
pub enum NonRootItemTagged {
	#[serde(rename = "dir")]
	Dir(Dir),
	#[serde(rename = "file")]
	File(File),
}

#[cfg(feature = "node")]
super::napi_to_json_impl!(NonRootObject);

impl From<NonRootFSObject<'_>> for NonRootItemTagged {
	fn from(obj: NonRootFSObject<'_>) -> Self {
		match obj {
			NonRootFSObject::Dir(dir) => NonRootItemTagged::Dir(dir.into_owned().into()),
			NonRootFSObject::File(file) => NonRootItemTagged::File(file.into_owned().into()),
		}
	}
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[derive(Serialize, Tsify)]
#[tsify(into_wasm_abi, large_number_types_as_bigints)]
pub struct DirSizeResponse {
	pub size: u64,
	pub files: u64,
	pub dirs: u64,
}
