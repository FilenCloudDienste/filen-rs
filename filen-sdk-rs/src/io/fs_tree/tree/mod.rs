use std::{
	borrow::Cow,
	collections::{HashMap, hash_map},
	path::{Path, PathBuf},
	sync::atomic::AtomicBool,
};

use string_interner::{DefaultBackend, StringInterner};

use crate::{Error, ErrorKind, consts::CALLBACK_INTERVAL};

use super::{WalkError, entry::*};

pub(super) mod local;
pub(super) mod remote;
// #[cfg(test)]
// mod test;

pub(crate) struct FSTree<DirExtra, FileExtra> {
	interner: StringInterner<DefaultBackend>,
	entries: Vec<Entry<DirExtra, FileExtra>>,
	root_num_children: u32,
}

impl<DirExtra, FileExtra> FSTree<DirExtra, FileExtra> {
	pub(crate) fn count_entries(&self) -> usize {
		self.entries.len()
	}

	pub(crate) fn list_children(&self, info: DirChildrenInfo) -> &[Entry<DirExtra, FileExtra>] {
		&self.entries[info.as_range()]
	}

	pub(crate) fn root_children(&self) -> DirChildrenInfo {
		DirChildrenInfo::new(
			self.entries.len() as u32 - self.root_num_children,
			self.root_num_children,
		)
	}

	pub(crate) fn get_name(&self, entry: &impl EntryName) -> &str {
		self.interner
			.resolve(entry.name())
			.expect("should resolve name")
	}

	pub(crate) fn dfs_iter_with_path<'a>(
		&'a self,
		root: &'a Path,
	) -> FSTreeDFSIteratorWithPath<'a, DirExtra, FileExtra> {
		FSTreeDFSIteratorWithPath {
			stack: vec![LevelState::from_dir_children_info(self.root_children())],
			tree: self,
			root,
		}
	}

	pub(crate) fn dfs_iter<'a>(&'a self) -> FSTreeDFSIterator<'a, DirExtra, FileExtra> {
		FSTreeDFSIterator {
			stack: vec![LevelState::from_dir_children_info(self.root_children())],
			tree: self,
		}
	}
}

struct LevelState {
	range: std::ops::Range<usize>,
}

impl LevelState {
	fn from_dir_children_info(info: DirChildrenInfo) -> Self {
		Self {
			range: info.as_range(),
		}
	}

	fn next_index(&mut self) -> Option<usize> {
		if self.range.start < self.range.end {
			let index = self.range.start;
			self.range.start += 1;
			Some(index)
		} else {
			None
		}
	}
}

pub(crate) struct FSTreeDFSIteratorWithPath<'a, DirExtra, FileExtra> {
	stack: Vec<LevelState>,
	tree: &'a FSTree<DirExtra, FileExtra>,
	root: &'a Path,
}

impl<DirExtra, FileExtra> FSTreeDFSIteratorWithPath<'_, DirExtra, FileExtra> {
	fn build_path(&self, root: &Path, current_name: &str) -> PathBuf {
		std::iter::once(root)
			.chain(
				self.stack
					.iter()
					.take(self.stack.len().saturating_sub(1))
					.map(|level| {
						let entry = &self.tree.entries[level.range.start - 1];
						self.tree.get_name(entry).as_ref()
					}),
			)
			.chain(std::iter::once(current_name.as_ref()))
			.collect()
	}
}

impl<'a, DirExtra, FileExtra> Iterator for FSTreeDFSIteratorWithPath<'a, DirExtra, FileExtra> {
	type Item = (&'a Entry<DirExtra, FileExtra>, PathBuf); // entry and path

	fn next(&mut self) -> Option<Self::Item> {
		let Some(next_index) = self.stack.last_mut()?.next_index() else {
			self.stack.pop();
			// switch to become in the future (tail call)
			return self.next();
		};

		let entry = &self.tree.entries[next_index];
		let path = self.build_path(self.root, self.tree.get_name(entry));

		if let Entry::Dir(dir_entry) = entry {
			let children_info = dir_entry.children_info();
			self.stack
				.push(LevelState::from_dir_children_info(children_info));
		}

		Some((entry, path))
	}
}

pub(crate) struct FSTreeDFSIterator<'a, DirExtra, FileExtra> {
	stack: Vec<LevelState>,
	tree: &'a FSTree<DirExtra, FileExtra>,
}

