use std::borrow::Cow;

use filen_macros::CowFrom;
use filen_types::{fs::ObjectType, traits::CowHelpers};

use crate::{
	connect::fs::SharedRootFile,
	fs::{
		HasMeta, HasName, HasRemoteInfo, HasType, HasUUID,
		file::{LinkedFile, traits::File},
	},
};

use super::{
	RemoteFile,
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
	Shared(Cow<'a, SharedRootFile>),
	Linked(Cow<'a, LinkedFile>),
}

impl HasType for RemoteFileType<'_> {
	fn object_type(&self) -> ObjectType {
		ObjectType::File
	}
}
