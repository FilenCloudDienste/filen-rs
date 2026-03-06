use crate::{crypto::error::ConversionError, fs::categories::Category};

pub(super) mod common;
pub(super) mod linked;
pub(super) mod normal;
pub(super) mod shared;

pub(crate) trait CategoryJSExt: Category {
	type RootJS: From<Self::Root> + Into<Self::Root>;
	type DirJS: From<Self::Dir> + Into<Self::Dir>;
	type FileJS: From<Self::File> + TryInto<Self::File, Error = ConversionError>;
	type RootFileJS: From<Self::RootFile> + TryInto<Self::RootFile, Error = ConversionError>;
}
