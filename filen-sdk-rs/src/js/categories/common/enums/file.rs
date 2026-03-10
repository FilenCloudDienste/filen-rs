use std::borrow::Cow;

use filen_macros::js_type;

use crate::{
	crypto::error::ConversionError,
	fs::file::enums::RemoteFileType,
	js::{File, LinkedFile, SharedFile},
};

#[js_type(import, wasm_all)]
pub enum AnyFile {
	Linked(LinkedFile),
	Shared(SharedFile),
	File(File),
}

impl From<RemoteFileType<'static>> for AnyFile {
	fn from(value: RemoteFileType<'static>) -> Self {
		match value {
			RemoteFileType::File(file) => Self::File(file.into_owned().into()),
			RemoteFileType::Shared(shared) => Self::Shared(shared.into_owned().into()),
			RemoteFileType::Linked(linked) => Self::Linked(linked.into_owned().into()),
		}
	}
}

impl TryFrom<AnyFile> for RemoteFileType<'static> {
	type Error = ConversionError;
	fn try_from(value: AnyFile) -> Result<Self, Self::Error> {
		Ok(match value {
			AnyFile::File(file) => Self::File(Cow::Owned(file.try_into()?)),
			AnyFile::Shared(shared) => Self::Shared(Cow::Owned(shared.try_into()?)),
			AnyFile::Linked(linked) => Self::Linked(Cow::Owned(linked.try_into()?)),
		})
	}
}

#[cfg(feature = "http-provider")]
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum ProviderAnyFile {
	Linked(LinkedFile),
	Shared(SharedFile),
	File(File),
}

#[cfg(feature = "http-provider")]
impl From<RemoteFileType<'static>> for ProviderAnyFile {
	fn from(value: RemoteFileType<'static>) -> Self {
		match value {
			RemoteFileType::File(file) => Self::File(file.into_owned().into()),
			RemoteFileType::Shared(shared) => Self::Shared(shared.into_owned().into()),
			RemoteFileType::Linked(linked) => Self::Linked(linked.into_owned().into()),
		}
	}
}

#[cfg(feature = "http-provider")]
impl TryFrom<ProviderAnyFile> for RemoteFileType<'static> {
	type Error = ConversionError;
	fn try_from(value: ProviderAnyFile) -> Result<Self, Self::Error> {
		Ok(match value {
			ProviderAnyFile::File(file) => Self::File(Cow::Owned(file.try_into()?)),
			ProviderAnyFile::Shared(shared) => Self::Shared(Cow::Owned(shared.try_into()?)),
			ProviderAnyFile::Linked(linked) => Self::Linked(Cow::Owned(linked.try_into()?)),
		})
	}
}
