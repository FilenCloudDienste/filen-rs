use std::cmp::Ordering;

use uuid::Uuid;

use crate::{
	fs::{
		HasName, HasRemoteInfo, HasUUID,
		categories::{Category, NonRootItemType},
		dir::traits::HasDirInfo,
		file::traits::HasFileInfo,
	},
	io::fs_tree::{
		WalkError,
		entry::{DFSWalkerDirEntry, DFSWalkerFileEntry},
	},
};

pub(crate) struct RemoteFSObjectEntry<'a, Cat: Category + ?Sized> {
	obj: NonRootItemType<'a, Cat>,
	depth: usize,
}

impl<'a, Cat: Category + ?Sized> RemoteFSObjectEntry<'a, Cat> {
	pub(crate) fn new(obj: NonRootItemType<'a, Cat>, depth: usize) -> Self {
		Self { obj, depth }
	}

	pub(crate) fn depth(&self) -> usize {
		self.depth
	}

	pub(crate) fn obj(&self) -> &NonRootItemType<'a, Cat> {
		&self.obj
	}

	pub(crate) fn into_obj(self) -> NonRootItemType<'a, Cat> {
		self.obj
	}
}

impl<Cat: Category + ?Sized> super::DFSWalkerEntry for RemoteFSObjectEntry<'_, Cat>
where
	Cat::File: DFSWalkerFileEntry<Extra = Cat::File>,
	Cat::Dir: DFSWalkerDirEntry<Extra = Cat::Dir>,
{
	type WalkerFileEntry = Cat::File;
	type WalkerDirEntry = Cat::Dir;
	type CompareStrategy = RemoteCompareStrategy<Cat>;

	fn depth(&self) -> usize {
		self.depth
	}

	fn name(&self) -> Result<&str, WalkError> {
		HasName::name(&self.obj).ok_or_else(|| WalkError::EncryptedMeta(self.obj.uuid().into()))
	}

	fn into_entry_type(self) -> super::EntryType<Cat::Dir, Cat::File> {
		match self.obj {
			NonRootItemType::<Cat>::Dir(dir) => super::EntryType::Dir(dir.into_owned()),
			NonRootItemType::<Cat>::File(file) => super::EntryType::File(file.into_owned()),
		}
	}
}

impl<T> super::DFSWalkerFileEntry for T
where
	T: HasFileInfo + Clone + 'static,
{
	type Extra = T;

	fn into_extra_data(self) -> Self::Extra {
		self
	}

	fn size(&self) -> Result<u64, WalkError> {
		Ok(HasFileInfo::size(self))
	}
}

impl<T> super::DFSWalkerDirEntry for T
where
	T: HasDirInfo + Clone + 'static,
{
	type Extra = Self;

	fn into_extra_data(self) -> Self::Extra {
		self
	}
}

pub(crate) struct RemoteCompareStrategy<Cat: Category + ?Sized> {
	_category: std::marker::PhantomData<Cat>,
}

impl<Cat: Category + ?Sized> super::CompareStrategy<Cat::Dir, Cat::File>
	for RemoteCompareStrategy<Cat>
{
	fn should_replace(
		existing: &super::Entry<Cat::Dir, Cat::File>,
		new: &super::Entry<Cat::Dir, Cat::File>,
	) -> bool {
		let replace = {
			match (existing, new) {
				// prefer dirs over files
				(super::Entry::Dir(_), super::Entry::File(_)) => false,
				(super::Entry::File(_), super::Entry::Dir(_)) => true,
				(super::Entry::File(existing), super::Entry::File(new)) => {
					let existing = existing.extra_data();
					let new = new.extra_data();
					match (existing.last_modified(), new.last_modified()) {
						(Some(existing_modified), Some(new_modified)) => {
							match existing_modified.cmp(&new_modified) {
								Ordering::Less => return true,
								Ordering::Greater => return false,
								Ordering::Equal => {}
							}
						}
						(_, Some(_)) => return true,
						_ => {}
					}
					match existing.timestamp().cmp(&new.timestamp()) {
						Ordering::Less => true,
						Ordering::Greater => false,
						Ordering::Equal => Uuid::from(existing.uuid()) < Uuid::from(new.uuid()),
					}
				}
				(super::Entry::Dir(existing), super::Entry::Dir(new)) => {
					let existing = existing.extra_data();
					let new = new.extra_data();
					match (existing.created(), new.created()) {
						(Some(existing_created), Some(new_created)) => {
							match existing_created.cmp(&new_created) {
								Ordering::Less => return true,
								Ordering::Greater => return false,
								Ordering::Equal => {}
							}
						}
						(_, Some(_)) => return true,
						_ => {}
					};
					match existing.timestamp().cmp(&new.timestamp()) {
						Ordering::Less => true,
						Ordering::Greater => false,
						Ordering::Equal => Uuid::from(existing.uuid()) < Uuid::from(new.uuid()),
					}
				}
			}
		};

		log::info!(
			"Conflict detected between entries. Existing: {:?}, New: {:?}. Resolved by choosing new: {}",
			existing,
			new,
			replace
		);
		replace
	}
}
