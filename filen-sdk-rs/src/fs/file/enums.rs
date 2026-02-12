use std::borrow::Cow;

use filen_macros::CowFrom;
use filen_types::{fs::ObjectType, traits::CowHelpers};

use crate::fs::{HasMeta, HasName, HasRemoteInfo, HasType, HasUUID, file::traits::File};

use super::{
	RemoteFile, RemoteRootFile,
	traits::{HasFileInfo, HasRemoteFileInfo},
};

#[derive(
	Debug,
	Clone,
	PartialEq,
	Eq,
	CowHelpers,
	CowFrom,
	HasUUID,
	HasName,
	HasMeta,
	HasRemoteInfo,
	HasFileInfo,
	HasRemoteFileInfo,
	File,
)]
pub enum RemoteFileType<'a> {
	File(Cow<'a, RemoteFile>),
	SharedFile(Cow<'a, RemoteRootFile>),
}

impl HasType for RemoteFileType<'_> {
	fn object_type(&self) -> ObjectType {
		ObjectType::File
	}
}
