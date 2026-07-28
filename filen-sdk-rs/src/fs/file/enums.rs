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
	AnonymousRemoteFile, RemoteFile,
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
	/// A file being read, whatever surface it was listed from. Reading needs
	/// no identity, so the stable-id slot is dropped on the way in — see
	/// [`AnonymousRemoteFile`].
	File(Cow<'a, AnonymousRemoteFile>),
	Shared(Cow<'a, SharedRootFile>),
	Linked(Cow<'a, LinkedFile>),
}

impl From<RemoteFile> for RemoteFileType<'static> {
	fn from(value: RemoteFile) -> Self {
		Self::File(Cow::Owned(value.into_anonymous()))
	}
}

// Unlike the anonymous instantiation (which the `CowFrom` derive borrows), a
// drive file has to be rebuilt to shed its stable id, so this clones. Reading
// is I/O-bound and the alternative is threading the id through a type that has
// no use for it.
impl<'a> From<&'a RemoteFile> for RemoteFileType<'a> {
	fn from(value: &'a RemoteFile) -> Self {
		Self::File(Cow::Owned(value.clone().into_anonymous()))
	}
}

impl HasType for RemoteFileType<'_> {
	fn object_type(&self) -> ObjectType {
		ObjectType::File
	}
}
