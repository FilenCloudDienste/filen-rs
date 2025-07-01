use std::borrow::Cow;
#[cfg(feature = "tokio")]
use std::path::Path;

use chrono::{DateTime, Utc};
use filen_types::fs::UuidStr;
use futures::TryFutureExt;

use crate::{
	api,
	auth::Client,
	crypto::shared::MetaCrypter,
	error::Error,
	fs::{
		HasMetaExt, HasName, HasUUID,
		dir::HasUUIDContents,
		enums::FSObject,
		file::{RemoteFile, meta::FileMeta},
	},
	io::FilenMetaExt,
	util::PathIteratorExt,
};

use super::{DirectoryMeta, DirectoryType, HasContents, RemoteDirectory, traits::SetDirMeta};

impl Client {
	pub async fn create_dir(
		&self,
		parent: &dyn HasUUIDContents,
		name: String,
	) -> Result<RemoteDirectory, Error> {
		self.create_dir_with_created(parent, name, chrono::Utc::now())
			.await
	}

	pub async fn create_dir_with_created(
		&self,
		parent: &dyn HasUUIDContents,
		name: String,
		created: DateTime<Utc>,
	) -> Result<RemoteDirectory, Error> {
		let _lock = self.lock_drive().await?;
		let mut dir = RemoteDirectory::new(name, parent.uuid_as_parent(), created)?;

		let response = api::v3::dir::create::post(
			self.client(),
			&api::v3::dir::create::Request {
				uuid: dir.uuid(),
				parent: parent.uuid(),
				name_hashed: Cow::Borrowed(&self.hash_name(dir.name())),
				meta: Cow::Borrowed(&dir.get_encrypted_meta(self.crypter())?),
			},
		)
		.await?;
		if dir.uuid() != response.uuid {
			dir.set_uuid(response.uuid);
		}
		futures::try_join!(
			self.update_search_hashes_for_item(&dir)
				.map_err(Error::from),
			self.update_item_with_maybe_connected_parent(&dir),
		)?;
		Ok(dir)
	}

	pub async fn get_dir(&self, uuid: UuidStr) -> Result<RemoteDirectory, Error> {
		let response = api::v3::dir::post(self.client(), &api::v3::dir::Request { uuid }).await?;

		RemoteDirectory::from_encrypted(
			uuid,
			response.parent,
			response.color.map(|s| s.into_owned()),
			response.favorited,
			&response.metadata,
			self.crypter(),
		)
	}

	pub async fn dir_exists(
		&self,
		parent: &dyn HasUUIDContents,
		name: &str,
	) -> Result<Option<UuidStr>, Error> {
		api::v3::dir::exists::post(
			self.client(),
			&api::v3::dir::exists::Request {
				parent: parent.uuid(),
				name_hashed: Cow::Borrowed(&self.hash_name(name.as_ref())),
			},
		)
		.await
		.map(|r| r.0)
	}

	pub async fn list_dir(
		&self,
		dir: &dyn HasContents,
	) -> Result<(Vec<RemoteDirectory>, Vec<RemoteFile>), Error> {
		let response = api::v3::dir::content::post(
			self.client(),
			&api::v3::dir::content::Request {
				uuid: dir.uuid_as_parent(),
			},
		)
		.await?;

		let dirs = response
			.dirs
			.into_iter()
			.map(|d| {
				RemoteDirectory::from_encrypted(
					d.uuid,
					d.parent,
					d.color.map(|s| s.into_owned()),
					d.favorited,
					&d.meta,
					self.crypter(),
				)
			})
			.collect::<Result<Vec<_>, _>>()?;

		let files = response
			.files
			.into_iter()
			.map(|f| {
				let meta = FileMeta::from_encrypted(&f.metadata, self.crypter(), f.version)?;
				Ok::<RemoteFile, Error>(RemoteFile::from_meta(
					f.uuid,
					f.parent,
					f.size,
					f.chunks,
					f.region,
					f.bucket,
					f.favorited,
					meta,
				))
			})
			.collect::<Result<Vec<_>, _>>()?;
		Ok((dirs, files))
	}

