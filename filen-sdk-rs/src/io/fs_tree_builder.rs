use std::{
	path::{Path, PathBuf},
	sync::atomic::AtomicBool,
};

use string_interner::{DefaultBackend, StringInterner, symbol::DefaultSymbol};

use crate::{Error, ErrorKind, consts::CALLBACK_INTERVAL, io::FilenMetaExt};

pub(crate) trait EntryName {
	fn name(&self) -> DefaultSymbol;
}

#[derive(Debug)]
pub(crate) struct FileEntry {
	name: DefaultSymbol,
}

impl EntryName for FileEntry {
	fn name(&self) -> DefaultSymbol {
		self.name
	}
}

struct UnfinalizedDirEntry {
	name: DefaultSymbol,
}

#[derive(Debug)]
pub(crate) struct DirEntry {
	name: DefaultSymbol,
	children_info: DirChildrenInfo,
}

impl EntryName for DirEntry {
	fn name(&self) -> DefaultSymbol {
		self.name
	}
}

impl DirEntry {
	pub(crate) fn children_info(&self) -> DirChildrenInfo {
		self.children_info
	}
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DirChildrenInfo {
	start_idx: u32,
	num_children: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum WalkError {
	#[error("detected a symlink loop at path {0:?}")]
	Loop(PathBuf),
	#[error("IO error at path {0:?}: {1}")]
	IO(Option<PathBuf>, std::io::Error),
	#[error("invalid file name at path {0:?}")]
	InvalidName(PathBuf),
}

#[derive(Debug)]
pub(crate) enum Entry {
	File(FileEntry),
	Dir(DirEntry),
}

impl EntryName for Entry {
	fn name(&self) -> DefaultSymbol {
		match self {
			Entry::File(f) => f.name(),
			Entry::Dir(d) => d.name(),
		}
	}
}

enum UnfinalizedEntry {
	File(FileEntry),
	Dir(UnfinalizedDirEntry),
}

pub(crate) struct FSTree {
	interner: StringInterner<DefaultBackend>,
	entries: Vec<Entry>,
}

impl FSTree {
	pub(crate) fn count_entries(&self) -> usize {
		self.entries.len()
	}

	pub(crate) fn list_children(&self, info: DirChildrenInfo) -> &[Entry] {
		let start = info.start_idx as usize;
		let end = start + info.num_children as usize;
		&self.entries[start..end]
	}

	pub(crate) fn root(&self) -> &Entry {
		&self.entries[self.entries.len() - 1]
	}

	pub(crate) fn get_name(&self, entry: &impl EntryName) -> &str {
		self.interner
			.resolve(entry.name())
			.expect("should resolve name")
	}
}

trait DFSIteratorEntryValue {
	fn path(&self) -> &Path;
	fn meta(&self) -> Result<std::fs::Metadata, std::io::Error>;
	fn file_type(&self) -> std::fs::FileType;
	fn depth(&self) -> usize;
}

impl DFSIteratorEntryValue for walkdir::DirEntry {
	fn path(&self) -> &Path {
		self.path()
	}
	fn meta(&self) -> Result<std::fs::Metadata, std::io::Error> {
		// metadata seems to always return an std::io::Error on failure
		self.metadata().map_err(|e| {
			e.into_io_error()
				.expect("metadata error should be io error")
		})
	}
	fn file_type(&self) -> std::fs::FileType {
		self.file_type()
	}
	fn depth(&self) -> usize {
		self.depth()
	}
}

pub(crate) struct FSStats {
	dirs: u64,
	files: u64,
	bytes: u64,
}

impl FSStats {
	fn new() -> Self {
		Self {
			dirs: 0,
			files: 0,
			bytes: 0,
		}
	}

	fn add_file(&mut self, size: u64) {
		self.files += 1;
		self.bytes += size;
	}

	fn add_dir(&mut self) {
		self.dirs += 1;
	}

	pub(crate) fn snapshot(&self) -> (u64, u64, u64) {
		(self.dirs, self.files, self.bytes)
	}
}

pub(crate) fn build_fs_tree_from_walkdir_iterator(
	root_path: &Path,
	error_callback: &impl Fn(Vec<WalkError>),
	progress_callback: &impl Fn(u64, u64, u64),
	should_cancel: &AtomicBool,
) -> Result<(FSTree, FSStats), Error> {
	let iter = walkdir::WalkDir::new(root_path)
		.follow_links(true)
		.into_iter()
		.filter_map(|res| match res {
			Ok(v) => Some(Ok(v)),
			// hope we get if let guards to make this cleaner someday
			// https://github.com/rust-lang/rust/issues/51114
			Err(e) if e.loop_ancestor().is_some() => {
				let path = e.loop_ancestor().unwrap().to_path_buf();
				Some(Err(WalkError::Loop(path)))
			}
			Err(e) => {
				let path = e.path().map(|p| p.to_path_buf());
				e.into_io_error()
					.map(|io_err| Err(WalkError::IO(path, io_err)))
			}
		});

	build_fs_tree(iter, error_callback, progress_callback, should_cancel)
}

/// Build a filesystem tree from a DFS iterator over filesystem entries.
///
/// The iterator must yield entries in depth-first order, with each entry providing
/// access to its path, metadata, and file type.
///
/// Returns an FSTree which is a maximally compact representation of the filesystem structure,
/// along with any errors encountered during the traversal.
fn build_fs_tree(
	dfs_iterator: impl Iterator<Item = Result<impl DFSIteratorEntryValue, WalkError>>,
	error_callback: &impl Fn(Vec<WalkError>),
	progress_callback: &impl Fn(u64, u64, u64),
	should_cancel: &AtomicBool,
) -> Result<(FSTree, FSStats), Error> {
	let mut interner = StringInterner::default();

	let mut ancestor_stack: Vec<(UnfinalizedDirEntry, Vec<Entry>)> = Vec::new();

	let mut final_entries: Vec<Entry> = Vec::new();

	let mut errors: Vec<WalkError> = Vec::new();

	let mut stats: FSStats = FSStats::new();
	let mut last_callback = std::time::Instant::now();

	for entry_result in dfs_iterator {
		let entry = match entry_result {
			Ok(e) => e,
			Err(err) => {
				errors.push(err);
				continue;
			}
		};
		adjust_stack_until_depth(&mut ancestor_stack, entry.depth(), &mut final_entries)?;

		let path = entry.path();
		let name = match path.file_name().and_then(|n| n.to_str()) {
			Some(n) => n,
			None => {
				errors.push(WalkError::InvalidName(path.to_path_buf()));
				continue;
			}
		};

		let name_symbol = interner.get_or_intern(name);

		let unfinalized_entry = if entry.file_type().is_dir() {
			stats.add_dir();
			UnfinalizedEntry::Dir(UnfinalizedDirEntry { name: name_symbol })
		} else if entry.file_type().is_file() {
			let metadata = match entry.meta() {
				Ok(m) => m,
				Err(e) => {
					errors.push(WalkError::IO(Some(path.to_path_buf()), e));
					continue;
				}
			};
			stats.add_file(FilenMetaExt::size(&metadata));
			UnfinalizedEntry::File(FileEntry { name: name_symbol })
		} else {
			// symlink, special file, etc.
			// skip
			continue;
		};

		match unfinalized_entry {
			UnfinalizedEntry::File(file_entry) => {
				ancestor_stack
					.last_mut()
					.expect("should have a parent directory for file entries")
					.1
					.push(Entry::File(file_entry));
			}
			UnfinalizedEntry::Dir(unfinalized_dir_entry) => {
				ancestor_stack.push((unfinalized_dir_entry, Vec::new()));
			}
		}

		// callbacks
		if std::time::Instant::now().duration_since(last_callback) >= CALLBACK_INTERVAL {
			last_callback = std::time::Instant::now();

			progress_callback(stats.dirs, stats.files, stats.bytes);
			if !errors.is_empty() {
				error_callback(std::mem::take(&mut errors));
			}
			if should_cancel.load(std::sync::atomic::Ordering::Relaxed) {
				Err(Error::custom(
					ErrorKind::Cancelled,
					"filesystem tree build cancelled",
				))?;
			}
		}
	}

	adjust_stack_until_depth(&mut ancestor_stack, 0, &mut final_entries)?;

	// shrink to fit to minimize memory usage
	interner.shrink_to_fit();
	final_entries.shrink_to_fit();

	if !errors.is_empty() {
		error_callback(errors);
	}

	Ok((
		FSTree {
			interner,
			entries: final_entries,
		},
		stats,
	))
}

fn adjust_stack_until_depth(
	stack: &mut Vec<(UnfinalizedDirEntry, Vec<Entry>)>,
	target_depth: usize,
	final_entries: &mut Vec<Entry>,
) -> Result<(), Error> {
	if stack.len() < target_depth {
		return Err(Error::custom(
			// todo fix this error type
			ErrorKind::IO,
			"cannot adjust ancestor stack to a deeper depth than current",
		));
	}

	while stack.len() > target_depth {
		// finalize the current ancestor level
		let (completed_parent, mut completed_children) = stack
			.pop()
			.expect("should have ancestor levels to pop when finalizing ancestors");

		let children_idx = final_entries.len() as u32;
		let children_count = completed_children.len() as u32;

		final_entries.append(&mut completed_children);
		let completed_parent = Entry::Dir(DirEntry {
			name: completed_parent.name,
			children_info: DirChildrenInfo {
				start_idx: children_idx,
				num_children: children_count,
			},
		});

		if let Some((_, parent_children)) = stack.last_mut() {
			parent_children.push(completed_parent);
		} else {
			final_entries.push(completed_parent);
		}
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	use std::alloc::{GlobalAlloc, Layout, System};
	use std::sync::atomic::{AtomicUsize, Ordering};

	static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

	struct TrackingAllocator;

	unsafe impl GlobalAlloc for TrackingAllocator {
		unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
			ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
			unsafe { System.alloc(layout) }
		}

		unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
			ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
			unsafe { System.dealloc(ptr, layout) }
		}
	}

	#[global_allocator]
	static GLOBAL: TrackingAllocator = TrackingAllocator;

	fn current_allocation() -> usize {
		ALLOCATED.load(Ordering::Relaxed)
	}

	#[test]
	fn test_walk() {
		env_logger::Builder::from_default_env()
			.filter_module("reqwest", log::LevelFilter::Info)
			.filter_module("html5ever", log::LevelFilter::Info)
			.filter_module("selectors", log::LevelFilter::Info)
			.init();
		let time = std::time::Instant::now();
		println!("Starting scan... {}", current_allocation());

		let (tree, stats) = build_fs_tree_from_walkdir_iterator(
			Path::new("/Users/end/Documents"),
			&|error| {
				log::error!("Errors during walk: {:?}", error);
			},
			&|dirs, files, bytes| {
				log::info!(
					"Scan progress - dirs: {}, files: {}, bytes: {} (allocated: {:.2} MiB)",
					dirs,
					files,
					bytes,
					current_allocation() as f64 / 1024.0 / 1024.0
				);
			},
			&AtomicBool::new(false),
		)
		.unwrap();

		let elapsed = time.elapsed();
		println!(
			"Total scanned dirs: {}, files: {}, bytes: {} in {} ms",
			stats.dirs,
			stats.files,
			stats.bytes,
			elapsed.as_millis()
		);
		println!(
			"Per second: dirs: {}, files: {}, bytes: {}",
			(stats.dirs as f64 / (elapsed.as_secs_f64())),
			(stats.files as f64 / (elapsed.as_secs_f64())),
			(stats.bytes as f64 / (elapsed.as_secs_f64())),
		);
		println!(
			"Total entries: {}, allocated: {:.2} MiB",
			tree.count_entries(),
			current_allocation() as f64 / 1024.0 / 1024.0
		);

		println!("root: {:?}", tree.entries[tree.entries.len() - 1]);
		println!("root children:");
		for child in tree.list_children(match &tree.entries[tree.entries.len() - 1] {
			Entry::Dir(d) => d.children_info(),
			_ => panic!("root should be a dir"),
		}) {
			let name = tree
				.interner
				.resolve(match child {
					Entry::Dir(d) => d.name,
					Entry::File(f) => f.name,
				})
				.expect("should resolve name");
			println!(" - {:?} - name {}", child, name);
			if let Entry::Dir(dir_entry) = child {
				println!("   dir children:");
				for grandchild in tree.list_children(dir_entry.children_info()) {
					let name = tree
						.interner
						.resolve(match grandchild {
							Entry::Dir(d) => d.name,
							Entry::File(f) => f.name,
						})
						.expect("should resolve name");
					println!("     - {:?} - name {}", grandchild, name);
				}
			}
		}
	}
}
