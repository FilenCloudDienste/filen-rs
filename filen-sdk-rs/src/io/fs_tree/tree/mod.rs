use std::{
	borrow::Cow,
	collections::{HashMap, hash_map},
	sync::atomic::AtomicBool,
};

use string_interner::{DefaultBackend, StringInterner};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::time::Instant;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::std::Instant;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use crate::io::CanonicalPath;
use crate::{Error, ErrorKind, consts::CALLBACK_INTERVAL};

use super::{WalkError, entry::*};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(super) mod local;
pub(super) mod remote;

pub(crate) struct FSTree<DirExtra, FileExtra> {
	interner: StringInterner<DefaultBackend>,
	entries: Vec<Entry<DirExtra, FileExtra>>,
	root_num_children: u32,
}

impl<DirExtra, FileExtra> FSTree<DirExtra, FileExtra> {
	// Only called from the native-only `io::dir_upload` path.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
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
		root: &'a str,
	) -> FSTreeDFSIteratorWithPath<'a, DirExtra, FileExtra> {
		FSTreeDFSIteratorWithPath::new(self.root_children(), self, root)
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
	root: &'a str,
}

impl<'a, DirExtra, FileExtra> FSTreeDFSIteratorWithPath<'a, DirExtra, FileExtra> {
	fn new(info: DirChildrenInfo, tree: &'a FSTree<DirExtra, FileExtra>, root: &'a str) -> Self {
		Self {
			stack: vec![LevelState::from_dir_children_info(info)],
			tree,
			root,
		}
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub(crate) fn canonicalize(
		self,
	) -> Result<FSTreeDFSIteratorWithPathCanonicalized<'a, DirExtra, FileExtra>, std::io::Error> {
		FSTreeDFSIteratorWithPathCanonicalized::new(self)
	}
}

impl<DirExtra, FileExtra> FSTreeDFSIteratorWithPath<'_, DirExtra, FileExtra> {
	fn descendants<'a>(&'a self, current_name: &'a str) -> impl Iterator<Item = &'a str> + Clone {
		self.stack
			.iter()
			.take(self.stack.len().saturating_sub(1))
			.map(|level| {
				let entry = &self.tree.entries[level.range.start - 1];
				self.tree.get_name(entry)
			})
			.chain(std::iter::once(current_name))
	}

	fn build_path_str(&self, root: &str, current_name: &str) -> String {
		std::iter::once(root)
			.skip_while(|s| s.is_empty())
			.chain(self.descendants(current_name))
			.intersperse(std::path::MAIN_SEPARATOR_STR)
			.collect()
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn build_path_canonical(
		&self,
		root: &CanonicalPath,
		current_name: &str,
	) -> Result<CanonicalPath, std::io::Error> {
		root.create_descendant_path(self.descendants(current_name))
	}
}

impl<'a, DirExtra, FileExtra> FSTreeDFSIteratorWithPath<'a, DirExtra, FileExtra> {
	fn inner_next<Ref: ?Sized, Own>(
		&mut self,
		root: &Ref,
		path_builder: &impl Fn(&Self, &Ref, &str) -> Own,
	) -> Option<(&'a Entry<DirExtra, FileExtra>, Own)> {
		// iterative rather than recursive so that stack usage stays constant
		// regardless of tree depth
		let next_index = loop {
			match self.stack.last_mut()?.next_index() {
				Some(index) => break index,
				None => {
					self.stack.pop();
				}
			}
		};

		let entry = &self.tree.entries[next_index];
		let path = path_builder(self, root, self.tree.get_name(entry));

		if let Entry::Dir(dir_entry) = entry {
			let children_info = dir_entry.children_info();
			self.stack
				.push(LevelState::from_dir_children_info(children_info));
		}

		Some((entry, path))
	}
}

