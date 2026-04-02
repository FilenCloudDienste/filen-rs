use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::api::v3::dir::color::DirColor;
use filen_types::fs::{ObjectType, ParentUuid, UuidStr};
use filen_types::traits::CowHelpers;
use futures::TryFutureExt;

use crate::{
	api,
	auth::Client,
	crypto::shared::MetaCrypter,
	error::{Error, ErrorExt, InvalidTypeError, MetadataWasNotDecryptedError},
	fs::{
		HasUUID,
		categories::{
			DirType, Normal,
			fs::{CategoryFS, ObjectMatch, find_item_in_dirs, find_item_in_files},
		},
		dir::{
			meta::{DirectoryMeta, DirectoryMetaChanges},
			traits::HasDirMeta,
		},
		file::RemoteFile,
	},
	runtime::do_cpu_intensive,
	util::PathIteratorExt,
};

use super::{RemoteDirectory, traits::UpdateDirMeta};

impl Client {
	pub async fn create_dir(
		&self,
		parent: &DirType<'_, Normal>,
		name: &str,
	) -> Result<RemoteDirectory, Error> {
		self.create_dir_with_created(parent, name, chrono::Utc::now())
			.await
	}

	pub(crate) async fn inner_create_dir_with_created(
		&self,
		parent: UuidStr,
		name: &str,
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
			(*parent.uuid()).into(),
			response.timestamp,
		);

