//! The entries of an archive in the order they are written. A requested directory is walked
//! breadth first with a bounded number of listings running ahead of the entries being written,
//! and each directory's entry follows everything below it.

use std::{
	borrow::Cow,
	collections::VecDeque,
	mem,
	pin::Pin,
	sync::{Mutex, PoisonError},
	task::{Context, Poll},
	vec,
};

use chrono::{DateTime, Utc};
use futures::{
	Stream, StreamExt,
	future::ready,
	stream::{self, FuturesOrdered},
};

use crate::{
	Error,
	fs::{
		HasName, HasUUID,
		categories::{Category, DirType, fs::CategoryFS},
		dir::traits::HasDirInfo,
		file::{enums::RemoteFileType, traits::HasFileInfo},
	},
	util::{MaybeSend, MaybeSendBoxFuture, MaybeSendBoxStream},
};

use super::{ZipState, zip_entry_path};

/// Directory listings one walk holds at once, running or listed but not yet written.
const LISTING_WINDOW: usize = 4;

/// An item of the archive, in the order it is written.
// Nearly every entry is a file: boxing it would allocate once per file to shrink the few
// directory entries.
#[allow(clippy::large_enum_variant)]
pub(super) enum Entry<'a> {
	/// A file, stored at `path`, or skipped when its name could not be decrypted.
	File {
		file: RemoteFileType<'a>,
		path: Option<String>,
	},
	/// A requested or listed directory, once everything below it is written. A root has no entry
	/// of its own.
	Dir(Option<DirEntry>),
}

impl<'a> Entry<'a> {
	/// The entry of `file`, stored in the directory at `parent_path`.
	pub(super) fn file(file: RemoteFileType<'a>, parent_path: &str) -> Self {
		let path = file
			.name()
			.map(|name| zip_entry_path(parent_path, name, file.uuid()));
		Self::File { file, path }
	}
}

/// The entries of a requested file: the file alone.
pub(super) fn file_entries(entry: Entry<'_>) -> MaybeSendBoxStream<'_, Result<Entry<'_>, Error>> {
	Box::pin(stream::once(ready(Ok(entry))))
}

/// The entries of a requested directory.
pub(super) fn dir_entries<'a, Cat, L>(
	walk: DirWalk<'a, Cat, L>,
) -> MaybeSendBoxStream<'a, Result<Entry<'a>, Error>>
where
	Cat: Category,
	L: DirLister<'a, Cat> + MaybeSend + 'a,
	RemoteFileType<'static>: From<Cat::File>,
{
	Box::pin(walk)
}

/// The entry of a directory.
pub(super) struct DirEntry {
	/// Its path in the archive, without the trailing slash.
	pub(super) path: String,
	pub(super) created: Option<DateTime<Utc>>,
}

impl DirEntry {
	/// The entry of `dir`, stored in the directory at `parent_path`.
	fn new(dir: &(impl HasName + HasUUID + HasDirInfo), parent_path: &str) -> Self {
		// An undecryptable name is unsafe and falls back to the uuid like any other.
		Self {
			path: zip_entry_path(parent_path, dir.name().unwrap_or_default(), dir.uuid()),
			created: dir.created(),
		}
	}
}

/// The directories and files directly in a directory.
type Listing<Cat> = (Vec<<Cat as Category>::Dir>, Vec<<Cat as Category>::File>);

/// Lists the directories of a walk: the client, or a test's fake.
pub(super) trait DirLister<'a, Cat: Category> {
	/// Lists `dir`. The listing does not borrow the lister.
	fn list(
		&self,
		dir: DirType<'a, Cat>,
	) -> impl Future<Output = Result<Listing<Cat>, Error>> + MaybeSend + use<'a, Cat, Self>;
}

/// Lists through a client, with what its category needs to list a directory.
pub(super) struct ClientLister<'a, 'ctx, Cat: CategoryFS> {
	pub(super) client: &'a Cat::Client,
	pub(super) context: Cat::ListDirContext<'ctx>,
}

