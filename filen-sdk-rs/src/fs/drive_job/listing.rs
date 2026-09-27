//! Listing the sources of a job that recreates drive items (a copy, a compressed archive):
//! each source directory recursively, with progress, and pausable and cancellable like the rest
//! of the job.

use std::{
	borrow::Cow,
	sync::atomic::{AtomicU64, Ordering},
};

use filen_macros::js_type;
use filen_types::{
	fs::{ParentUuid, Uuid},
	traits::CowHelpers,
};

use crate::{
	Error,
	auth::Client,
	connect::{DirPublicLink, fs::SharingRole},
	consts::CALLBACK_INTERVAL,
	fs::{
		HasName, HasParent, HasUUID,
		categories::{DirType, Linked, Normal, Shared, fs::CategoryFS},
		dir::{
			RemoteDirectory,
			traits::{HasDirInfo, HasRemoteDirInfo},
		},
		file::enums::RemoteFileType,
	},
	job::{JobControl, Stopped, report::Ops},
	util::sleep,
};

use super::plan::{Listed, PlanSource, SourceDir};

/// A directory whose items a job recreates (copies, compresses), with what is needed to list it.
#[derive(Debug, Clone)]
pub enum ItemSourceDir {
	/// One of the user's own directories.
	Normal(RemoteDirectory),
	/// A directory shared with (or by) the user.
	Shared(DirType<'static, Shared>, SharingRole),
	/// A directory in a public link.
	Linked(DirType<'static, Linked>, DirPublicLink),
}

/// An item a job recreates: a file, or a directory with everything below it.
#[derive(Debug, Clone)]
pub enum ItemSource {
	File(RemoteFileType<'static>),
	Dir(ItemSourceDir),
}

/// The source of a failed item: a file can be addressed again as is; a directory is addressed
/// through the handle the caller attached to it.
#[derive(Debug, Clone)]
pub enum FailedSource<D> {
	File(Box<RemoteFileType<'static>>),
	Dir(D),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct ScanProgress {
	pub sources_done: u64,
	pub sources_total: u64,
	/// Bytes of listing responses received so far, and the expected total when known.
	pub listing_bytes: u64,
	pub listing_total_bytes: Option<u64>,
}

/// The source directory for the root of a listing that can start at a category root; `handle`
/// takes the root to address it again.
pub(crate) fn root_source_dir<Cat>(
	root: DirType<'static, Cat>,
	handle: impl FnOnce(DirType<'static, Cat>) -> ItemSourceDir,
) -> SourceDir<ItemSourceDir>
where
	Cat: CategoryFS,
	Cat::Root: HasName + HasDirInfo + HasRemoteDirInfo,
	Cat::Dir: HasRemoteDirInfo,
{
	match root {
		DirType::Root(root) => {
			let color = root.color().into_owned_cow();
			SourceDir::new(root.into_owned(), color, |root| {
				handle(DirType::Root(Cow::Owned(root)))
			})
		}
		DirType::Dir(dir) => {
			let color = dir.color().into_owned_cow();
			SourceDir::new(dir.into_owned(), color, |dir| {
				handle(DirType::Dir(Cow::Owned(dir)))
			})
		}
	}
}

/// Bytes of listing responses received by earlier sources, and by the one being listed.
#[derive(Default)]
pub(crate) struct ListingBytes {
	pub(crate) done: AtomicU64,
	pub(crate) current: AtomicU64,
	/// `u64::MAX` while unknown.
	pub(crate) current_total: AtomicU64,
}

impl ListingBytes {
	pub(crate) fn scan(&self, sources_done: u64, sources_total: u64) -> ScanProgress {
		let done = self.done.load(Ordering::Relaxed);
		let current = self.current.load(Ordering::Relaxed);
		let total = self.current_total.load(Ordering::Relaxed);
		ScanProgress {
			sources_done,
			sources_total,
			listing_bytes: done + current,
			listing_total_bytes: (total != u64::MAX).then(|| done + total),
		}
	}

	pub(crate) fn next_source(&self) {
		let current = self.current.swap(0, Ordering::Relaxed);
		self.done.fetch_add(current, Ordering::Relaxed);
		self.current_total.store(u64::MAX, Ordering::Relaxed);
	}
}

/// The directories and files below a source directory.
pub(crate) type Listing = (
	Vec<Listed<SourceDir<ItemSourceDir>>>,
	Vec<Listed<RemoteFileType<'static>>>,
);

/// The directory a listed entry is in. The planner keys entries by directory uuid; an entry
/// listed under anything else is left out, and logged.
fn listed_parent(uuid: Uuid, parent: ParentUuid) -> Option<Uuid> {
	let ParentUuid::Uuid(parent) = parent else {
		tracing::warn!(
			"leaving out listed entry {uuid}, whose parent {parent:?} is not a directory"
		);
		return None;
	};
	Some(parent)
}

/// Lists `root` recursively. `handle_of` takes a listed directory to address it again for a
/// retry.
async fn list_source<Cat>(
	client: &Cat::Client,
	root: &DirType<'_, Cat>,
	context: Cat::ListDirContext<'_>,
	handle_of: impl Fn(Cat::Dir) -> ItemSourceDir,
	bytes: &ListingBytes,
) -> Result<Listing, Error>
where
	Cat: CategoryFS,
	Cat::Dir: HasRemoteDirInfo,
	RemoteFileType<'static>: From<Cat::File>,
{
	let progress = |received: u64, total: Option<u64>| {
		bytes.current.store(received, Ordering::Relaxed);
		bytes
			.current_total
			.store(total.unwrap_or(u64::MAX), Ordering::Relaxed);
	};
	let (dirs, files) = Cat::list_dir_recursive(client, root, Some(&progress), context).await?;
	let dirs = dirs
		.into_iter()
		.filter_map(|dir| {
			let parent = listed_parent(dir.uuid(), *dir.parent())?;
			let color = dir.color().into_owned_cow();
			let item = SourceDir::new(dir, color, &handle_of);
			Some(Listed { parent, item })
		})
		.collect();
	let files = files
		.into_iter()
		.filter_map(|file| {
			let parent = listed_parent(file.uuid(), *file.parent())?;
			Some(Listed {
				parent,
				item: RemoteFileType::from(file),
			})
		})
		.collect();
	Ok((dirs, files))
}

impl Client {
	/// Lists a source directory recursively, for the planner.
	pub(crate) async fn list_item_source(
		&self,
		dir: ItemSourceDir,
		bytes: &ListingBytes,
	) -> Result<PlanSource<ItemSourceDir>, Error> {
		let (root, (dirs, files)) = match dir {
			ItemSourceDir::Normal(dir) => {
				let listing = list_source::<Normal>(
					self,
					&DirType::Dir(Cow::Borrowed(&dir)),
					(),
					ItemSourceDir::Normal,
					bytes,
				)
				.await?;
				let color = dir.color().into_owned_cow();
				(SourceDir::new(dir, color, ItemSourceDir::Normal), listing)
			}
			ItemSourceDir::Shared(root, role) => {
				// every listed directory's handle owns the role it is listed again with on retry
				let listing = list_source::<Shared>(
					self,
					&root,
					&role,
					|dir| ItemSourceDir::Shared(DirType::Dir(Cow::Owned(dir)), role.clone()),
					bytes,
				)
				.await?;
				let root = root_source_dir(root, |root| ItemSourceDir::Shared(root, role));
				(root, listing)
			}
			ItemSourceDir::Linked(root, link) => {
				// every listed directory's handle owns the link it is listed again with on retry
				let listing = list_source::<Linked>(
					self.unauthed(),
					&root,
					Cow::Borrowed(&link),
					|dir| ItemSourceDir::Linked(DirType::Dir(Cow::Owned(dir)), link.clone()),
					bytes,
				)
				.await?;
				let root = root_source_dir(root, |root| ItemSourceDir::Linked(root, link));
				(root, listing)
			}
		};
		Ok(PlanSource::Dir { root, dirs, files })
	}
}

/// Runs a listing, reporting scan progress while it downloads. A cancel drops it; a pause
/// lets it finish (it holds no transfer memory), and the job counts as pausing until it has.
pub(crate) async fn watch_listing<T>(
	listing: impl Future<Output = T>,
	ops: &Ops,
	control: &JobControl,
	report: impl Fn(),
) -> Result<T, ScanError> {
	let _op = ops.op();
	let listing = std::pin::pin!(control.until_stopping(listing));
	let ticker = async {
		loop {
			sleep(CALLBACK_INTERVAL).await;
			ops.set_pause_requested(control.is_pause_requested());
			report();
		}
	};
	let result = tokio::select! {
		result = listing => result,
		_ = ticker => unreachable!("the ticker never ends"),
	};
	result.map_err(|Stopped| {
		ops.set_cancelling();
		ScanError::Stopped
	})
}

pub(crate) enum ScanError {
	Stopped,
	Failed(Error),
}

/// For a stop the reporter was already told about, as `Reporter::checkpoint` does.
impl From<Stopped> for ScanError {
	fn from(_: Stopped) -> Self {
		Self::Stopped
	}
}
