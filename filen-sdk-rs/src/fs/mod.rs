use std::borrow::Cow;

use dir::{Directory, DirectoryType};
use file::RemoteFile;
use filen_types::crypto::EncryptedString;

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

impl HasMeta for NonRootFSObject<'_> {
	fn name(&self) -> &str {
		match self {
			NonRootFSObject::Dir(dir) => dir.name(),
			NonRootFSObject::File(file) => file.name(),
		}
	}

	fn meta(&self, crypter: impl MetaCrypter) -> Result<EncryptedString, ConversionError> {
		match self {
			NonRootFSObject::Dir(dir) => dir.meta(crypter),
			NonRootFSObject::File(file) => file.meta(crypter),
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

pub trait HasParent {
	fn parent(&self) -> uuid::Uuid;
}

pub trait HasMeta {
	fn name(&self) -> &str;
	fn meta(&self, crypter: impl MetaCrypter) -> Result<EncryptedString, ConversionError>;
}

pub async fn list_dir(
	client: &Client,
	dir: &impl HasContents,
) -> Result<(Vec<Directory>, Vec<RemoteFile>), Error> {
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
	parent: &impl HasContents,
	name: impl Into<String>,
) -> Result<dir::Directory, Error> {
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
	crate::search::update_search_hashes_for_item(client, &dir).await?;
	Ok(dir)
}

pub async fn trash_dir(client: &Client, dir: Directory) -> Result<(), Error> {
	api::v3::dir::trash::post(
		client.client(),
		&api::v3::dir::trash::Request { uuid: dir.uuid() },
	)
	.await?;
	Ok(())
}

pub async fn find_item_in_dir(
	client: &Client,
	dir: &impl HasContents,
	name: impl AsRef<str>,
) -> Result<Option<FSObjectType<'static>>, Error> {
	let (dirs, files) = list_dir(client, dir).await?;
	if let Some(dir) = dirs.into_iter().find(|d| d.name() == name.as_ref()) {
		return Ok(Some(FSObjectType::Dir(Cow::Owned(dir))));
	}
	if let Some(file) = files.into_iter().find(|f| f.name() == name.as_ref()) {
		return Ok(Some(FSObjectType::File(Cow::Owned(file))));
	}
	Ok(None)
}

pub async fn find_item_at_path(
	client: &Client,
	path: impl AsRef<str>,
) -> Result<Option<FSObjectType>, Error> {
	let mut curr_dir = DirectoryType::Root(Cow::Borrowed(client.root()));
	let mut curr_path = String::with_capacity(path.as_ref().len());
	let mut path_iter = path.as_ref().split('/');
	while let Some(component) = path_iter.next() {
		if component.is_empty() {
			continue;
		}
		match find_item_in_dir(client, &curr_dir, component).await {
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
		DirectoryType::Root(_) => Ok(Some(FSObjectType::Root(Cow::Borrowed(client.root())))),
		DirectoryType::Dir(dir) => Ok(Some(FSObjectType::Dir(dir))),
	}
}

pub async fn find_or_create_dir(
	client: &Client,
	path: impl AsRef<str>,
) -> Result<DirectoryType, Error> {
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
			return Err(Error::Custom(format!(
				"find_or_create_dir path {}/{} is a file when trying to create dir {}",
				curr_path,
				component,
				path.as_ref()
			)));
		}

		let new_dir = create_dir(client, &curr_dir, component).await?;
		curr_dir = DirectoryType::Dir(Cow::Owned(new_dir));
		curr_path.push_str(component);
		curr_path.push('/');
	}
	Ok(curr_dir)
}

pub async fn list_dir_recursive(
	client: &Client,
	dir: &impl HasContents,
) -> Result<(Vec<Directory>, Vec<RemoteFile>), Error> {
	let response = api::v3::dir::download::post(
		client.client(),
		&api::v3::dir::download::Request {
			uuid: dir.uuid(),
			skip_cache: false,
		},
	)
	.await?;

	let dirs = response
		.dirs
		.into_iter()
		.map(|d| dir::Directory::try_from_encrypted(d, client.crypter()))
		.filter_map(|d| match d {
			Ok(Some(d)) => Some(Ok(d)),
			Ok(None) => None,
			Err(e) => Some(Err(e)),
		})
		.collect::<Result<Vec<_>, _>>()?;

	let files = response
		.files
		.into_iter()
		.map(|f| file::RemoteFile::from_encrypted(f, client.crypter()))
		.collect::<Result<Vec<_>, _>>()?;
	Ok((dirs, files))
}

pub async fn dir_exists(
	client: &Client,
	parent: &impl HasContents,
	name: impl AsRef<str>,
) -> Result<Option<uuid::Uuid>, Error> {
	let response = api::v3::dir::exists::post(
		client.client(),
		&api::v3::dir::exists::Request {
			parent: parent.uuid(),
			name_hashed: client.hash_name(name.as_ref()),
		},
	)
	.await?;
	Ok(match (response.exists, response.uuid) {
		(true, Some(uuid)) => Some(uuid),
		(false, _) => None,
		(true, None) => {
			return Err(Error::Custom(
				"dir_exists returned true but no uuid".to_owned(),
			));
		}
	})
}

// todo add overwriting
// I want to add this in tandem with a locking mechanism so that I avoid race conditions
pub async fn move_dir(
	client: &Client,
	dir: &mut Directory,
	new_parent: &impl HasContents,
) -> Result<(), Error> {
	api::v3::dir::r#move::post(
		client.client(),
		&api::v3::dir::r#move::Request {
			uuid: dir.uuid(),
			to: new_parent.uuid(),
		},
	)
	.await?;
	dir.parent = new_parent.uuid();
	Ok(())
}

pub async fn get_dir_size(
	client: &Client,
	dir: &impl HasContents,
	trash: bool,
) -> Result<api::v3::dir::size::Response, Error> {
	let response = api::v3::dir::size::post(
		client.client(),
		&api::v3::dir::size::Request {
			uuid: dir.uuid(),
			sharer_id: None,
			receiver_id: None,
			trash,
		},
	)
	.await?;
	Ok(response)
}