impl<'a, 'ctx, Cat: CategoryFS> DirLister<'a, Cat> for ClientLister<'a, 'ctx, Cat> {
	fn list(
		&self,
		dir: DirType<'a, Cat>,
	) -> impl Future<Output = Result<Listing<Cat>, Error>> + MaybeSend + use<'a, 'ctx, Cat> {
		let client = self.client;
		let context = self.context.clone();
		async move { Cat::list_dir(client, &dir, None::<&fn(u64, Option<u64>)>, context).await }
	}
}

/// A directory found but not yet listed.
struct QueuedDir<'a, Cat: Category> {
	dir: DirType<'a, Cat>,
	/// `None` for a root, whose items go to the top of the archive.
	entry: Option<DirEntry>,
}

/// A listed directory, its subdirectories not yet queued and its files not yet handed out.
struct ListedDir<Cat: Category> {
	entry: Option<DirEntry>,
	dirs: Vec<Cat::Dir>,
	files: vec::IntoIter<Cat::File>,
}

impl<Cat: Category> ListedDir<Cat> {
	fn path(&self) -> &str {
		self.entry.as_ref().map_or("", |entry| &entry.path)
	}
}

/// The entries of one requested directory: its files and those of every directory below it,
/// directory by directory in breadth-first order, then the directories' own entries, each
/// before its parent's.
pub(super) struct DirWalk<'a, Cat: Category, L> {
	lister: L,
	state: &'a Mutex<ZipState>,
	unlisted: VecDeque<QueuedDir<'a, Cat>>,
	listing: FuturesOrdered<MaybeSendBoxFuture<'a, Result<ListedDir<Cat>, Error>>>,
	/// Listings done, in order, that wait for the ones before them to be handed out.
	listed: VecDeque<ListedDir<Cat>>,
	current: Option<ListedDir<Cat>>,
	/// Entries of the directories whose files are all handed out, in the order they were listed.
	written_dirs: Vec<Option<DirEntry>>,
}

// Nothing in the walk is pinned in place: its listings are boxed.
impl<Cat: Category, L> Unpin for DirWalk<'_, Cat, L> {}

impl<'a, Cat, L> DirWalk<'a, Cat, L>
where
	Cat: Category,
	L: DirLister<'a, Cat> + 'a,
{
	/// The walk of `dir`, a requested item, whose files and directories count towards `state`'s
	/// totals as they are listed.
	pub(super) fn new(dir: DirType<'a, Cat>, lister: L, state: &'a Mutex<ZipState>) -> Self {
		let entry = match &dir {
			DirType::Root(_) => None,
			DirType::Dir(dir) => Some(DirEntry::new(dir.as_ref(), "")),
		};
		Self {
			lister,
			state,
			unlisted: VecDeque::from([QueuedDir { dir, entry }]),
			listing: FuturesOrdered::new(),
			listed: VecDeque::new(),
			current: None,
			written_dirs: Vec::new(),
		}
	}

	fn start_listings(&mut self) {
		while self.listing.len() + self.listed.len() < LISTING_WINDOW {
			let Some(QueuedDir { dir, entry }) = self.unlisted.pop_front() else {
				return;
			};
			let listing = self.lister.list(dir);
			let state = self.state;
			self.listing.push_back(Box::pin(async move {
				let (dirs, files) = listing.await?;
				count_listed(state, &dirs, &files);
				Ok(ListedDir {
					entry,
					dirs,
					files: files.into_iter(),
				})
			}));
		}
	}

	fn queue_subdirs(&mut self, listed: &mut ListedDir<Cat>) {
		let dirs = mem::take(&mut listed.dirs);
		let parent_path = listed.path();
		self.unlisted.extend(dirs.into_iter().map(|dir| QueuedDir {
			entry: Some(DirEntry::new(&dir, parent_path)),
			dir: DirType::Dir(Cow::Owned(dir)),
		}));
	}
}

/// Adds a listing's directories and files to the archive's totals.
fn count_listed(state: &Mutex<ZipState>, dirs: &[impl Sized], files: &[impl HasFileInfo]) {
	let items = u64::try_from(dirs.len().saturating_add(files.len())).unwrap_or(u64::MAX);
	let bytes = files
		.iter()
		.fold(0u64, |sum, file| sum.saturating_add(file.size()));
	let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
	state.total_items = state.total_items.saturating_add(items);
	state.total_bytes = state.total_bytes.saturating_add(bytes);
}

