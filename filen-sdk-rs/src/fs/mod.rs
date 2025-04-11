use dir::Directory;
use file::RemoteFile;
use filen_types::crypto::EncryptedString;

use crate::{
	api,
	auth::Client,
	crypto::{error::ConversionError, shared::MetaCrypter},
};

pub mod dir;
pub mod file;

pub enum FSObject {
	Dir(dir::Directory),
	Root(dir::RootDirectory),
	File(file::RemoteFile),
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

// pub fn find_item_at_path(
// 	client: &Client,
// 	path: impl AsRef<Path>,
// ) -> Result<FSObject, crate::error::Error> {
// 	for component in path.as_ref().canonicalize()?.components() {}
// 	todo!();
// }