		futures::try_join!(
			self.update_search_hashes_for_item(&dir)
				.map_err(Error::from),
			self.update_item_with_maybe_connected_parent((&dir).into()),
		)?;
		Ok(dir)
	}

	pub async fn create_dir_with_created(
		&self,
		parent: &DirType<'_, Normal>,
		name: &str,
		created: DateTime<Utc>,
	) -> Result<RemoteDirectory, Error> {
		self.inner_create_dir_with_created(*parent.uuid(), name, created)
			.await
	}

	#[cfg(feature = "malformed")]
	pub async fn create_malformed_dir(
		&self,
		parent: &DirType<'_, Normal>,
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
		parent: &DirType<'_, Normal>,
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

	#[allow(private_bounds)]
	pub async fn list_dir<'a, F, Cat>(
		&self,
		parent: &DirType<'_, Cat>,
		progress: Option<&F>,
	) -> Result<(Vec<Cat::Dir>, Vec<Cat::File>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
		Cat: CategoryFS<Client = Self>,
		Cat::ListDirContext<'a>: Default,
	{
		Cat::list_dir(self, parent, progress, Cat::ListDirContext::default()).await
	}

	pub async fn list_linked<F>(
		&self,
		progress: Option<&F>,
	) -> Result<(Vec<RemoteDirectory>, Vec<RemoteFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		// todo check if returned parent is ParentUuid::Linked or not
		crate::fs::categories::normal::list_parent_uuid(self, ParentUuid::Links, progress).await
	}

	pub async fn list_favorites<F>(
		&self,
		progress: Option<&F>,
	) -> Result<(Vec<RemoteDirectory>, Vec<RemoteFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		// todo check if returned parent is ParentUuid::Favorites or not
		crate::fs::categories::normal::list_parent_uuid(self, ParentUuid::Favorites, progress).await
	}

	pub async fn list_recents<F>(
		&self,
		progress: Option<&F>,
	) -> Result<(Vec<RemoteDirectory>, Vec<RemoteFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		// todo check if returned parent is ParentUuid::Recents or not
		crate::fs::categories::normal::list_parent_uuid(self, ParentUuid::Recents, progress).await
	}

	pub async fn list_trash<F>(
		&self,
		progress: Option<&F>,
	) -> Result<(Vec<RemoteDirectory>, Vec<RemoteFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		// todo check if returned parent is ParentUuid::Trash or not
		crate::fs::categories::normal::list_parent_uuid(self, ParentUuid::Trash, progress).await
	}

	/// Recursively lists all directories and files inside the given directory.
	///
	/// This might take a long time and use a lot of memory (>1GiB) for large directories.
	/// Since the entire directory structure needs to be held in memory while decrypting,
	/// this function might fail with an Out Of Memory error on platforms with limited memory,
	/// such as WASM.
	///
	/// The progress callback receives the number of bytes downloaded so far and the total number of bytes to download, if known.
	#[allow(private_bounds)]
	pub async fn list_dir_recursive<Cat, F>(
		&self,
		dir: &DirType<'_, Cat>,
		progress_callback: Option<&F>,
		context: Cat::ListDirContext<'_>,
	) -> Result<(Vec<Cat::Dir>, Vec<Cat::File>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
		Cat: CategoryFS<Client = Self>,
	{
		Cat::list_dir_recursive(self, dir, progress_callback, context).await
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

	// /// Finds an item in the directory by name or UUID.
	// /// Returns the match by name if one exists, otherwise returns the match by UUID.
	// /// If no match is found, returns None.

	pub async fn find_or_create_dir_starting_at<'a>(
		&self,
		dir: DirType<'a, Normal>,
		path: &str,
	) -> Result<DirType<'a, Normal>, Error> {
		let _lock = self.lock_drive().await?;
		let mut curr_dir = dir;
		for (component, remaining_path) in path.path_iter() {
			let (dirs, files) = self
				.list_dir::<_, Normal>(&curr_dir, None::<&fn(u64, Option<u64>)>)
				.await?;

			let dir_uuid_match = match find_item_in_dirs::<Normal>(dirs, component) {
				Some(ObjectMatch::Name(dir)) => {
					curr_dir = DirType::Dir(Cow::Owned(dir));
					continue;
				}
				Some(ObjectMatch::Uuid(obj)) => Some(obj),
				None => None,
			};

			match find_item_in_files::<Normal>(files, component) {
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
				curr_dir = DirType::Dir(Cow::Owned(dir));
				continue;
			}

			let new_dir = self.create_dir(&curr_dir, component).await?;
			curr_dir = DirType::Dir(Cow::Owned(new_dir));
		}
		Ok(curr_dir)
	}

	pub async fn find_or_create_dir(&self, path: &str) -> Result<DirType<'_, Normal>, Error> {
		self.find_or_create_dir_starting_at(DirType::Root(Cow::Borrowed(self.root())), path)
			.await
	}

	// todo add overwriting
	// I want to add this in tandem with a locking mechanism so that I avoid race conditions
	pub async fn move_dir(
		&self,
		dir: &mut RemoteDirectory,
		new_parent: &DirType<'_, Normal>,
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
				color: color.as_borrowed_cow(),
			},
		)
		.await?;
		dir.color = color.into_owned_cow();
		Ok(())
	}
}

// pub(crate) trait CategoryDirExt<Cat: CategoryFS> {
// 	async fn find_item_in_dir<F>(
// 		client: &Cat::Client,
// 		dir: &DirType<'_, Cat>,
// 		progress_callback: Option<&F>,
// 		name_or_uuid: &str,
// 		list_dir_context: Cat::ListDirContext<'_>,
// 	) -> Result<Option<NonRootItemType<'static, Cat>>, Error>
// 	where
// 		F: Fn(u64, Option<u64>) + Send + Sync {

// 		}
// }

// impl<Cat: CategoryFS> CategoryDirExt<Cat> for Cat {
// 	async
// }

// #[allow(private_bounds)]
// pub async fn find_item_in_dir<F, CatExt>(
// 	&self,
// 	dir: &DirType<'_, CatExt>,
// 	progress_callback: Option<&F>,
// 	name_or_uuid: &str,
// 	list_dir_context: CatExt::ListDirContext<'_>,
// ) -> Result<Option<NonRootItemType<'static, CatExt>>, Error>
// where
// 	CatExt: CategoryFSExt<Client = Self>,
// 	F: Fn(u64, Option<u64>) + Send + Sync,
// {

// }
