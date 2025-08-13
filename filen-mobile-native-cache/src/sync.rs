use std::borrow::Cow;

use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	fs::{
		UnsharedFSObject,
		client_impl::ObjectOrRemainingPath,
		dir::{RemoteDirectory, RootDirectory, UnsharedDirectoryType},
		file::RemoteFile,
	},
};
use filen_types::fs::ObjectType;
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesOrdered};
use log::debug;
use rusqlite::Connection;

use crate::{
	CacheError,
	auth::AuthCacheState,
	ffi::PathFfiId,
	sql::{self, DBDirObject, DBItemTrait, DBRoot, dir::DBDir, file::DBFile, object::DBObject},
};

#[allow(unused)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalRemoteComparison<'a> {
	SameDir(DBDir, RemoteDirectory),
	DifferentDir(DBDir, RemoteDirectory),
	SameFile(DBFile, RemoteFile),
	DifferentFile(DBFile, RemoteFile),
	Force(DBObject, UnsharedFSObject<'a>),
	NotFound(DBObject),
	Root(DBRoot),
}

impl LocalRemoteComparison<'_> {
	fn force(self) -> Self {
		match self {
			LocalRemoteComparison::SameDir(dir, remote_dir) => {
				LocalRemoteComparison::Force(DBObject::Dir(dir), remote_dir.into())
			}
			LocalRemoteComparison::SameFile(file, remote_file) => {
				LocalRemoteComparison::Force(DBObject::File(file), remote_file.into())
			}
			LocalRemoteComparison::Root(root) => {
				let remote_root = Cow::Owned(RootDirectory::new(root.uuid()));
				LocalRemoteComparison::Force(
					DBObject::Root(root),
					UnsharedFSObject::Root(remote_root),
				)
			}
			other => other,
		}
	}
}

async fn check_local_item_matches_remote(
	obj: DBObject,
	client: &Client,
) -> LocalRemoteComparison<'_> {
	match obj {
		DBObject::Dir(dir) => match client.get_dir(dir.uuid).await {
			Ok(remote_dir) => {
				if dir == remote_dir {
					LocalRemoteComparison::SameDir(dir, remote_dir)
				} else {
					LocalRemoteComparison::DifferentDir(dir, remote_dir)
				}
			}
			Err(_) => LocalRemoteComparison::NotFound(DBObject::Dir(dir)),
		},
		DBObject::File(file) => match client.get_file(file.uuid).await {
			Ok(remote_file) => {
				if file == remote_file {
					LocalRemoteComparison::SameFile(file, remote_file)
				} else {
					LocalRemoteComparison::DifferentFile(file, remote_file)
				}
			}
			Err(_) => LocalRemoteComparison::NotFound(DBObject::File(file)),
		},
		DBObject::Root(root) => LocalRemoteComparison::Root(root),
	}
}

