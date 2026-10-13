use std::sync::Arc;

use chrono::{DateTime, Utc};
use filen_types::{api::v3::bulk::MAX_WRITE_ITEMS, fs::Uuid};

use super::Backend;
use crate::{
	api::v3::{
		bulk::{Answer, Sent, answered, code},
		dir::create::Response,
	},
	auth::Client,
	error::{Error, ErrorKind},
	fs::dir::client_impl::NewDirRequest,
};

/// A directory for [`Client::create_dirs`] to create.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NewDir<'a> {
	pub(crate) name: &'a str,
	pub(crate) created: DateTime<Utc>,
	pub(crate) parent: NewDirParent,
}

/// Where a [`NewDir`] goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NewDirParent {
	/// A directory that already exists.
	Existing(Uuid),
	/// The directory at this index of the same list, which must come before the child.
	Earlier(usize),
}

/// A directory [`Client::create_dirs`] created, or found already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CreatedDir {
	/// The directory's uuid, as the server answered it.
	pub(crate) uuid: Uuid,
	/// Whether a directory with the same name already existed in the parent: the server created
	/// nothing and answered that directory's uuid, which differs from the one requested.
	pub(crate) merged: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum CreateDirsError {
	/// A directory names a parent at its own index or after it; nothing was sent.
	#[error(
		"directory {item} names directory {parent} as its parent, which does not come before it"
	)]
	ParentNotEarlier { item: usize, parent: usize },
	/// The parent directory in the same list was not created, so the directory was not either.
	#[error("directory {item} was not created because its parent, directory {parent}, was not")]
	ParentNotCreated { item: usize, parent: usize },
}

impl From<CreateDirsError> for Error {
	fn from(e: CreateDirsError) -> Self {
		let kind = match e {
			CreateDirsError::ParentNotEarlier { .. } => ErrorKind::Internal,
			CreateDirsError::ParentNotCreated { .. } => ErrorKind::FolderNotFound,
		};
		Error::custom_with_source(kind, e, None::<&str>)
	}
}

