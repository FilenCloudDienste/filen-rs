use std::borrow::Cow;

use dir::{Directory, DirectoryType};
use file::RemoteFile;
use filen_types::crypto::{EncryptedString, rsa::RSAEncryptedString};
use rsa::RsaPublicKey;

use crate::{
	api,
	auth::Client,
	crypto::{error::ConversionError, shared::MetaCrypter},
	error::Error,
};

pub mod dir;
pub mod file;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FSObjectType<'a> {
	Dir(Cow<'a, dir::Directory>),
	Root(Cow<'a, dir::RootDirectory>),
	File(Cow<'a, file::RemoteFile>),
}

impl<'a> From<DirectoryType<'a>> for FSObjectType<'a> {
	fn from(dir: DirectoryType<'a>) -> Self {
		match dir {
			DirectoryType::Root(dir) => FSObjectType::Root(dir),
			DirectoryType::Dir(dir) => FSObjectType::Dir(dir),
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NonRootFSObject<'a> {
	Dir(Cow<'a, dir::Directory>),
	File(Cow<'a, file::RemoteFile>),
}

impl<'a> From<&'a RemoteFile> for NonRootFSObject<'a> {
	fn from(file: &'a RemoteFile) -> Self {
		NonRootFSObject::File(Cow::Borrowed(file))
	}
}

impl From<RemoteFile> for NonRootFSObject<'_> {
	fn from(file: RemoteFile) -> Self {
		NonRootFSObject::File(Cow::Owned(file))
	}
}

impl<'a> From<&'a Directory> for NonRootFSObject<'a> {
	fn from(dir: &'a Directory) -> Self {
		NonRootFSObject::Dir(Cow::Borrowed(dir))
	}
}

impl From<Directory> for NonRootFSObject<'_> {
	fn from(dir: Directory) -> Self {
		NonRootFSObject::Dir(Cow::Owned(dir))
	}
}

impl NonRootObject for NonRootFSObject<'_> {
	fn name(&self) -> &str {
		match self {
			NonRootFSObject::Dir(dir) => dir.name(),
			NonRootFSObject::File(file) => file.name(),
		}
	}

	fn get_meta_string(&self) -> String {
		match self {
			NonRootFSObject::Dir(dir) => dir.get_meta_string(),
			NonRootFSObject::File(file) => file.get_meta_string(),
		}
	}

	fn parent(&self) -> uuid::Uuid {
		match self {
			NonRootFSObject::Dir(dir) => dir.parent(),
			NonRootFSObject::File(file) => file.parent(),
		}
	}
}

impl HasUUID for NonRootFSObject<'_> {
	fn uuid(&self) -> uuid::Uuid {
		match self {
			NonRootFSObject::Dir(dir) => dir.uuid(),
			NonRootFSObject::File(file) => file.uuid(),
		}
	}
}

pub trait HasUUID {
	fn uuid(&self) -> uuid::Uuid;
}

pub trait HasContents: HasUUID {}

pub trait NonRootObject {
	fn name(&self) -> &str;
	fn get_meta_string(&self) -> String;
	fn parent(&self) -> uuid::Uuid;
	fn get_encrypted_meta(
		&self,
		crypter: &impl MetaCrypter,
	) -> Result<EncryptedString, ConversionError> {
		crypter.encrypt_meta(&self.get_meta_string())
	}
	fn get_rsa_encrypted_meta(
		&self,
		public_key: &RsaPublicKey,
	) -> Result<RSAEncryptedString, rsa::Error> {
		let meta = self.get_meta_string();
		crate::crypto::rsa::encrypt_with_public_key(public_key, meta.as_bytes())
	}
}

impl Client {
	pub async fn find_item_at_path(
		&self,
		path: impl AsRef<str>,
	) -> Result<Option<FSObjectType>, Error> {
		let mut curr_dir = DirectoryType::Root(Cow::Borrowed(self.root()));
		let mut curr_path = String::with_capacity(path.as_ref().len());
		let mut path_iter = path.as_ref().split('/');
		while let Some(component) = path_iter.next() {
			if component.is_empty() {
				continue;
			}
			match self.find_item_in_dir(&curr_dir, component).await {
				Ok(Some(FSObjectType::Dir(dir))) => {
					curr_dir = DirectoryType::Dir(Cow::Owned(dir.into_owned()));
					curr_path.push_str(component);
					curr_path.push('/');
					continue;
				}
				Ok(Some(FSObjectType::File(file))) => {
					if let Some(next) = path_iter.next() {
						return Err(Error::Custom(format!(
							"Path {} is a file, but tried to access {}/{}",
							curr_path, curr_path, next
						)));
					}
					return Ok(Some(FSObjectType::File(Cow::Owned(file.into_owned()))));
				}
				Err(e) => return Err(e),
				_ => {}
			}
			return Ok(None);
		}
		match curr_dir {
			DirectoryType::Root(_) => Ok(Some(FSObjectType::Root(Cow::Borrowed(self.root())))),
			DirectoryType::Dir(dir) => Ok(Some(FSObjectType::Dir(dir))),
		}
	}

	pub async fn empty_trash(&self) -> Result<(), Error> {
		api::v3::trash::empty::post(self.client()).await?;
		Ok(())
	}
}