impl<'a, DirExtra, FileExtra> Iterator for FSTreeDFSIterator<'a, DirExtra, FileExtra> {
	type Item = (&'a Entry<DirExtra, FileExtra>, usize); // entry and depth

	fn next(&mut self) -> Option<Self::Item> {
		let Some(next_index) = self.stack.last_mut()?.next_index() else {
			self.stack.pop();
			// switch to become in the future (tail call)
			return self.next();
		};

		let entry = &self.tree.entries[next_index];
		let depth = self.stack.len() - 1;

		if let Entry::Dir(dir_entry) = entry {
			let children_info = dir_entry.children_info();
			self.stack
				.push(LevelState::from_dir_children_info(children_info));
		}

		Some((entry, depth))
	}
}

#[allow(type_alias_bounds)]
type FSTreeForDFSE<DFSE>
where
	DFSE: DFSWalkerEntry,
= FSTree<
	<DFSE::WalkerDirEntry as DFSWalkerDirEntry>::Extra,
	<DFSE::WalkerFileEntry as DFSWalkerFileEntry>::Extra,
>;

// should not receive the root entry
fn build_fs_tree<DFSE>(
	dfs_iterator: impl Iterator<Item = Result<DFSE, WalkError>>,
	error_callback: &mut impl FnMut(Vec<Error>),
	progress_callback: &mut impl FnMut(u64, u64, u64),
	should_cancel: &AtomicBool,
) -> Result<(FSTreeForDFSE<DFSE>, FSStats), Error>
where
	DFSE: DFSWalkerEntry,
	<<DFSE as DFSWalkerEntry>::WalkerDirEntry as DFSWalkerDirEntry>::Extra: std::fmt::Debug,
	<<DFSE as DFSWalkerEntry>::WalkerFileEntry as DFSWalkerFileEntry>::Extra: std::fmt::Debug,
{
	let mut builder = FSTreeBuilder::new();

	for entry_result in dfs_iterator {
		builder.process_entry(entry_result)?;

		if builder.should_invoke_callbacks() {
			let (dirs, files, bytes) = builder.stats_snapshot();
			progress_callback(dirs, files, bytes);

			let errors = builder.take_errors();
			if !errors.is_empty() {
				error_callback(errors);
			}

			if should_cancel.load(std::sync::atomic::Ordering::Relaxed) {
				return Err(Error::custom(ErrorKind::Cancelled, "cancelled"));
			}
		}
	}

	let (fs_tree, stats, errors) = builder.finalize::<DFSE::CompareStrategy>()?;

	if !errors.is_empty() {
		error_callback(errors);
	}

	Ok((fs_tree, stats))
}

type AncestorStackLevel<DirExtra, FileExtra> = (
	UnfinalizedDirEntry<DirExtra>,
	Vec<Entry<DirExtra, FileExtra>>,
);

struct AncestorStack<DirExtra, FileExtra> {
	entries: Vec<AncestorStackLevel<DirExtra, FileExtra>>,
	root_children: Vec<Entry<DirExtra, FileExtra>>,
}

impl<DirExtra, FileExtra> AncestorStack<DirExtra, FileExtra> {
	pub fn new() -> Self {
		Self {
			entries: Vec::new(),
			root_children: Vec::new(),
		}
	}

	fn append_to_top_level(&mut self, entry: Entry<DirExtra, FileExtra>) {
		if let Some(top) = self.entries.last_mut() {
			top.1.push(entry);
		} else {
			self.root_children.push(entry);
		}
	}

	fn push_to_stack(&mut self, dir: UnfinalizedDirEntry<DirExtra>) {
		self.entries.push((dir, Vec::new()));
	}

	fn pop(
		&mut self,
	) -> (
		Option<UnfinalizedDirEntry<DirExtra>>,
		Vec<Entry<DirExtra, FileExtra>>,
	) {
		match self.entries.pop() {
			Some((dir, children)) => (Some(dir), children),
			None => (None, std::mem::take(&mut self.root_children)),
		}
	}

