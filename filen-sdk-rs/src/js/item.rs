use std::borrow::Cow;

use serde::Deserialize;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use tsify::Tsify;

use crate::{
	fs::{UnsharedFSObject, file::RemoteFile},
	js::{Dir, File, Root},
};

#[derive(Deserialize)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), derive(Tsify))]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	tsify(from_wasm_abi)
)]
#[serde(untagged)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub enum Item {
	File(File),
	Dir(Dir),
	Root(Root),
}

impl TryFrom<Item> for UnsharedFSObject<'static> {
	type Error = <RemoteFile as TryFrom<File>>::Error;
	fn try_from(value: Item) -> Result<Self, Self::Error> {
		Ok(match value {
			Item::Dir(dir) => UnsharedFSObject::Dir(Cow::Owned(dir.into())),
			Item::Root(root) => UnsharedFSObject::Root(Cow::Owned(root.into())),
			Item::File(file) => UnsharedFSObject::File(Cow::Owned(file.try_into()?)),
		})
	}
}