impl<'a, Cat, L> Stream for DirWalk<'a, Cat, L>
where
	Cat: Category,
	L: DirLister<'a, Cat> + 'a,
	RemoteFileType<'static>: From<Cat::File>,
{
	type Item = Result<Entry<'a>, Error>;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let this = self.get_mut();
		loop {
			this.start_listings();
			// Polled whenever the walk is, files left or not. While the window ahead is full
			// nothing polls the walk, so a listing then only gets as far as its request, sent
			// when it was first polled, and what of its response the HTTP stack buffers on its
			// own. The window's entries are being written meanwhile; driving the listings from
			// there too would need the walk outside the entry stream, for one round trip saved
			// at most per window's worth of entries.
			while let Poll::Ready(Some(listed)) = this.listing.poll_next_unpin(cx) {
				// a failed listing loses the archive: report it now, not after the entries ahead
				this.listed.push_back(listed?);
			}

			if let Some(current) = &mut this.current {
				if let Some(file) = current.files.next() {
					let file = RemoteFileType::<'static>::from(file);
					let entry = Entry::file(file, current.path());
					return Poll::Ready(Some(Ok(entry)));
				}
				if let Some(done) = this.current.take() {
					this.written_dirs.push(done.entry);
				}
			}

			match this.listed.pop_front() {
				Some(mut listed) => {
					this.queue_subdirs(&mut listed);
					this.current = Some(listed);
				}
				None if this.listing.is_empty() && this.unlisted.is_empty() => {
					// every file is out: the directories follow, last listed first
					return Poll::Ready(this.written_dirs.pop().map(|entry| Ok(Entry::Dir(entry))));
				}
				None => return Poll::Pending,
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{
		collections::HashMap,
		sync::atomic::{AtomicUsize, Ordering},
	};

	use filen_types::fs::Uuid;
	use futures::executor::block_on;

	use super::*;
	use crate::{
		ErrorKind,
		fs::{
			categories::Normal,
			dir::{RemoteDirectory, RootDirectory},
			drive_job::test_support::{drive_dir, drive_file},
			file::RemoteFile,
		},
	};

	fn dir(id: u128, name: &str) -> RemoteDirectory {
		drive_dir(Uuid::from_u128(id), name)
	}

	fn file(id: u128, name: &str, size: u64) -> RemoteFile {
		drive_file(Uuid::from_u128(id), name, size)
	}

	/// A drive held in memory, counting the listings asked of it.
	#[derive(Default)]
	struct FakeLister {
		listings: HashMap<Uuid, (Vec<RemoteDirectory>, Vec<RemoteFile>)>,
		calls: AtomicUsize,
		/// The directory whose listing fails.
		fail_listing: Option<Uuid>,
	}

	impl FakeLister {
		fn add(&mut self, parent: u128, dirs: Vec<RemoteDirectory>, files: Vec<RemoteFile>) {
			self.listings.insert(Uuid::from_u128(parent), (dirs, files));
		}
	}

	impl<'a, 'l> DirLister<'a, Normal> for &'l FakeLister {
		fn list(
			&self,
			dir: DirType<'a, Normal>,
		) -> impl Future<Output = Result<Listing<Normal>, Error>> + MaybeSend + use<'a, 'l> {
			self.calls.fetch_add(1, Ordering::Relaxed);
			if self.fail_listing == Some(dir.uuid()) {
				return ready(Err(Error::custom(ErrorKind::Server, "listing failed")));
			}
			ready(Ok(self
				.listings
				.get(&dir.uuid())
				.cloned()
				.unwrap_or_default()))
		}
	}

	fn entry_name(entry: &Entry<'_>) -> String {
		match entry {
			Entry::File { path, .. } => path.clone().unwrap(),
			Entry::Dir(Some(dir)) => format!("{}/", dir.path),
			Entry::Dir(None) => "(root)".to_owned(),
		}
	}

	#[test]
	fn writes_files_breadth_first_and_each_directory_after_everything_below_it() {
		let top = dir(1, "top");
		let mut lister = FakeLister::default();
		lister.add(
			1,
			vec![dir(2, "a"), dir(3, "b")],
			vec![file(10, "f1", 5), file(11, "f2", 7)],
		);
		lister.add(2, vec![], vec![file(12, "a1", 1)]);
		lister.add(3, vec![dir(4, "c")], vec![]);
		lister.add(4, vec![], vec![file(13, "c1", 2)]);
		let state = Mutex::new(ZipState::new(0, 1));

		let walk = DirWalk::new(DirType::Dir(Cow::Borrowed(&top)), &lister, &state);
		let names: Vec<String> = block_on(walk.map(|entry| entry_name(&entry.unwrap())).collect());

		assert_eq!(
			names,
			[
				"top/f1",
				"top/f2",
				"top/a/a1",
				"top/b/c/c1",
				"top/b/c/",
				"top/b/",
				"top/a/",
				"top/",
			]
		);
		let state = state.into_inner().unwrap();
		// the requested directory, then the 4 + 1 + 1 + 1 items its listings hold
		assert_eq!((state.total_items, state.total_bytes), (8, 15));
	}

	#[test]
	fn a_root_puts_its_items_at_the_top_and_has_no_entry_of_its_own() {
		let root = RootDirectory::new(Uuid::from_u128(1));
		let mut lister = FakeLister::default();
		lister.add(1, vec![dir(2, "a")], vec![file(10, "f", 1)]);
		let state = Mutex::new(ZipState::new(0, 1));

		let walk = DirWalk::<Normal, _>::new(DirType::Root(Cow::Borrowed(&root)), &lister, &state);
		let names: Vec<String> = block_on(walk.map(|entry| entry_name(&entry.unwrap())).collect());

		assert_eq!(names, ["f", "a/", "(root)"]);
	}

	#[test]
	fn an_unsafe_directory_name_falls_back_to_its_uuid() {
		let top = dir(1, "..");
		let mut lister = FakeLister::default();
		lister.add(1, vec![], vec![file(10, "f", 1)]);
		let state = Mutex::new(ZipState::new(0, 1));

		let walk = DirWalk::new(DirType::Dir(Cow::Borrowed(&top)), &lister, &state);
		let names: Vec<String> = block_on(walk.map(|entry| entry_name(&entry.unwrap())).collect());

		let uuid = Uuid::from_u128(1);
		assert_eq!(names, [format!("{uuid}/f"), format!("{uuid}/")]);
	}

	#[test]
	fn lists_at_most_a_window_of_directories_ahead_of_the_one_being_written() {
		let top = dir(1, "top");
		let mut lister = FakeLister::default();
		let subdirs: Vec<RemoteDirectory> =
			(0..20).map(|i| dir(100 + i, &format!("d{i}"))).collect();
		for i in 0..20 {
			lister.add(100 + i, vec![], vec![file(200 + i, "f", 1)]);
		}
		lister.add(1, subdirs, vec![]);
		let state = Mutex::new(ZipState::new(0, 1));

		let mut walk = DirWalk::new(DirType::Dir(Cow::Borrowed(&top)), &lister, &state);
		let first = block_on(walk.next()).unwrap().unwrap();

		assert_eq!(entry_name(&first), "top/d0/f");
		// `top` and `d0` are taken; the window holds the next ones
		assert_eq!(lister.calls.load(Ordering::Relaxed), 2 + LISTING_WINDOW);
	}

	#[test]
	fn a_failed_listing_is_reported_before_the_files_ahead_of_it() {
		let top = dir(1, "top");
		let mut lister = FakeLister::default();
		lister.add(
			1,
			vec![dir(2, "a")],
			vec![file(10, "f1", 1), file(11, "f2", 1)],
		);
		lister.fail_listing = Some(Uuid::from_u128(2));
		let state = Mutex::new(ZipState::new(0, 1));

		let mut walk = DirWalk::new(DirType::Dir(Cow::Borrowed(&top)), &lister, &state);

		let error = block_on(walk.next()).unwrap().map(|_| ()).unwrap_err();
		assert_eq!(error.kind(), ErrorKind::Server);
	}
}