	pub async fn list_dir_recursive(
		&self,
		dir: &dyn HasContents,
	) -> Result<(Vec<RemoteDirectory>, Vec<RemoteFile>), Error> {
		let response = api::v3::dir::download::post(
			self.client(),
			&api::v3::dir::download::Request {
				uuid: dir.uuid_as_parent(),
				skip_cache: false,
			},
		)
		.await?;

		let dirs = response
			.dirs
			.into_iter()
			.filter_map(|response_dir| {
				Some(RemoteDirectory::from_encrypted(
					response_dir.uuid,
					match response_dir.parent {
						// the request returns the base dir for the request as one of its dirs, we filter it out here
						None => return None,
						Some(parent) => parent,
					},
					response_dir.color.map(|s| s.into_owned()),
					response_dir.favorited,
					&response_dir.meta,
					self.crypter(),
				))
			})
			.collect::<Result<Vec<_>, _>>()?;

		let files = response
			.files
			.into_iter()
			.map(|f| {
				let decrypted_size = self.crypter().decrypt_meta(&f.size)?;
				let decrypted_size = decrypted_size
					.parse::<u64>()
					.map_err(|_| Error::Custom("Failed to parse decrypted size".to_string()))?;
				let meta = FileMeta::from_encrypted(&f.metadata, self.crypter(), f.version)?;
				Ok::<RemoteFile, Error>(RemoteFile::from_meta(
					f.uuid,
					f.parent,
					decrypted_size,
					f.chunks,
					f.region,
					f.bucket,
					f.favorited,
					meta,
				))
			})
			.collect::<Result<Vec<_>, _>>()?;
		Ok((dirs, files))
	}

	pub async fn trash_dir(&self, dir: &RemoteDirectory) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::dir::trash::post(
			self.client(),
			&api::v3::dir::trash::Request { uuid: dir.uuid() },
		)
		.await?;
		Ok(())
	}

	pub async fn restore_dir(&self, dir: &RemoteDirectory) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::dir::restore::post(
			self.client(),
			&api::v3::dir::restore::Request { uuid: dir.uuid() },
		)
		.await?;
		Ok(())
	}

	pub async fn delete_dir_permanently(&self, dir: RemoteDirectory) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::dir::delete::permanent::post(
			self.client(),
			&api::v3::dir::delete::permanent::Request { uuid: dir.uuid() },
		)
		.await?;
		Ok(())
	}

	pub async fn update_dir_metadata(
		&self,
		dir: &mut RemoteDirectory,
		new_meta: DirectoryMeta<'_>,
	) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::dir::metadata::post(
			self.client(),
			&api::v3::dir::metadata::Request {
				uuid: dir.uuid(),
				name_hashed: Cow::Borrowed(&self.hash_name(new_meta.name())),
				metadata: Cow::Borrowed(
					&self
						.crypter()
						.encrypt_meta(&serde_json::to_string(&new_meta)?)?,
				),
			},
		)
		.await?;
		dir.set_meta(new_meta);
		self.update_maybe_connected_item(dir).await?;

		Ok(())
	}

	pub async fn find_item_in_dir(
		&self,
		dir: &dyn HasContents,
		name: &str,
	) -> Result<Option<FSObject<'static>>, Error> {
		let (dirs, files) = self.list_dir(dir).await?;
		if let Some(dir) = dirs.into_iter().find(|d| d.name() == name) {
			return Ok(Some(FSObject::Dir(Cow::Owned(dir))));
		}
		if let Some(file) = files.into_iter().find(|f| f.name() == name) {
			return Ok(Some(FSObject::File(Cow::Owned(file))));
		}
		Ok(None)
	}

	pub async fn find_or_create_dir_starting_at<'a>(
		&self,
		dir: DirectoryType<'a>,
		path: &str,
	) -> Result<DirectoryType<'a>, Error> {
		let _lock = self.lock_drive().await?;
		let mut curr_dir = dir;
		for (component, remaining_path) in path.path_iter() {
			let (dirs, files) = self.list_dir(&curr_dir).await?;
			if let Some(dir) = dirs.into_iter().find(|d| d.name() == component) {
				curr_dir = DirectoryType::Dir(Cow::Owned(dir));
				continue;
			}

			if files.iter().any(|f| f.name() == component) {
				return Err(Error::Custom(format!(
					"find_or_create_dir path {remaining_path}/{component} is a file when trying to create dir {path}"
				)));
			}

			let new_dir = self.create_dir(&curr_dir, component.to_string()).await?;
			curr_dir = DirectoryType::Dir(Cow::Owned(new_dir));
		}
		Ok(curr_dir)
	}

	pub async fn find_or_create_dir(&self, path: &str) -> Result<DirectoryType<'_>, Error> {
		self.find_or_create_dir_starting_at(DirectoryType::Root(Cow::Borrowed(self.root())), path)
			.await
	}

	// todo add overwriting
	// I want to add this in tandem with a locking mechanism so that I avoid race conditions
	pub async fn move_dir(
		&self,
		dir: &mut RemoteDirectory,
		new_parent: &dyn HasUUIDContents,
	) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::dir::r#move::post(
			self.client(),
			&api::v3::dir::r#move::Request {
				uuid: dir.uuid(),
				to: new_parent.uuid(),
			},
		)
		.await?;
		dir.set_parent(new_parent.uuid().into());
		Ok(())
	}

	pub async fn get_dir_size(
		&self,
		dir: &dyn HasUUIDContents,
		trash: bool,
	) -> Result<api::v3::dir::size::Response, Error> {
		api::v3::dir::size::post(
			self.client(),
			&api::v3::dir::size::Request {
				uuid: dir.uuid(),
				sharer_id: None,
				receiver_id: None,
				trash,
			},
		)
		.await
	}
}

