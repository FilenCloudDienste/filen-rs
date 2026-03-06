use std::{borrow::Cow, sync::Arc};

use filen_types::fs::ObjectType;

use crate::{
	Error,
	error::InvalidTypeError,
	fs::{
		HasName, HasUUID,
		categories::{DirType, NonRootFileType, NonRootItemType},
	},
	util::{AtomicDropCanceller, MaybeSend, PathIteratorExt},
};

use super::Category;

pub struct DirSizeInfo {
	pub files: u64,
	pub dirs: u64,
	pub size: u64,
}

// `async_fn_in_trait` is allowed because these traits are internal-only (enforced by
// `private_bounds`) and we don't need to specify `Send` bounds on the returned futures.
#[allow(async_fn_in_trait)]
#[allow(private_bounds)]
pub trait CategoryFS: Category {
	type ListDirContext<'a>: Clone + Send;
	#[allow(clippy::type_complexity)]
	fn list_dir<F>(
		client: &Self::Client,
		parent: &DirType<'_, Self>,
		progress: Option<&F>,
		context: Self::ListDirContext<'_>,
	) -> impl Future<Output = Result<(Vec<Self::Dir>, Vec<Self::File>), Error>> + MaybeSend
	where
		F: Fn(u64, Option<u64>) + Send + Sync;

