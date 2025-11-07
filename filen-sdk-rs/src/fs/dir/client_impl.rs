use std::borrow::Cow;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::path::Path;

use chrono::{DateTime, Utc};
use filen_types::api::v3::dir::color::DirColor;
use filen_types::fs::{ObjectType, ParentUuid, UuidStr};
use futures::TryFutureExt;
#[cfg(feature = "multi-threaded-crypto")]
use rayon::iter::ParallelIterator;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use crate::ErrorKind;
use crate::{
	api,
	auth::Client,
	connect::fs::{ShareInfo, SharingRole},
	crypto::shared::MetaCrypter,
	error::{Error, ErrorExt, InvalidTypeError, MetadataWasNotDecryptedError},
	fs::{
		HasName, HasParent, HasUUID, NonRootFSObject,
		dir::{
			DirectoryTypeWithShareInfo, HasUUIDContents,
			meta::{DirectoryMeta, DirectoryMetaChanges},
			traits::HasDirMeta,
		},
		file::{RemoteFile, meta::FileMeta},
	},
	runtime::{blocking_join, do_cpu_intensive},
	util::{IntoMaybeParallelIterator, PathIteratorExt},
};

use super::{DirectoryType, HasContents, RemoteDirectory, traits::UpdateDirMeta};

enum ObjectMatch<T> {
	Name(T),
	Uuid(T),
}

impl Client {
	// todo, do not allow using shared dirs here
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
		let (mut uuid, meta) = RemoteDirectory::make_parts(name, created)?;

		let response = api::v3::dir::create::post(
			self.client(),
			&api::v3::dir::create::Request {
				uuid,
				parent: *parent.uuid(),
				name_hashed: Cow::Borrowed(&self.hash_name(&meta.name)),
				meta: self.crypter().encrypt_meta(&meta.to_json_string()).await,
			},
		)
		.await?;

		if uuid != response.uuid {
			uuid = response.uuid;
		}

		let dir: RemoteDirectory = RemoteDirectory::new_from_parts(
			uuid,
			meta,
			parent.uuid_as_parent(),
			response.timestamp,
		);