#[cfg(feature = "tokio")]
impl Client {
	pub async fn recursive_upload_dir(
		&self,
		dir: &Path,
		name: String,
		parent: &dyn HasUUIDContents,
		created: DateTime<Utc>,
	) -> Result<RemoteDirectory, Error> {
		use futures::StreamExt;

		use crate::consts::MAX_SMALL_PARALLEL_REQUESTS;

		let _lock = self.lock_drive().await?;

		let read_dir = tokio::fs::read_dir(dir).await?;
		let remote_dir = self.create_dir_with_created(parent, name, created).await?;
		let stream = tokio_stream::wrappers::ReadDirStream::new(read_dir);

		let stream = stream
			.map(|entry| async {
				let entry = entry?;
				let path = entry.path();
				let meta = entry.metadata().await?;
				if meta.is_dir() {
					let name = entry.file_name().into_string().map_err(|_| {
						Error::Custom("Failed to convert OsString to String".to_string())
					})?;
					Box::pin(self.recursive_upload_dir(
						&path,
						name,
						&remote_dir,
						FilenMetaExt::created(&meta),
					))
					.await?;
				} else if meta.is_file() {
					use tokio_util::compat::TokioAsyncReadCompatExt;

					let name = entry.file_name().into_string().map_err(|_| {
						Error::Custom("Failed to convert OsString to String".to_string())
					})?;
					// stop from overloading client with too many open files
					let _sem = self.open_file_semaphore.acquire().await.unwrap();
					let os_file = tokio::fs::File::open(&path).await?;
					let file = self
						.make_file_builder(name, &remote_dir)
						.modified(FilenMetaExt::modified(&meta))
						.created(FilenMetaExt::created(&meta))
						.build();

					self.upload_file_from_reader(
						file.into(),
						&mut os_file.compat(),
						None,
						Some(meta.size()),
					)
					.await?;
				} else {
					return Err(Error::Custom("Unsupported file type".to_string()));
				}
				Ok::<_, Error>(())
			})
			.buffer_unordered(MAX_SMALL_PARALLEL_REQUESTS);

		{
			tokio::pin!(stream);
			while let Some(result) = stream.next().await {
				use crate::error::ErrorExt;
				result.context("recursive_upload_dir")?; // propagate any errors
			}
		}

		Ok(remote_dir)
	}
}
