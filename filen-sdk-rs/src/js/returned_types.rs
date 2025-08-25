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
pub enum NonRootObject {
	#[serde(rename = "dir")]
	Dir(Dir),
	#[serde(rename = "file")]
	File(File),
}

#[cfg(feature = "node")]
super::napi_to_json_impl!(NonRootObject);

impl From<NonRootFSObject<'_>> for NonRootObject {
	fn from(obj: NonRootFSObject<'_>) -> Self {
		match obj {
			NonRootFSObject::Dir(dir) => NonRootObject::Dir(dir.into_owned().into()),
			NonRootFSObject::File(file) => NonRootObject::File(file.into_owned().into()),
		}
	}
}