/// Get futures for checking if items in a path need to be updated.
///
/// This function retrieves items in a specified path and creates futures to check if the local items match the remote items.
/// The returned futures will resolve to `Some((item, remote_item))` if the local item does not match the remote item,
/// or `None` if the local item matches the remote item or if the remote item does not exist.
fn get_required_update_futures<'a, 'b>(
	objects: Vec<(DBObject, &'b str)>,
	all: bool,
	client: &'a Client,
) -> Result<FuturesOrdered<BoxFuture<'a, (LocalRemoteComparison<'a>, &'b str)>>, CacheError>
where
	'b: 'a,
{
	let mut item_iter = objects.into_iter().peekable();
	let mut futures = FuturesOrdered::new();
	while let Some((obj, rest_of_path)) = item_iter.next() {
		if !all && item_iter.peek().is_none() {
			// If we did not get all items and this is the last item, we force the check
			futures.push_back(
				async move {
					(
						check_local_item_matches_remote(obj, client).await.force(),
						rest_of_path,
					)
				}
				.boxed(),
			);
			continue;
		}
		futures.push_back(
			async move {
				(
					check_local_item_matches_remote(obj, client).await,
					rest_of_path,
				)
			}
			.boxed(),
		);
	}
	Ok(futures)
}

fn update_dirs(
	conn: &mut Connection,
	dirs: Vec<UnsharedDirectoryType<'_>>,
) -> Result<DBDirObject, CacheError> {
	let mut last_dir_obj = None;
	for dir in dirs {
		match dir {
			UnsharedDirectoryType::Root(root) => {
				last_dir_obj = Some(DBRoot::upsert_from_remote(conn, &root)?.into());
			}
			UnsharedDirectoryType::Dir(dir) => {
				last_dir_obj = Some(DBDir::upsert_from_remote(conn, dir.into_owned())?.into());
			}
		}
	}
	last_dir_obj.ok_or_else(|| CacheError::remote("No directories found in the provided list"))
}

impl AuthCacheState {
	async fn update_items_in_path_starting_at<'a>(
		&self,
		path: &'a str,
		parent: UnsharedDirectoryType<'_>,
	) -> Result<UpdateItemsInPath<'a>, CacheError> {
		match self
			.client
			.get_items_in_path_starting_at(path, parent)
			.await
		{
			Ok((dirs, ObjectOrRemainingPath::Object(last_item))) => {
				let conn = &mut self.conn();
				if !dirs.is_empty() {
					update_dirs(conn, dirs)?;
				}
				let obj = DBObject::upsert_from_remote(conn, last_item)?;
				Ok(UpdateItemsInPath::Complete(obj))
			}
			Ok((dirs, ObjectOrRemainingPath::RemainingPath(path))) => {
				let obj = update_dirs(&mut self.conn(), dirs)?;
				Ok(UpdateItemsInPath::Partial(path, obj))
			}
			Err((error, (dirs, last_item), path_remaining))
				if error.kind() == ErrorKind::InvalidType =>
			{
				let conn = &mut self.conn();
				if !dirs.is_empty() {
					update_dirs(conn, dirs)?;
				}
				// We got a file when we expected a directory, so we update the directories and the file
				let dir_obj = match DBObject::upsert_from_remote(conn, last_item)? {
					DBObject::File(_) => {
						return Err(filen_sdk_rs::Error::from(
							filen_sdk_rs::error::InvalidTypeError {
								expected: ObjectType::File,
								actual: ObjectType::Dir,
							},
						)
						.into());
					}
					DBObject::Dir(dir) => dir.into(),
					DBObject::Root(root) => root.into(),
				};
				// but return partial to indicate that the path is invalid
				Ok(UpdateItemsInPath::Partial(path_remaining, dir_obj))
			}
			Err((e, (dirs, last_item), _)) => {
				let conn = &mut self.conn();
				if !dirs.is_empty() {
					update_dirs(conn, dirs)?;
				}
				DBObject::upsert_from_remote(conn, last_item)?;
				Err(e.into())
			}
		}
	}

	pub(crate) async fn update_items_in_path<'a>(
		&self,
		path_values: &'a PathFfiId<'a>,
	) -> Result<UpdateItemsInPath<'a>, CacheError> {
		debug!(
			"Updating items in path: {}, root: {}, name or uuid: {}",
			path_values.full_path, path_values.root_uuid, path_values.name_or_uuid
		);
		let (objects, all) = sql::select_objects_in_path(&self.conn(), path_values)?;
		let mut futures = get_required_update_futures(objects, all, &self.client)?;

		let mut last_valid_obj: Option<DBObject> = None;
		let mut final_remaining_path: &str = path_values.inner_path;

		let mut break_early = false;

		while let Some((item, remaining_path)) = futures.next().await {
			if !matches!(item, LocalRemoteComparison::NotFound(_)) {
				final_remaining_path = remaining_path;
			}

			match item {
				LocalRemoteComparison::SameDir(db_dir, _) => {
					last_valid_obj = Some(DBObject::Dir(db_dir));
				}
				LocalRemoteComparison::DifferentDir(_, remote_directory) => {
					let db_dir = DBDir::upsert_from_remote(&mut self.conn(), remote_directory)?;
					last_valid_obj = Some(DBObject::Dir(db_dir));
					break_early = true;
					break;
				}
				LocalRemoteComparison::SameFile(db_file, _) => {
					last_valid_obj = Some(DBObject::File(db_file));
				}
				LocalRemoteComparison::DifferentFile(_, remote_file) => {
					last_valid_obj = Some(DBObject::File(DBFile::upsert_from_remote(
						&mut self.conn(),
						remote_file,
					)?));
				}
				LocalRemoteComparison::Force(dbobject, _) => {
					last_valid_obj = Some(dbobject);
					break_early = true;
					break;
				}
				LocalRemoteComparison::Root(root) => {
					last_valid_obj = Some(DBObject::Root(root));
				}
				LocalRemoteComparison::NotFound(_) => {
					break_early = true;
					break;
				}
			}
		}
		// SAFETY: We always have at least the root object
		let last_valid_obj = last_valid_obj.ok_or_else(|| {
			CacheError::remote(format!(
				"No valid items found in path: {}",
				path_values.full_path
			))
		})?;
		if !break_early {
			// local cache matches remote, no need to update
			return Ok(UpdateItemsInPath::Complete(last_valid_obj));
		}
		let last_dir = match last_valid_obj {
			DBObject::Dir(dir) => UnsharedDirectoryType::Dir(Cow::Owned(dir.into())),
			DBObject::Root(root) => UnsharedDirectoryType::Root(Cow::Owned(root.into())),
			DBObject::File(_) => {
				return Err(
					filen_sdk_rs::Error::from(filen_sdk_rs::error::InvalidTypeError {
						actual: ObjectType::File,
						expected: ObjectType::Dir,
					})
					.into(),
				);
			}
		};
		let resp = self
			.update_items_in_path_starting_at(final_remaining_path, last_dir)
			.await?;
		Ok(resp)
	}
}

pub(crate) enum UpdateItemsInPath<'a> {
	Complete(DBObject),
	Partial(&'a str, DBDirObject),
}