impl Client {
	/// Creates `dirs` under one hold of the drive lock, `bulk/dir/create` requests of at most
	/// [`MAX_WRITE_ITEMS`] directories in list order, and answers each directory's result in the
	/// same order. Parents must come before their children.
	///
	/// A directory whose parent was not created fails without being sent. Unlike
	/// [`Client::create_dir`], it neither adds the new directories to the parent's links and
	/// shares nor fetches a directory found already there: the caller decides both.
	///
	/// The outer error is a lock failure, or a parent index that does not come before its child,
	/// under which nothing was sent. A request that fails as a whole fails each of its directories
	/// with that error, and some unknown prefix of them may have been created.
	pub(crate) async fn create_dirs(
		&self,
		dirs: &[NewDir<'_>],
	) -> Result<Vec<Result<CreatedDir, Error>>, Error> {
		let _lock = self.lock_drive().await?;
		create_dirs(self, dirs).await
	}
}

/// [`Client::create_dirs`] without the lock, through `backend`.
pub(super) async fn create_dirs(
	backend: &impl Backend,
	dirs: &[NewDir<'_>],
) -> Result<Vec<Result<CreatedDir, Error>>, Error> {
	for (item, dir) in dirs.iter().enumerate() {
		if let NewDirParent::Earlier(parent) = dir.parent
			&& parent >= item
		{
			return Err(CreateDirsError::ParentNotEarlier { item, parent }.into());
		}
	}
	let mut results = Vec::with_capacity(dirs.len());
	for (number, chunk) in dirs.chunks(MAX_WRITE_ITEMS).enumerate() {
		// cannot overflow: the chunks' items are all in `dirs`
		create_chunk(backend, chunk, number * MAX_WRITE_ITEMS, &mut results).await;
	}
	Ok(results)
}

/// A directory of a chunk before the chunk is sent.
enum Slot {
	Failed(Error),
	Minted {
		request: NewDirRequest,
		/// The parent the bulk request names: the uuid an earlier chunk's directory was answered,
		/// or the uuid this chunk's directory is requested under, which the server remaps.
		parent: Uuid,
	},
}

/// What the server made of a chunk.
enum Answers {
	Answered(std::vec::IntoIter<Answer<Response>>),
	Unsupported,
	Failed(Arc<Error>),
}

/// Creates `chunk`, whose first directory is directory `start` of the list, and appends each
/// directory's result to `results`, which holds the results of the list's earlier directories.
async fn create_chunk(
	backend: &impl Backend,
	chunk: &[NewDir<'_>],
	start: usize,
	results: &mut Vec<Result<CreatedDir, Error>>,
) {
	let mut slots = Vec::with_capacity(chunk.len());
	for (offset, dir) in chunk.iter().enumerate() {
		let parent = match dir.parent {
			NewDirParent::Existing(uuid) => Ok(uuid),
			NewDirParent::Earlier(parent) => {
				let uuid = match parent.checked_sub(start) {
					None => results
						.get(parent)
						.and_then(|result| result.as_ref().ok())
						.map(|created| created.uuid),
					Some(offset) => slots.get(offset).and_then(|slot| match slot {
						Slot::Minted { request, .. } => Some(request.uuid),
						Slot::Failed(_) => None,
					}),
				};
				uuid.ok_or(CreateDirsError::ParentNotCreated {
					item: start + offset,
					parent,
				})
			}
		};
		slots.push(match parent {
			Err(e) => Slot::Failed(e.into()),
			Ok(parent) => match backend.mint_dir(dir.name, dir.created).await {
				Ok(request) => Slot::Minted { request, parent },
				Err(e) => Slot::Failed(e),
			},
		});
	}

	let items = slots
		.iter()
		.filter_map(|slot| match slot {
			Slot::Minted { request, parent } => Some(request.in_parent(*parent)),
			Slot::Failed(_) => None,
		})
		.collect::<Vec<_>>();
	let mut answers = match backend.bulk_create_dirs(&items).await {
		Ok(Sent::Answers(answers)) => Answers::Answered(
			answers
				.into_iter()
				.map(|(_, answer)| answer)
				.collect::<Vec<_>>()
				.into_iter(),
		),
		Ok(Sent::Unsupported(_)) => Answers::Unsupported,
		Err(e) => Answers::Failed(Arc::new(e)),
	};
	// ends the borrow of `slots`, which the answers are matched with below
	drop(items);

	// whether the server created the chunk's directory in this request, so that it ran the
	// directory's children in the folder it answered
	let mut created_here = Vec::with_capacity(chunk.len());
	for (offset, (dir, slot)) in chunk.iter().zip(slots).enumerate() {
		let item = start + offset;
		let request = match slot {
			Slot::Minted { request, .. } => request,
			Slot::Failed(e) => {
				results.push(Err(e));
				created_here.push(false);
				continue;
			}
		};
		let answer = match &mut answers {
			Answers::Failed(e) => Some(Err(Error::unshared(Arc::clone(e)))),
			Answers::Unsupported => None,
			Answers::Answered(answers) => match answers.next().map(answered) {
				Some(Some(Ok(response))) => Some(Ok(created(&request, &response))),
				// its parent was created by its single call, after the server had run this
				// directory in the uuid the parent was requested under
				Some(Some(Err(e)))
					if code(&e) == Some("cannot_create_in_this_folder")
						&& parent_created_after(dir, start, &created_here) =>
				{
					None
				}
				Some(Some(Err(e))) => Some(Err(e)),
				// `internal_error`: only the single call can tell whether it ran
				Some(None) => None,
				None => Some(Err(Error::custom(
					ErrorKind::Internal,
					"fewer answers than directories (should be impossible: `send` checks the count)",
				))),
			},
		};
		// only an answer of this request is already a result
		created_here.push(matches!(answer, Some(Ok(_))));
		let result = match answer {
			Some(result) => result,
			None => create_one(backend, item, dir, &request, results).await,
		};
		results.push(result);
	}
}

/// Whether `dir`'s parent is a directory of the chunk starting at `start` that the server did not
/// create in the chunk's request.
fn parent_created_after(dir: &NewDir<'_>, start: usize, created_here: &[bool]) -> bool {
	match dir.parent {
		NewDirParent::Existing(_) => false,
		NewDirParent::Earlier(parent) => parent
			.checked_sub(start)
			.and_then(|offset| created_here.get(offset))
			.is_some_and(|created| !created),
	}
}

/// Creates directory `item` with its single call, in the folder its parent's create answered.
async fn create_one(
	backend: &impl Backend,
	item: usize,
	dir: &NewDir<'_>,
	request: &NewDirRequest,
	results: &[Result<CreatedDir, Error>],
) -> Result<CreatedDir, Error> {
	let parent = match dir.parent {
		NewDirParent::Existing(uuid) => uuid,
		NewDirParent::Earlier(parent) => match results.get(parent) {
			Some(Ok(created)) => created.uuid,
			_ => return Err(CreateDirsError::ParentNotCreated { item, parent }.into()),
		},
	};
	let response = backend.create_one_dir(&request.in_parent(parent)).await?;
	Ok(created(request, &response))
}

fn created(request: &NewDirRequest, response: &Response) -> CreatedDir {
	CreatedDir {
		uuid: response.uuid,
		merged: response.uuid != request.uuid,
	}
}