impl<'a, DirExtra, FileExtra> Iterator for FSTreeDFSIteratorWithPath<'a, DirExtra, FileExtra> {
	type Item = (&'a Entry<DirExtra, FileExtra>, String); // entry and path

	fn next(&mut self) -> Option<Self::Item> {
		self.inner_next(self.root, &Self::build_path_str)
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) struct FSTreeDFSIteratorWithPathCanonicalized<'a, DirExtra, FileExtra> {
	inner: FSTreeDFSIteratorWithPath<'a, DirExtra, FileExtra>,
	root: CanonicalPath,
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
impl<'a, DirExtra, FileExtra> FSTreeDFSIteratorWithPathCanonicalized<'a, DirExtra, FileExtra> {
	fn new(
		inner: FSTreeDFSIteratorWithPath<'a, DirExtra, FileExtra>,
	) -> Result<Self, std::io::Error> {
		Ok(Self {
			root: CanonicalPath::new(inner.root.as_ref())?,
			inner,
		})
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
impl<'a, DirExtra, FileExtra> Iterator
	for FSTreeDFSIteratorWithPathCanonicalized<'a, DirExtra, FileExtra>
{
	// The canonical path is fallible per entry: a server-supplied name that is
	// not a safe single component (`..`, absolute, separator-bearing) is
	// rejected here so it can never escape the download root.
	type Item = (
		&'a Entry<DirExtra, FileExtra>,
		Result<CanonicalPath, std::io::Error>,
	);

	fn next(&mut self) -> Option<Self::Item> {
		self.inner.inner_next(
			&self.root,
			&FSTreeDFSIteratorWithPath::<DirExtra, FileExtra>::build_path_canonical,
		)
	}
}

pub(crate) struct FSTreeDFSIterator<'a, DirExtra, FileExtra> {
	stack: Vec<LevelState>,
	tree: &'a FSTree<DirExtra, FileExtra>,
}

impl<'a, DirExtra, FileExtra> Iterator for FSTreeDFSIterator<'a, DirExtra, FileExtra> {
	type Item = (&'a Entry<DirExtra, FileExtra>, usize); // entry and depth

	fn next(&mut self) -> Option<Self::Item> {
		// iterative rather than recursive so that stack usage stays constant
		// regardless of tree depth
		let next_index = loop {
			match self.stack.last_mut()?.next_index() {
				Some(index) => break index,
				None => {
					self.stack.pop();
				}
			}
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
	last_callback: Instant,
	// Depth of the most recently rejected entry, if any. A walker may still
	// descend into a rejected directory (e.g. one with a non-UTF-8 name), so
	// while this is set, deeper entries are descendants of the rejected entry
	// and must be dropped to keep the ancestor stack aligned with walker
	// depths.
	rejected_entry_depth: Option<usize>,
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
			last_callback: Instant::now(),
			rejected_entry_depth: None,
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

		let depth = entry.depth();

		if let Some(rejected_depth) = self.rejected_entry_depth {
			if depth > rejected_depth {
				// DFS ordering means this is a descendant of a rejected
				// entry. Drop it silently: the rejection already recorded an
				// error for the subtree root, and adjusting the ancestor
				// stack to this depth would fail.
				return Ok(false);
			}
			self.rejected_entry_depth = None;
		}

		let (res, errors) = self.adjust_stack_until_depth::<DFSE::CompareStrategy>(depth);
		self.errors.extend(errors);
		res?;

		let name = match entry.name() {
			Ok(n) => n,
			Err(err) => {
				self.errors.push(err.into());
				self.rejected_entry_depth = Some(depth);
				return Ok(false);
			}
		};

		// interning must happen before into_entry_type because `name` borrows
		// `entry`, which into_entry_type consumes; a rejected entry therefore
		// leaves its name in the interner (harmless)
		let name_symbol = self.interner.get_or_intern(name);

		let entry_type = match entry.into_entry_type() {
			Ok(entry_type) => entry_type,
			Err(err) => {
				self.errors.push(err.into());
				// Setting the marker here is a no-op safety net, not a real
				// requirement: the only rejection `into_entry_type` can
				// produce is for special files (sockets, fifos, ...), and
				// those are always walkdir leaves, never internal nodes.
				// Walkdir decides whether to descend from the same cached
				// `file_type().is_dir()` this match also reads, so a
				// non-dir/non-file entry can have no children to desync
				// against.
				self.rejected_entry_depth = Some(depth);
				return Ok(false);
			}
		};

		match entry_type {
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
		Ok(Instant::now().duration_since(self.last_callback) >= CALLBACK_INTERVAL)
	}

	pub(super) fn should_invoke_callbacks(&mut self) -> bool {
		if Instant::now().duration_since(self.last_callback) >= CALLBACK_INTERVAL {
			self.last_callback = Instant::now();
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

#[cfg(test)]
mod tests {
	use uuid::Uuid;

	use super::*;

	const DEPTH: usize = 8_000;

	fn deep_chain_entries()
	-> impl Iterator<Item = Result<SecondPassEntry<'static, (), ()>, WalkError>> {
		(1..=DEPTH)
			.map(|depth| {
				Ok(SecondPassEntry::dir(
					SecondPassDirEntry::new("d", ()),
					depth,
				))
			})
			.chain(std::iter::once(Ok(SecondPassEntry::file(
				SecondPassFileEntry::new("f", (), 1),
				DEPTH + 1,
			))))
	}

	struct RejectableEntry {
		name: Result<&'static str, Uuid>,
		depth: usize,
		dir: bool,
	}

	impl DFSWalkerEntry for RejectableEntry {
		type WalkerDirEntry = SecondPassDirEntry<'static, ()>;
		type WalkerFileEntry = SecondPassFileEntry<'static, ()>;
		type CompareStrategy = PanicCompareStrategy<(), ()>;

		fn depth(&self) -> usize {
			self.depth
		}

		fn name(&self) -> Result<&str, WalkError> {
			self.name.map_err(WalkError::EncryptedMeta)
		}

		fn into_entry_type(
			self,
		) -> Result<EntryType<Self::WalkerDirEntry, Self::WalkerFileEntry>, WalkError> {
			let name = self.name.unwrap_or("");
			if self.dir {
				Ok(EntryType::Dir(SecondPassDirEntry::new(name, ())))
			} else {
				Ok(EntryType::File(SecondPassFileEntry::new(name, (), 1)))
			}
		}
	}

	#[test]
	fn rejected_dir_subtree_is_skipped_without_desyncing_the_walk() {
		let bad_uuid = Uuid::from_u128(1);
		let entries = vec![
			Ok(RejectableEntry {
				name: Err(bad_uuid),
				depth: 1,
				dir: true,
			}),
			// walkers still descend into a directory whose entry was rejected
			Ok(RejectableEntry {
				name: Ok("child"),
				depth: 2,
				dir: false,
			}),
			Ok(RejectableEntry {
				name: Ok("sibling"),
				depth: 1,
				dir: false,
			}),
		];

		let mut errors = Vec::new();
		let (tree, stats) = build_fs_tree(
			entries.into_iter(),
			&mut |errs| errors.extend(errs),
			&mut |_, _, _| {},
			&AtomicBool::new(false),
		)
		.expect("walk should continue past a rejected directory");

		assert_eq!(stats.snapshot(), (0, 1, 1));
		let names = tree
			.dfs_iter()
			.map(|(entry, _)| tree.get_name(entry).to_owned())
			.collect::<Vec<_>>();
		assert_eq!(names, ["sibling"]);

		assert_eq!(errors.len(), 1);
		assert!(matches!(
			errors[0].downcast_ref::<WalkError>(),
			Some(WalkError::EncryptedMeta(uuid)) if uuid == &bad_uuid
		));
	}

	#[test]
	fn deep_tree_build_and_traversal_use_constant_stack() {
		std::thread::Builder::new()
			.stack_size(256 * 1024)
			.spawn(|| {
				let (tree, stats) = build_fs_tree(
					deep_chain_entries(),
					&mut |_| {},
					&mut |_, _, _| {},
					&AtomicBool::new(false),
				)
				.expect("deep tree should build");

				assert_eq!(stats.snapshot(), (DEPTH as u64, 1, 1));
				assert_eq!(tree.dfs_iter().count(), DEPTH + 1);
				assert_eq!(tree.dfs_iter_with_path("root").count(), DEPTH + 1);
			})
			.expect("failed to spawn test thread")
			.join()
			.expect("deep tree traversal should not overflow the stack");
	}
}
