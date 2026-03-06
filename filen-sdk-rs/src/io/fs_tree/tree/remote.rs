use std::{borrow::Cow, collections::HashMap};

use crate::{
	Error,
	fs::{
		HasParent, HasUUID,
		categories::{Category, DirType, NonRootItemType, fs::CategoryFSExt},
	},
	io::fs_tree::{
		WalkError,
		entry::{DFSWalkerDirEntry, DFSWalkerFileEntry, remote::RemoteFSObjectEntry},
	},
};

use filen_types::fs::ParentUuid;
use uuid::Uuid;

use super::{FSStats, FSTree};

pub(crate) struct WalkDirFromHashMap<Cat: Category + ?Sized> {
	map: HashMap<Uuid, Vec<NonRootItemType<'static, Cat>>>,
	stack: Vec<Uuid>,
}

impl<Cat: Category + ?Sized> WalkDirFromHashMap<Cat> {
	pub fn new(root_uuid: Uuid, dirs: Vec<Cat::Dir>, files: Vec<Cat::File>) -> Result<Self, Error> {
		let mut map: HashMap<Uuid, Vec<NonRootItemType<'static, Cat>>> = HashMap::new();
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
				.push(NonRootItemType::<Cat>::Dir(Cow::Owned(dir)));
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
				.push(NonRootItemType::<Cat>::File(Cow::Owned(file)));
		}
		let stack = vec![root_uuid];
		Ok(Self { map, stack })
	}
}

impl<Cat> Iterator for WalkDirFromHashMap<Cat>
where
	Cat: Category + ?Sized,
{
	type Item = Result<RemoteFSObjectEntry<'static, Cat>, WalkError>;

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

		if let NonRootItemType::<Cat>::Dir(dir) = &obj {
			self.stack.push(Uuid::from(dir.uuid()));
		}

		Some(Ok(RemoteFSObjectEntry::new(obj, depth)))
	}
}

#[allow(private_bounds)]
pub(crate) async fn build_fs_tree_from_remote_iterator<F, Cat>(
	client: &Cat::Client,
	dir: DirType<'_, Cat>,
	error_callback: &mut impl FnMut(Vec<Error>),
	progress_callback: &mut impl FnMut(u64, u64, u64),
	list_dir_progress_callback: Option<&F>,
	should_cancel: &std::sync::atomic::AtomicBool,
	context: Cat::ListDirContext<'_>,
) -> Result<(FSTree<Cat::Dir, Cat::File>, FSStats), Error>
where
	F: Fn(u64, Option<u64>) + Send + Sync,
	Cat: CategoryFSExt + ?Sized,
	Cat::File: DFSWalkerFileEntry<Extra = Cat::File>,
	Cat::Dir: DFSWalkerDirEntry<Extra = Cat::Dir>,
{
	let root_uuid = dir.uuid().into();
	let (dirs, files) =
		Cat::list_dir_recursive(client, &dir, list_dir_progress_callback, context).await?;
	let iter = WalkDirFromHashMap::<Cat>::new(root_uuid, dirs, files)?;

	super::build_fs_tree(iter, error_callback, progress_callback, should_cancel)
}