	async fn list_dir_recursive<F>(
		client: &Self::Client,
		parent: &DirType<'_, Self>,
		progress: Option<&F>,
		context: Self::ListDirContext<'_>,
	) -> Result<(Vec<Self::Dir>, Vec<Self::File>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync;

	async fn dir_size(
		client: &Self::Client,
		dir: &DirType<'_, Self>,
		context: Self::ListDirContext<'_>,
	) -> Result<DirSizeInfo, Error>;
}

pub(crate) enum ObjectMatch<T> {
	Name(T),
	Uuid(T),
}

pub(crate) fn find_item_in_dirs<Cat>(
	dirs: Vec<Cat::Dir>,
	name_or_uuid: &str,
) -> Option<ObjectMatch<Cat::Dir>>
where
	Cat: Category + ?Sized,
{
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

pub(crate) fn find_item_in_files<Cat>(
	files: Vec<Cat::File>,
	name_or_uuid: &str,
) -> Option<ObjectMatch<Cat::File>>
where
	Cat: Category + ?Sized,
{
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

fn inner_find_item_in_dirs_and_files<Cat>(
	dirs: Vec<Cat::Dir>,
	files: Vec<Cat::File>,
	name_or_uuid: &str,
) -> Option<NonRootItemType<'static, Cat>>
where
	Cat: Category + ?Sized,
{
	let uuid_match = match find_item_in_dirs::<Cat>(dirs, name_or_uuid) {
		Some(ObjectMatch::Name(dir)) => {
			return Some(NonRootItemType::Dir(Cow::Owned(dir)));
		}
		Some(ObjectMatch::Uuid(dir)) => Some(dir),
		None => None,
	};
	match find_item_in_files::<Cat>(files, name_or_uuid) {
		Some(ObjectMatch::Name(file)) => Some(NonRootItemType::File(Cow::Owned(file))),
		Some(ObjectMatch::Uuid(file)) => Some(NonRootItemType::File(Cow::Owned(file))),
		None => uuid_match.map(|dir| NonRootItemType::Dir(Cow::Owned(dir))),
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectOrRemainingPath<'a, 'b, Cat: Category + ?Sized> {
	Object(NonRootFileType<'a, Cat>),
	RemainingPath(&'b str),
}
pub type GetItemsResponseSuccess<'a, 'b, Cat: Category + ?Sized> =
	(Vec<DirType<'a, Cat>>, ObjectOrRemainingPath<'a, 'b, Cat>);
pub type GetItemsResponseError<'a, Cat: Category + ?Sized> =
	(Vec<DirType<'a, Cat>>, NonRootFileType<'a, Cat>);

// `async_fn_in_trait` is allowed because this is an internal blanket-impl extension trait
// (enforced by `private_bounds`) and `Send` bounds on futures are not required here.
#[allow(async_fn_in_trait)]
#[allow(private_bounds)]
pub trait CategoryFSExt: CategoryFS {
	async fn find_item_in_dir<F>(
		client: &Self::Client,
		parent: &DirType<'_, Self>,
		progress_callback: Option<&F>,
		name_or_uuid: &str,
		context: Self::ListDirContext<'_>,
	) -> Result<Option<NonRootItemType<'static, Self>>, Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		let (dirs, files) = Self::list_dir::<F>(client, parent, progress_callback, context).await?;
		Ok(inner_find_item_in_dirs_and_files::<Self>(
			dirs,
			files,
			name_or_uuid,
		))
	}

	// todo, lock drive in normal category
	async fn get_items_in_path_starting_at<'a, 'b>(
		client: &'a Self::Client,
		path: &'b str,
		mut curr_dir: DirType<'a, Self>,
		context: Self::ListDirContext<'_>,
	) -> Result<
		GetItemsResponseSuccess<'a, 'b, Self>,
		(Error, GetItemsResponseError<'a, Self>, &'b str),
	> {
		let mut dirs: Vec<DirType<'a, Self>> =
			Vec::with_capacity(path.chars().filter(|c| *c == '/').count() + 1);

		let mut path_iter = path.path_iter().peekable();
		let mut last_rest_of_path = path;
		while let Some((component, rest_of_path)) = path_iter.next() {
			match Self::find_item_in_dir(
				client,
				&curr_dir,
				None::<&fn(u64, Option<u64>)>,
				component,
				context.clone(),
			)
			.await
			{
				Ok(Some(NonRootItemType::Dir(dir))) => {
					let old_dir = std::mem::replace(&mut curr_dir, DirType::<Self>::Dir(dir));
					dirs.push(old_dir);
					if path_iter.peek().is_none() {
						return Ok((dirs, ObjectOrRemainingPath::Object(curr_dir.into())));
					}
					last_rest_of_path = rest_of_path;
					continue;
				}
				Ok(Some(NonRootItemType::File(file))) => {
					let file = NonRootFileType::<Self>::File(file);
					dirs.push(curr_dir);
					if path_iter.peek().is_some() {
						return Err((
							InvalidTypeError {
								actual: ObjectType::File,
								expected: ObjectType::Dir,
							}
							.into(),
							(dirs, file),
							rest_of_path,
						));
					}
					return Ok((dirs, ObjectOrRemainingPath::Object(file)));
				}
				Ok(None) => {
					dirs.push(curr_dir);
					return Ok((
						dirs,
						ObjectOrRemainingPath::RemainingPath(last_rest_of_path),
					));
				}
				Err(e) => return Err((e, (dirs, curr_dir.into()), rest_of_path)),
			}
		}
		Ok((dirs, ObjectOrRemainingPath::Object(curr_dir.into())))
	}

	async fn list_dir_recursive_with_paths<F, F1>(
		client: Arc<Self::Client>,
		dir: DirType<'_, Self>,
		list_dir_progress_callback: Option<&F>,
		scan_errors_callback: &mut F1,
		context: Self::ListDirContext<'_>,
	) -> Result<(Vec<(Self::Dir, String)>, Vec<(Self::File, String)>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
		F1: FnMut(Vec<Error>),
	{
		let drop_canceller = AtomicDropCanceller::default();

		let (tree, stats) = crate::io::fs_tree::build_fs_tree_from_remote_iterator::<F, Self>(
			&client,
			dir,
			scan_errors_callback,
			&mut |_dirs, _files, _bytes| {
				// this can be a noop because we download everything all at once and then scan it
				// which means that this should be very fast
			},
			list_dir_progress_callback,
			drop_canceller.cancelled(),
			context,
		)
		.await?;

		let iter = tree.dfs_iter_with_path("");
		let (num_dirs, num_files, _) = stats.snapshot();

		let mut files = Vec::with_capacity(num_files as usize);
		let mut dirs = Vec::with_capacity(num_dirs as usize);

		for (entry, path) in iter {
			match entry {
				crate::io::fs_tree::Entry::Dir(dir_entry) => {
					dirs.push((dir_entry.extra_data().clone(), path))
				}
				crate::io::fs_tree::Entry::File(file_entry) => {
					files.push((file_entry.extra_data().clone(), path))
				}
			}
		}

		Ok((dirs, files))
	}
}

impl<T> CategoryFSExt for T where T: CategoryFS + ?Sized {}