		futures::try_join!(
			self.update_search_hashes_for_item(&dir)
				.map_err(Error::from),
			self.update_item_with_maybe_connected_parent(&dir),
		)?;
		Ok(dir)
	}

	#[cfg(feature = "malformed")]
	pub async fn create_malformed_dir(
		&self,
		parent: &dyn HasUUIDContents,
		name: &str,
		contents: &str,
	) -> Result<UuidStr, Error> {
		use filen_types::crypto::EncryptedString;
		let _lock = self.lock_drive().await?;

		let uuid = UuidStr::new_v4();
		let response = api::v3::dir::create::post(
			self.client(),
			&api::v3::dir::create::Request {
				uuid,
				parent: *parent.uuid(),
				name_hashed: Cow::Borrowed(&self.hash_name(name)),
				meta: EncryptedString(Cow::Borrowed(contents)),
			},
		)
		.await?;
		Ok(response.uuid)
	}

	pub async fn get_dir(&self, uuid: UuidStr) -> Result<RemoteDirectory, Error> {
		let response = api::v3::dir::post(self.client(), &api::v3::dir::Request { uuid }).await?;

		Ok(do_cpu_intensive(|| {
			RemoteDirectory::blocking_from_encrypted(
				uuid,
				// v3 api returns the original parent as the parent if the file is in the trash
				if response.trash {
					ParentUuid::Trash
				} else {
					response.parent
				},
				response.color,
				response.favorited,
				response.timestamp,
				response.metadata,
				&*self.crypter(),
			)
		})
		.await)
	}

	pub async fn dir_exists(
		&self,
		parent: &dyn HasUUIDContents,
		name: &str,
	) -> Result<Option<UuidStr>, Error> {
		api::v3::dir::exists::post(
			self.client(),
			&api::v3::dir::exists::Request {
				parent: *parent.uuid(),
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

		let crypter = self.crypter();

		do_cpu_intensive(|| {
			let (dirs, files) = blocking_join!(
				|| response
					.dirs
					.into_maybe_par_iter()
					.map(|d| {
						RemoteDirectory::blocking_from_encrypted(
							d.uuid,
							d.parent,
							d.color,
							d.favorited,
							d.timestamp,
							d.meta,
							&*crypter,
						)
					})
					.collect::<Vec<_>>(),
				|| response
					.files
					.into_maybe_par_iter()
					.map(|f| {
						let meta =
							FileMeta::blocking_from_encrypted(f.metadata, &*crypter, f.version);
						Ok::<RemoteFile, Error>(RemoteFile::from_meta(
							f.uuid,
							f.parent,
							f.size,
							f.chunks,
							f.region,
							f.bucket,
							f.timestamp,
							f.favorited,
							meta,
						))
					})
					.collect::<Result<Vec<_>, _>>()
			);

			Ok((dirs, files?))
		})
		.await
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

		let crypter = self.crypter();

		do_cpu_intensive(|| {
			let (dirs, files) = blocking_join!(
				|| response
					.dirs
					.into_maybe_par_iter()
					.filter_map(|response_dir| {
						Some(RemoteDirectory::blocking_from_encrypted(
							response_dir.uuid,
							match response_dir.parent {
								// the request returns the base dir for the request as one of its dirs, we filter it out here
								None => return None,
								Some(parent) => parent,
							},
							response_dir.color,
							response_dir.favorited,
							response_dir.timestamp,
							response_dir.meta,
							&*crypter,
						))
					})
					.collect::<Vec<_>>(),
				|| response
					.files
					.into_maybe_par_iter()
					.map(|f| {
						let meta =
							FileMeta::blocking_from_encrypted(f.metadata, &*crypter, f.version);
						Ok::<RemoteFile, Error>(RemoteFile::from_meta(
							f.uuid,
							f.parent,
							f.chunks_size,
							f.chunks,
							f.region,
							f.bucket,
							f.timestamp,
							f.favorited,
							meta,
						))
					})
					.collect::<Result<Vec<_>, _>>()
			);
			Ok((dirs, files?))
		})
		.await
	}

	pub async fn trash_dir(&self, dir: &mut RemoteDirectory) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::dir::trash::post(
			self.client(),
			&api::v3::dir::trash::Request { uuid: *dir.uuid() },
		)
		.await?;
		dir.parent = ParentUuid::Trash;
		Ok(())
	}

	pub async fn restore_dir(&self, dir: &mut RemoteDirectory) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::dir::restore::post(
			self.client(),
			&api::v3::dir::restore::Request { uuid: *dir.uuid() },
		)
		.await?;

		// api v3 doesn't return the parentUUID we returned to, so we query it separately for now
		let resp =
			api::v3::dir::post(self.client(), &api::v3::dir::Request { uuid: *dir.uuid() }).await?;
		dir.parent = resp.parent;
		Ok(())
	}

	pub async fn delete_dir_permanently(&self, dir: RemoteDirectory) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::dir::delete::permanent::post(
			self.client(),
			&api::v3::dir::delete::permanent::Request { uuid: *dir.uuid() },
		)
		.await?;
		Ok(())
	}

	pub async fn update_dir_metadata(
		&self,
		dir: &mut RemoteDirectory,
		changes: DirectoryMetaChanges,
	) -> Result<(), Error> {
		let new_borrowed_meta = dir.get_meta();
		let temp_meta = new_borrowed_meta.borrow_with_changes(&changes)?;
		let DirectoryMeta::Decoded(temp_meta) = temp_meta else {
			return Err(MetadataWasNotDecryptedError.into());
		};

		let (_lock, encrypted_meta) = futures::join!(
			self.lock_drive(),
			do_cpu_intensive(|| {
				Ok::<_, Error>(
					self.crypter()
						.blocking_encrypt_meta(&serde_json::to_string(&temp_meta)?),
				)
			})
		);

		api::v3::dir::metadata::post(
			self.client(),
			&api::v3::dir::metadata::Request {
				uuid: *dir.uuid(),
				name_hashed: Cow::Borrowed(&self.hash_name(temp_meta.name())),
				metadata: encrypted_meta?,
			},
		)
		.await?;

		dir.update_meta(changes)?;
		self.update_maybe_connected_item(dir).await?;

		Ok(())
	}

	fn inner_find_item_in_dirs(
		&self,
		dirs: Vec<RemoteDirectory>,
		name_or_uuid: &str,
	) -> Option<ObjectMatch<RemoteDirectory>> {
		let mut uuid_match = None;

		for dir in dirs {
			if dir.name().is_some_and(|n| n == name_or_uuid) {
				return Some(ObjectMatch::Name(dir));
			} else if dir.uuid().as_ref() == name_or_uuid {
				uuid_match = Some(ObjectMatch::Uuid(dir));
			}
		}
		uuid_match
	}

	fn inner_find_item_in_files(
		&self,
		files: Vec<RemoteFile>,
		name_or_uuid: &str,
	) -> Option<ObjectMatch<RemoteFile>> {
		let mut uuid_match = None;

		for file in files {
			if file.name().is_some_and(|n| n == name_or_uuid) {
				return Some(ObjectMatch::Name(file));
			} else if file.uuid().as_ref() == name_or_uuid {
				uuid_match = Some(ObjectMatch::Uuid(file));
			}
		}
		uuid_match
	}

	fn inner_find_item_in_dirs_and_files(
		&self,
		dirs: Vec<RemoteDirectory>,
		files: Vec<RemoteFile>,
		name_or_uuid: &str,
	) -> Option<NonRootFSObject<'static>> {
		let uuid_match = match self.inner_find_item_in_dirs(dirs, name_or_uuid) {
			Some(ObjectMatch::Name(dir)) => return Some(NonRootFSObject::Dir(Cow::Owned(dir))),
			Some(ObjectMatch::Uuid(dir)) => Some(dir),
			None => None,
		};
		match self.inner_find_item_in_files(files, name_or_uuid) {
			Some(ObjectMatch::Name(file)) => Some(NonRootFSObject::File(Cow::Owned(file))),
			Some(ObjectMatch::Uuid(file)) => Some(NonRootFSObject::File(Cow::Owned(file))),
			None => uuid_match.map(|dir| NonRootFSObject::Dir(Cow::Owned(dir))),
		}
	}

	/// Finds an item in the directory by name or UUID.
	/// Returns the match by name if one exists, otherwise returns the match by UUID.
	/// If no match is found, returns None.
	pub async fn find_item_in_dir(
		&self,
		// TODO, disallow shared dirs here
		dir: &dyn HasContents,
		name_or_uuid: &str,
	) -> Result<Option<NonRootFSObject<'static>>, Error> {
		let (dirs, files) = self.list_dir(dir).await?;
		Ok(self.inner_find_item_in_dirs_and_files(dirs, files, name_or_uuid))
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

			let dir_uuid_match = match self.inner_find_item_in_dirs(dirs, component) {
				Some(ObjectMatch::Name(dir)) => {
					curr_dir = DirectoryType::Dir(Cow::Owned(dir));
					continue;
				}
				Some(ObjectMatch::Uuid(obj)) => Some(obj),
				None => None,
			};

			match self.inner_find_item_in_files(files, component) {
				Some(ObjectMatch::Name(_)) | Some(ObjectMatch::Uuid(_)) => {
					return Err(InvalidTypeError {
						actual: ObjectType::File,
						expected: ObjectType::Dir,
					}.with_context(format!(
						"find_or_create_dir path {remaining_path}/{component} is a file when trying to create dir {path}"
					)));
				}
				None => {}
			};

			if let Some(dir) = dir_uuid_match {
				curr_dir = DirectoryType::Dir(Cow::Owned(dir));
				continue;
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
				uuid: *dir.uuid(),
				to: *new_parent.uuid(),
			},
		)
		.await?;
		dir.set_parent((*new_parent.uuid()).into());
		Ok(())
	}

	pub async fn get_dir_size<'a, T>(
		&self,
		dir: &'a T,
	) -> Result<api::v3::dir::size::Response, Error>
	where
		&'a T: Into<DirectoryTypeWithShareInfo<'a>>,
	{
		let request = match dir.into() {
			DirectoryTypeWithShareInfo::Root(r) => api::v3::dir::size::Request {
				uuid: *r.uuid(),
				sharer_id: None,
				receiver_id: None,
				trash: false,
			},
			DirectoryTypeWithShareInfo::Dir(d) => api::v3::dir::size::Request {
				uuid: *d.uuid(),
				sharer_id: None,
				receiver_id: None,
				trash: *d.parent() == ParentUuid::Trash,
			},
			DirectoryTypeWithShareInfo::SharedDir(d) => api::v3::dir::size::Request {
				uuid: *d.dir.uuid(),
				sharer_id: if let SharingRole::Sharer(ShareInfo { id, .. }) = d.sharing_role {
					Some(id)
				} else {
					None
				},
				receiver_id: if let SharingRole::Receiver(ShareInfo { id, .. }) = d.sharing_role {
					Some(id)
				} else {
					None
				},
				trash: false,
			},
		};
		api::v3::dir::size::post(self.client(), &request).await
	}

	pub async fn set_dir_color(
		&self,
		dir: &mut RemoteDirectory,
		color: DirColor<'_>,
	) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::dir::color::post(
			self.client(),
			&api::v3::dir::color::Request {
				uuid: *dir.uuid(),
				color: color.borrow_clone(),
			},
		)
		.await?;
		dir.color = color.into_owned();
		Ok(())
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
impl Client {
	pub async fn recursive_upload_dir(
		&self,
		dir: &Path,
		name: String,
		parent: &dyn HasUUIDContents,
		created: DateTime<Utc>,
	) -> Result<RemoteDirectory, Error> {
		use futures::StreamExt;

		use crate::{consts::MAX_SMALL_PARALLEL_REQUESTS, io::FilenMetaExt};

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
						Error::custom(
							ErrorKind::Conversion,
							"Failed to convert OsString to String",
						)
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
						Error::custom(
							ErrorKind::Conversion,
							"Failed to convert OsString to String",
						)
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
					return Err(Error::custom(
						ErrorKind::IO,
						format!("Unsupported file type for path: {}", path.display()),
					));
				}
				Ok::<_, Error>(())
			})
			.buffer_unordered(MAX_SMALL_PARALLEL_REQUESTS);

		{
			tokio::pin!(stream);
			while let Some(result) = stream.next().await {
				use crate::error::ResultExt;
				result.context("recursive_upload_dir")?; // propagate any errors
			}
		}

		Ok(remote_dir)
	}
}
