use std::borrow::Cow;

use dir::{Directory, DirectoryType};
use file::RemoteFile;
use filen_types::crypto::EncryptedString;

use crate::{
	api,
	auth::Client,
	crypto::{error::ConversionError, shared::MetaCrypter},
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

pub enum NonRootFSObject {
	Dir(dir::Directory),
	File(file::RemoteFile),
}

pub trait HasUUID {
	fn uuid(&self) -> uuid::Uuid;
}

pub trait HasContents: HasUUID {}

pub trait HasParent {
	fn parent(&self) -> uuid::Uuid;
}

pub trait HasMeta {
	fn name(&self) -> &str;
	fn meta(&self, crypter: impl MetaCrypter) -> Result<EncryptedString, ConversionError>;
}

pub async fn list_dir(
	client: &Client,
	dir: impl HasContents,
) -> Result<(Vec<Directory>, Vec<RemoteFile>), crate::error::Error> {
	let response = api::v3::dir::content::post(
		client.client(),
		&api::v3::dir::content::Request { uuid: dir.uuid() },
	)
	.await?;

	let dirs = response
		.dirs
		.into_iter()
		.map(|d| dir::Directory::from_encrypted(d, client.crypter()))
		.collect::<Result<Vec<_>, _>>()?;

	let files = response
		.files
		.into_iter()
		.map(|f| file::RemoteFile::from_encrypted(f, client.crypter()))
		.collect::<Result<Vec<_>, _>>()?;
	Ok((dirs, files))
}

pub async fn create_dir(
	client: &Client,
	parent: impl HasContents,
	name: impl Into<String>,
) -> Result<dir::Directory, crate::error::Error> {
	let mut dir = dir::Directory::new(name.into(), parent.uuid(), chrono::Utc::now());

	let response = api::v3::dir::create::post(
		client.client(),
		&api::v3::dir::create::Request {
			uuid: dir.uuid(),
			parent: dir.parent(),
			name_hashed: client.hash_name(dir.name()),
			meta: dir.meta(client.crypter())?,
		},
	)
	.await?;
	if dir.uuid != response.uuid {
		println!("UUID mismatch: {} != {}", dir.uuid(), response.uuid);
		dir.uuid = response.uuid;
	}
	Ok(dir)
}

pub async fn trash_dir(client: &Client, dir: Directory) -> Result<(), crate::error::Error> {
	api::v3::dir::trash::post(
		client.client(),
		&api::v3::dir::trash::Request { uuid: dir.uuid() },
	)
	.await?;
	Ok(())
}

pub async fn find_item_at_path(
	client: &Client,
	path: impl AsRef<str>,
) -> Result<FSObjectType, crate::error::Error> {
	let mut curr_dir = DirectoryType::Root(Cow::Borrowed(client.root()));
	let mut curr_path = String::with_capacity(path.as_ref().len());
	let mut path_iter = path.as_ref().split('/');
	while let Some(component) = path_iter.next() {
		if component.is_empty() {
			continue;
		}
		let (dirs, files) = list_dir(client, &curr_dir).await?;
		if let Some(dir) = dirs.into_iter().find(|d| d.name() == component) {
			curr_dir = DirectoryType::Dir(Cow::Owned(dir));
			curr_path.push_str(component);
			curr_path.push('/');
			continue;
		}

		if let Some(file) = files.into_iter().find(|f| f.name() == component) {
			if let Some(next) = path_iter.next() {
				return Err(crate::error::Error::Custom(format!(
					"Path {} is a file, but tried to access {}/{}",
					curr_path, curr_path, next
				)));
			}
			return Ok(FSObjectType::File(Cow::Owned(file)));
		}
	}
	match curr_dir {
		DirectoryType::Root(_) => Ok(FSObjectType::Root(Cow::Borrowed(client.root()))),
		DirectoryType::Dir(dir) => Ok(FSObjectType::Dir(dir)),
	}
}

pub async fn find_or_create_dir(
	client: &Client,
	path: impl AsRef<str>,
) -> Result<DirectoryType, crate::error::Error> {
	let mut curr_dir = DirectoryType::Root(Cow::Borrowed(client.root()));
	let mut curr_path = String::with_capacity(path.as_ref().len());
	for component in path.as_ref().split('/') {
		if component.is_empty() {
			continue;
		}
		let (dirs, files) = list_dir(client, &curr_dir).await?;
		if let Some(dir) = dirs.into_iter().find(|d| d.name() == component) {
			curr_dir = DirectoryType::Dir(Cow::Owned(dir));
			curr_path.push_str(component);
			curr_path.push('/');
			continue;
		}

		if files.iter().any(|f| f.name() == component) {
			return Err(crate::error::Error::Custom(format!(
				"find_or_create_dir path {}/{} is a file when trying to create dir {}",
				curr_path,
				component,
				path.as_ref()
			)));
		}

		let new_dir = create_dir(client, curr_dir, component).await?;
		curr_dir = DirectoryType::Dir(Cow::Owned(new_dir));
		curr_path.push_str(component);
		curr_path.push('/');
	}
	Ok(curr_dir)
}
