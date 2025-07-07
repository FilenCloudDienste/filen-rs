use std::{borrow::Cow, sync::Arc};

use filen_types::fs::{ParentUuid, UuidStr};
use futures::AsyncRead;

use crate::{
	api,
	auth::Client,
	consts::CHUNK_SIZE_U64,
	crypto::shared::MetaCrypter,
	error::Error,
	fs::{HasUUID, dir::HasUUIDContents},
};

use super::{
	BaseFile, FileBuilder, RemoteFile,
	meta::FileMeta,
	read::FileReader,
	traits::{File, SetFileMeta},
	write::FileWriter,
};

impl Client {
	pub async fn trash_file(&self, file: &mut RemoteFile) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::file::trash::post(
			self.client(),
			&api::v3::file::trash::Request { uuid: file.uuid() },
		)
		.await?;
		file.parent = ParentUuid::Trash;
		Ok(())
	}

	pub async fn restore_file(&self, file: &RemoteFile) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::file::restore::post(
			self.client(),
			&api::v3::file::restore::Request { uuid: file.uuid() },
		)
		.await
	}

	pub async fn delete_file_permanently(&self, file: RemoteFile) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::file::delete::permanent::post(
			self.client(),
			&api::v3::file::delete::permanent::Request { uuid: file.uuid() },
		)
		.await
	}

	pub async fn move_file(
		&self,
		file: &mut RemoteFile,
		new_parent: &impl HasUUIDContents,
	) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::file::r#move::post(
			self.client(),
			&api::v3::file::r#move::Request {
				uuid: file.uuid(),
				new_parent: new_parent.uuid(),
			},
		)
		.await?;
		file.parent = new_parent.uuid().into();
		Ok(())
	}

	pub async fn update_file_metadata(
		&self,
		file: &mut RemoteFile,
		new_meta: FileMeta<'_>,
	) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::file::metadata::post(
			self.client(),
			&api::v3::file::metadata::Request {
				uuid: file.uuid(),
				name: Cow::Borrowed(&self.crypter().encrypt_meta(&new_meta.name)?),
				name_hashed: Cow::Borrowed(&self.hash_name(&new_meta.name)),
				metadata: Cow::Borrowed(
					&self
						.crypter()
						.encrypt_meta(&serde_json::to_string(&new_meta)?)?,
				),
			},
		)
		.await?;

		file.set_meta(new_meta);

		self.update_maybe_connected_item(file).await?;
		Ok(())
	}

	pub async fn get_file(&self, uuid: UuidStr) -> Result<RemoteFile, Error> {
		let response = api::v3::file::post(self.client(), &api::v3::file::Request { uuid }).await?;
		let meta = FileMeta::from_encrypted(&response.metadata, self.crypter(), response.version)?;
		Ok(RemoteFile::from_meta(
			uuid,
			response.parent,
			response.size,
			response.size.div_ceil(CHUNK_SIZE_U64),
			response.region,
			response.bucket,
			false,
			meta,
		))
	}

	pub async fn file_exists(
		&self,
		name: impl AsRef<str>,
		parent: &impl HasUUIDContents,
	) -> Result<Option<UuidStr>, Error> {
		api::v3::file::exists::post(
			self.client(),
			&api::v3::file::exists::Request {
				name_hashed: self.hash_name(name.as_ref()),
				parent: parent.uuid().into(),
			},
		)
		.await
		.map(|r| r.0)
	}

	pub fn get_file_reader<'a>(&'a self, file: &'a dyn File) -> impl AsyncRead + 'a {
		FileReader::new(file, self)
	}

	pub fn make_file_builder(
		&self,
		name: impl Into<String>,
		parent: &impl HasUUIDContents,
	) -> FileBuilder {
		FileBuilder::new(name, parent, self)
	}

	pub(crate) fn inner_get_file_writer<'a>(
		&'a self,
		file: Arc<BaseFile>,
		callback: Option<Arc<dyn Fn(u64) + Send + Sync + 'a>>,
		size: Option<u64>,
	) -> Result<FileWriter<'a>, Error> {
		if file.root.name.is_empty() {
			let name = match Arc::try_unwrap(file).map(|f| f.root.name) {
				Ok(name) => name,
				Err(file) => file.name().to_string(),
			};
			Err(Error::InvalidName(name))
		} else {
			Ok(FileWriter::new(file, self, callback, size))
		}
	}

	pub fn get_file_writer(&self, file: impl Into<Arc<BaseFile>>) -> Result<FileWriter<'_>, Error> {
		self.inner_get_file_writer(file.into(), None, None)
	}

	pub fn get_file_writer_with_callback<'a>(
		&'a self,
		file: impl Into<Arc<BaseFile>>,
		callback: Arc<dyn Fn(u64) + Send + Sync + 'a>,
	) -> Result<FileWriter<'a>, Error> {
		self.inner_get_file_writer(file.into(), Some(callback), None)
	}
}