	fn len(&self) -> usize {
		// how does this handle empty folder?
		self.entries.len() + 1
	}
}

pub(super) struct FSTreeBuilder<DirExtra, FileExtra> {
	interner: StringInterner<DefaultBackend>,
	ancestor_stack: AncestorStack<DirExtra, FileExtra>,
	final_entries: Vec<Entry<DirExtra, FileExtra>>,
	errors: Vec<Error>,
	stats: FSStats,
	last_callback: std::time::Instant,
}

impl<DirExtra, FileExtra> FSTreeBuilder<DirExtra, FileExtra>
where
	DirExtra: Clone + std::fmt::Debug + 'static,
	FileExtra: Clone + std::fmt::Debug + 'static,
{
	pub(super) fn new() -> Self {
		Self {
			interner: StringInterner::default(),
			ancestor_stack: AncestorStack::new(),
			final_entries: Vec::new(),
			errors: Vec::new(),
			stats: FSStats::new(),
			last_callback: std::time::Instant::now(),
		}
	}

	pub(super) fn process_entry<DFSE>(
		&mut self,
		entry_result: Result<DFSE, WalkError>,
	) -> Result<bool, Error>
	where
		DFSE: DFSWalkerEntry,
		<DFSE::WalkerDirEntry as DFSWalkerDirEntry>::Extra: Into<DirExtra>,
		<DFSE::WalkerFileEntry as DFSWalkerFileEntry>::Extra: Into<FileExtra>,
		DFSE::CompareStrategy: CompareStrategy<DirExtra, FileExtra>,
	{
		let entry = match entry_result {
			Ok(e) => e,
			Err(err) => {
				self.errors.push(err.into());
				return Ok(false);
			}
		};

		let (res, errors) = self.adjust_stack_until_depth::<DFSE::CompareStrategy>(entry.depth());
		self.errors.extend(errors);
		res?;

		let name = match entry.name() {
			Ok(n) => n,
			Err(err) => {
				self.errors.push(err.into());
				return Ok(false);
			}
		};

		let name_symbol = self.interner.get_or_intern(name);

		match entry.into_entry_type() {
			EntryType::File(file) => {
				let size = match file.size() {
					Ok(s) => s,
					Err(e) => {
						self.errors.push(e.into());
						return Ok(false);
					}
				};
				self.stats.add_file(size);

				self.ancestor_stack
					.append_to_top_level(Entry::File(FileEntry::new(
						name_symbol,
						file.into_extra_data().into(),
						size,
					)));
			}
			EntryType::Dir(dir) => {
				self.stats.add_dir();

				self.ancestor_stack.push_to_stack(UnfinalizedDirEntry::new(
					name_symbol,
					dir.into_extra_data().into(),
				));
			}
		}

		// Check if it's time for callbacks
		Ok(std::time::Instant::now().duration_since(self.last_callback) >= CALLBACK_INTERVAL)
	}

	pub(super) fn should_invoke_callbacks(&mut self) -> bool {
		if std::time::Instant::now().duration_since(self.last_callback) >= CALLBACK_INTERVAL {
			self.last_callback = std::time::Instant::now();
			true
		} else {
			false
		}
	}

	pub(super) fn take_errors(&mut self) -> Vec<Error> {
		std::mem::take(&mut self.errors)
	}

	pub(super) fn stats_snapshot(&self) -> (u64, u64, u64) {
		self.stats.snapshot()
	}

	#[allow(clippy::type_complexity)]
	pub(super) fn finalize<C>(
		mut self,
	) -> Result<(super::FSTree<DirExtra, FileExtra>, FSStats, Vec<Error>), Error>
	where
		C: CompareStrategy<DirExtra, FileExtra>,
	{
		let (res, errors) = self.adjust_stack_until_depth::<C>(0);
		let dir_children = res?.unwrap();
		self.errors.extend(errors);

		let first_tree = super::FSTree {
			interner: self.interner,
			entries: self.final_entries,
			root_num_children: dir_children.into_num_children(),
		};

		let iter = first_tree.dfs_iter().map(|(entry, depth)| match entry {
			Entry::Dir(dir) => SecondPassEntry::dir(
				SecondPassDirEntry::new(first_tree.get_name(dir), dir.extra_data().clone()),
				depth + 1,
			),
			Entry::File(file_entry) => SecondPassEntry::file(
				SecondPassFileEntry::new(
					first_tree.get_name(file_entry),
					file_entry.extra_data().clone(),
					file_entry.size(),
				),
				depth + 1,
			),
		});

		let mut second_pass = Self::new();

		for entry in iter {
			second_pass
				.process_entry::<SecondPassEntry<DirExtra, FileExtra>>(Ok(entry))
				.unwrap();
		}

		let (res, _) =
			second_pass.adjust_stack_until_depth::<PanicCompareStrategy<DirExtra, FileExtra>>(0);
		let dir_children = res.unwrap().unwrap();

		second_pass.interner.shrink_to_fit();
		second_pass.final_entries.shrink_to_fit();

		let stats = second_pass.stats;

		let final_tree = super::FSTree {
			interner: second_pass.interner,
			entries: second_pass.final_entries,
			root_num_children: dir_children.into_num_children(),
		};

		Ok((final_tree, stats, std::mem::take(&mut self.errors)))
	}

	fn adjust_stack_until_depth<C>(
		&mut self,
		target_depth: usize,
	) -> (Result<Option<DirChildrenInfo>, Error>, Vec<Error>)
	where
		C: CompareStrategy<DirExtra, FileExtra>,
	{
		let mut errors = Vec::new();
		if self.ancestor_stack.len() < target_depth {
			return (
				Err(Error::custom(
					ErrorKind::Internal,
					"cannot adjust ancestor stack to a deeper depth than current",
				)),
				errors,
			);
		}

		while self.ancestor_stack.len() > target_depth {
			// finalize the current ancestor level
			let (completed_parent, completed_children) = self.ancestor_stack.pop();

			let (mut filtered_children, child_errors) = filter_children::<DirExtra, FileExtra, C>(
				&self.ancestor_stack,
				completed_parent
					.as_ref()
					.and_then(|p| self.interner.resolve(p.name())),
				completed_children,
				&self.interner,
			);
			errors.extend(child_errors);

			let children_idx = self.final_entries.len() as u32;
			let children_count = filtered_children.len() as u32;

			let children_info = DirChildrenInfo::new(children_idx, children_count);
			self.final_entries.append(&mut filtered_children);

			if let Some(completed_parent) = completed_parent {
				let completed_parent =
					Entry::Dir(DirEntry::from_unfinalized(completed_parent, children_info));

				self.ancestor_stack.append_to_top_level(completed_parent);
			} else {
				return (Ok(Some(children_info)), errors);
			}
		}

		(Ok(None), errors)
	}
}

fn filter_children<DirExtra, FileExtra, C: CompareStrategy<DirExtra, FileExtra>>(
	ancestor_stack: &AncestorStack<DirExtra, FileExtra>,
	parent_name: Option<&str>,
	mut children: Vec<Entry<DirExtra, FileExtra>>,
	interner: &StringInterner<DefaultBackend>,
) -> (Vec<Entry<DirExtra, FileExtra>>, Vec<Error>)
where
	DirExtra: std::fmt::Debug,
	FileExtra: std::fmt::Debug,
{
	let mut by_name: HashMap<_, (_, Vec<_>)> = HashMap::new();

	for child in children.drain(..) {
		let name = match &child {
			Entry::Dir(dir_entry) => dir_entry.name(),
			Entry::File(file_entry) => file_entry.name(),
		};

		let name = interner.resolve(name).unwrap();
		let name = if name.chars().any(|c| c.is_uppercase()) {
			Cow::Owned(name.trim().to_lowercase())
		} else {
			Cow::Borrowed(name.trim())
		};

		match by_name.entry(name) {
			hash_map::Entry::Occupied(mut o) => {
				o.get_mut().1.push(child);
			}
			hash_map::Entry::Vacant(v) => {
				v.insert((child, Vec::new()));
			}
		}
	}

	let mut errors = Vec::new();

	for (name, (first, extra)) in by_name.into_iter() {
		if !extra.is_empty() {
			let duplicate_path = ancestor_stack
				.entries
				.iter()
				.map(|(entry, _)| interner.resolve(entry.name()).unwrap())
				.chain(parent_name)
				.chain(std::iter::once(name.as_ref()))
				// should be replaced by an intersperse + collect after
				// https://doc.rust-lang.org/std/iter/trait.Iterator.html#method.intersperse
				// is stabilized
				.collect::<Vec<_>>()
				.join("/");

			errors.push(WalkError::DuplicateName(duplicate_path).into());
		}

		let preferred = extra.into_iter().fold(first, |max, candidate| {
			if C::should_replace(&max, &candidate) {
				candidate
			} else {
				max
			}
		});
		children.push(preferred);
	}

	(children, errors)
}

#[derive(Debug, Clone)]
pub(crate) struct FSStats {
	dirs: u64,
	files: u64,
	bytes: u64,
}

impl FSStats {
	pub(super) fn new() -> Self {
		Self {
			dirs: 0,
			files: 0,
			bytes: 0,
		}
	}

	pub(super) fn add_file(&mut self, size: u64) {
		self.files += 1;
		self.bytes += size;
	}

	pub(super) fn add_dir(&mut self) {
		self.dirs += 1;
	}

	pub(crate) fn snapshot(&self) -> (u64, u64, u64) {
		(self.dirs, self.files, self.bytes)
	}
}
