use std::{borrow::Cow, collections::HashMap};

use crate::{
	Error,
	fs::{
		HasParent, HasUUID, NonRootFSObject,
		dir::{DirectoryType, RemoteDirectory},
		file::RemoteFile,
	},
	io::fs_tree::{WalkError, entry::remote::RemoteFSObjectEntry},
};

use filen_types::fs::ParentUuid;
use uuid::Uuid;

use super::{FSStats, FSTree};

pub(crate) struct WalkDirFromHashMap {
	map: HashMap<Uuid, Vec<NonRootFSObject<'static>>>,
	stack: Vec<Uuid>,
}

impl WalkDirFromHashMap {
	pub fn new(
		root_uuid: Uuid,
		dirs: Vec<RemoteDirectory>,
		files: Vec<RemoteFile>,
	) -> Result<Self, Error> {
		let mut map: HashMap<Uuid, Vec<NonRootFSObject<'static>>> = HashMap::new();
		for dir in dirs {
			let ParentUuid::Uuid(parent_uuid) = dir.parent() else {
				return Err(Error::custom(
					crate::ErrorKind::Internal,
					format!(
						"WalkDirFromHashMap::new encountered directory with non-UUID parent {:?} should be impossible",
						dir.parent()
					),
				));
			};
			map.entry(Uuid::from(parent_uuid))
				.or_default()
				.push(NonRootFSObject::Dir(Cow::Owned(dir)));
		}
		for file in files {
			let ParentUuid::Uuid(parent_uuid) = file.parent() else {
				return Err(Error::custom(
					crate::ErrorKind::Internal,
					format!(
						"WalkDirFromHashMap::new encountered directory with non-UUID parent {:?} should be impossible",
						file.parent()
					),
				));
			};
			map.entry(Uuid::from(parent_uuid))
				.or_default()
				.push(NonRootFSObject::File(Cow::Owned(file)));
		}
		let stack = vec![root_uuid];
		Ok(Self { map, stack })
	}
}

impl Iterator for WalkDirFromHashMap {
	type Item = Result<RemoteFSObjectEntry<'static>, WalkError>;

	fn next(&mut self) -> Option<Self::Item> {
		let current_parent = self.stack.last()?;
		let current_children = match self.map.get_mut(current_parent) {
			None => {
				self.stack.pop();
				return self.next();
			}
			Some(children) => children,
		};
		let obj = match current_children.pop() {
			None => {
				self.stack.pop();
				return self.next();
			}
			Some(obj) => obj,
		};

		let depth = self.stack.len();

		if let NonRootFSObject::Dir(dir) = &obj {
			self.stack.push(Uuid::from(dir.uuid()));
		}

		Some(Ok(RemoteFSObjectEntry::new(obj, depth)))
	}
}

pub(crate) async fn build_fs_tree_from_remote_iterator<F>(
	client: std::sync::Arc<crate::auth::Client>,
	dir: DirectoryType<'_>,
	error_callback: &mut impl FnMut(Vec<Error>),
	progress_callback: &mut impl FnMut(u64, u64, u64),
	list_dir_progress_callback: &F,
	should_cancel: &std::sync::atomic::AtomicBool,
) -> Result<(FSTree<RemoteDirectory, RemoteFile>, FSStats), Error>
where
	F: Fn(u64, Option<u64>) + Send + Sync,
{
	let root_uuid = dir.uuid().into();
	let (dirs, files) = client
		.list_dir_recursive(dir, list_dir_progress_callback)
		.await?;

	let iter = WalkDirFromHashMap::new(root_uuid, dirs, files)?;

	super::build_fs_tree(iter, error_callback, progress_callback, should_cancel)
}
