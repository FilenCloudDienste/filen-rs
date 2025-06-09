use std::borrow::Cow;

use anyhow::Result;
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasUUID, UnsharedFSObject,
		dir::{RemoteDirectory, UnsharedDirectoryType},
		file::RemoteFile,
	},
};
use filen_types::fs::ObjectType;
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesOrdered};
use rusqlite::Connection;

use crate::{
	FilenMobileDB,
	sql::{self, DBDir, DBDirObject, DBFile, DBObject, DBRoot},
};

#[allow(unused)]
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
			other => other,
		}
	}
}

async fn check_local_item_matches_remote(obj: DBObject, client: &Client) -> LocalRemoteComparison {
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
) -> Result<FuturesOrdered<BoxFuture<'a, (LocalRemoteComparison<'a>, &'b str)>>>
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

fn update_dirs_and_item(
	conn: &mut Connection,
	mut dirs: Vec<UnsharedDirectoryType<'_>>,
	last_item: Option<UnsharedFSObject<'_>>,
) -> Result<DBObject> {
	// SAFETY: We assume that the last item is always present,
	// either from the last_item parameter or from the dirs vector.
	let last_item = last_item.unwrap_or_else(|| dirs.pop().unwrap().into());
	for dir in dirs {
		match dir {
			UnsharedDirectoryType::Root(root) => {
				DBRoot::upsert_from_remote(conn, &root)?;
			}
			UnsharedDirectoryType::Dir(dir) => {
				DBDir::upsert_from_remote(conn, dir.into_owned())?;
			}
		}
	}
	match last_item {
		UnsharedFSObject::File(file) => {
			Ok(DBFile::upsert_from_remote(conn, file.into_owned())?.into())
		}
		UnsharedFSObject::Dir(dir) => Ok(DBDir::upsert_from_remote(conn, dir.into_owned())?.into()),
		UnsharedFSObject::Root(root) => Ok(DBRoot::upsert_from_remote(conn, &root)?.into()),
	}
}

async fn update_items_in_path_starting_at<'a>(
	db: &FilenMobileDB,
	client: &Client,
	path: &'a str,
	parent: UnsharedDirectoryType<'_>,
) -> Result<UpdateItemsInPath<'a>> {
	match client.get_items_in_path_starting_at(path, parent).await {
		Ok((dirs, last_item)) => {
			let obj = update_dirs_and_item(&mut db.conn(), dirs, last_item)?;
			Ok(UpdateItemsInPath::Complete(obj))
		}
		Err((
			filen_sdk_rs::error::Error::InvalidType(ObjectType::File, ObjectType::Dir),
			(dirs, last_item),
			path_remaining,
		)) => {
			// We got a file when we expected a directory, so we update the directories and the file
			let dir_obj = match update_dirs_and_item(&mut db.conn(), dirs, last_item)? {
				DBObject::File(_) => {
					return Err(filen_sdk_rs::error::Error::InvalidType(
						ObjectType::File,
						ObjectType::Dir,
					)
					.into());
				}
				DBObject::Dir(dir) => dir.into(),
				DBObject::Root(root) => root.into(),
			};
			// but return false to indicate that the path is invalid
			Ok(UpdateItemsInPath::Partial(path_remaining, dir_obj))
		}
		Err((e, (dirs, last_item), _)) => {
			update_dirs_and_item(&mut db.conn(), dirs, last_item)?;
			Err(e.into())
		}
	}
}

#[allow(unused)]
pub(crate) enum UpdateItemsInPath<'a> {
	Complete(DBObject),
	Partial(&'a str, DBDirObject),
}

pub(crate) async fn update_items_in_path<'a>(
	db: &FilenMobileDB,
	client: &Client,
	path: &'a str,
) -> Result<UpdateItemsInPath<'a>> {
	let (objects, all) = sql::select_objects_in_path(&db.conn(), client.root().uuid(), path)?;
	let mut futures = get_required_update_futures(objects, all, client)?;

	let mut last_valid_obj: Option<DBObject> = None;
	let mut final_remaining_path: &str = path;

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
				let db_dir = DBDir::upsert_from_remote(&mut db.conn(), remote_directory)?;
				last_valid_obj = Some(DBObject::Dir(db_dir));
				break_early = true;
				break;
			}
			LocalRemoteComparison::SameFile(db_file, _) => {
				last_valid_obj = Some(DBObject::File(db_file));
			}
			LocalRemoteComparison::DifferentFile(_, remote_file) => {
				last_valid_obj = Some(DBObject::File(DBFile::upsert_from_remote(
					&mut db.conn(),
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
				break_early = true;
				break;
			}
			LocalRemoteComparison::NotFound(_) => {
				break_early = true;
				break;
			}
		}
	}
	// SAFETY: We always have at least the root object
	let last_valid_obj =
		last_valid_obj.ok_or_else(|| anyhow::anyhow!("No valid items found in path: {}", path))?;
	if !break_early {
		// local cache matches remote, no need to update
		return Ok(UpdateItemsInPath::Complete(last_valid_obj));
	}
	let last_dir = match last_valid_obj {
		DBObject::Dir(dir) => UnsharedDirectoryType::Dir(Cow::Owned(dir.into())),
		DBObject::Root(root) => UnsharedDirectoryType::Root(Cow::Owned(root.into())),
		DBObject::File(_) => {
			return Err(
				filen_sdk_rs::error::Error::InvalidType(ObjectType::File, ObjectType::Dir).into(),
			);
		}
	};
	let resp = update_items_in_path_starting_at(db, client, final_remaining_path, last_dir).await?;
	Ok(resp)
}

// pub(crate) enum ObjectOrParent {
// 	Object(DBObject),
// 	Parent(DBDirObject),
// }

// pub(crate) async fn update_or_create_items_in_path(
// 	db: &FilenMobileDB,
// 	client: &Client,
// 	path: &str,
// ) -> Result<ObjectOrParent> {
// 	let (remaining_path, mut last_dir) = match update_items_in_path(db, client, path).await? {
// 		UpdateItemsInPath::Complete(db_obj) => return Ok(ObjectOrParent::Object(db_obj)),
// 		UpdateItemsInPath::Partial(path, dbdir_trait) => (path, dbdir_trait),
// 	};
// 	for component in remaining_path.split('/') {
// 		if component.is_empty() {
// 			continue; // Skip empty components
// 		}
// 		let new_dir = client
// 			.create_dir(&last_dir.uuid(), component.to_string())
// 			.await?;
// 		last_dir = DBDir::upsert_from_remote(&mut db.conn(), new_dir)?.into();
// 	}
// 	Ok(ObjectOrParent::Parent(last_dir))
// }
