use std::borrow::Cow;

use serde::Deserialize;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use tsify::Tsify;

use crate::{
	fs::{FSObject, file::RemoteFile},
	js::{Dir, File, Root, RootFile, RootWithMeta},
};

#[derive(Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi)
)]
#[serde(untagged)]
pub enum Item {
	File(File),
	RootFile(RootFile),
	Dir(Dir),
	RootWithMeta(RootWithMeta),
	Root(Root),
}

impl TryFrom<Item> for FSObject<'static> {
	type Error = <RemoteFile as TryFrom<File>>::Error;
	fn try_from(value: Item) -> Result<Self, Self::Error> {
		Ok(match value {
			Item::Dir(dir) => Self::Dir(Cow::Owned(dir.into())),
			Item::Root(root) => Self::Root(Cow::Owned(root.into())),
			Item::File(file) => Self::File(Cow::Owned(file.try_into()?)),
			Item::RootFile(root_file) => Self::SharedFile(Cow::Owned(root_file.try_into()?)),
			Item::RootWithMeta(dir) => Self::RootWithMeta(Cow::Owned(dir.into())),
		})
	}
}
