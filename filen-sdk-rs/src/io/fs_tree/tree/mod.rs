use std::sync::atomic::AtomicBool;

use futures::Stream;
use string_interner::{DefaultBackend, StringInterner};

use crate::{Error, ErrorKind, consts::CALLBACK_INTERVAL};

use super::{
	WalkError,
	entry::{
		DFSWalkerDirEntry, DFSWalkerEntry, DFSWalkerFileEntry, DirChildrenInfo, DirEntry, Entry,
		EntryName, EntryType, FileEntry, UnfinalizedDirEntry,
	},
};

pub(super) mod local;
pub(super) mod remote;

pub(crate) struct FSTree<DirExtra, FileExtra> {
	interner: StringInterner<DefaultBackend>,
	entries: Vec<Entry<DirExtra, FileExtra>>,
}

impl<DirExtra, FileExtra> FSTree<DirExtra, FileExtra> {
	pub(crate) fn count_entries(&self) -> usize {
		self.entries.len()
	}

	pub(crate) fn list_children(&self, info: DirChildrenInfo) -> &[Entry<DirExtra, FileExtra>] {
		&self.entries[info.as_range()]
	}

	pub(crate) fn root(&self) -> &Entry<DirExtra, FileExtra> {
		&self.entries[self.entries.len() - 1]
	}

	pub(crate) fn get_name(&self, entry: &impl EntryName) -> &str {
		self.interner
			.resolve(entry.name())
			.expect("should resolve name")
	}
}

#[allow(type_alias_bounds)]
type FSTreeForDFSE<DFSE>
where
	DFSE: DFSWalkerEntry,
= FSTree<
	<DFSE::WalkerDirEntry<'static> as DFSWalkerDirEntry>::Extra,
	<DFSE::WalkerFileEntry<'static> as DFSWalkerFileEntry>::Extra,
>;

fn build_fs_tree<DFSE>(
	dfs_iterator: impl Iterator<Item = Result<DFSE, WalkError>>,
	error_callback: &mut impl FnMut(Vec<WalkError>),
	progress_callback: &mut impl FnMut(u64, u64, u64),
	should_cancel: &AtomicBool,
) -> Result<(FSTreeForDFSE<DFSE>, FSStats), Error>
where
	DFSE: DFSWalkerEntry,
	for<'a> <DFSE::WalkerDirEntry<'a> as DFSWalkerDirEntry>::Extra:
		Into<<DFSE::WalkerDirEntry<'static> as DFSWalkerDirEntry>::Extra>,
	for<'a> <DFSE::WalkerFileEntry<'a> as DFSWalkerFileEntry>::Extra:
		Into<<DFSE::WalkerFileEntry<'static> as DFSWalkerFileEntry>::Extra>,
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

	let errors = builder.take_errors();
	if !errors.is_empty() {
		error_callback(errors);
	}

	builder.finalize()
}

async fn build_fs_tree_async<DFSE>(
	mut dfs_stream: impl Stream<Item = Result<DFSE, WalkError>> + Unpin,
	error_callback: &mut impl FnMut(Vec<WalkError>),
	progress_callback: &mut impl FnMut(u64, u64, u64),
) -> Result<
	(
		FSTree<
			<DFSE::WalkerDirEntry<'static> as DFSWalkerDirEntry>::Extra,
			<DFSE::WalkerFileEntry<'static> as DFSWalkerFileEntry>::Extra,
		>,
		FSStats,
	),
	Error,
>
where
	DFSE: DFSWalkerEntry + 'static,
	for<'a> <DFSE::WalkerDirEntry<'a> as DFSWalkerDirEntry>::Extra:
		Into<<DFSE::WalkerDirEntry<'static> as DFSWalkerDirEntry>::Extra>,
	for<'a> <DFSE::WalkerFileEntry<'a> as DFSWalkerFileEntry>::Extra:
		Into<<DFSE::WalkerFileEntry<'static> as DFSWalkerFileEntry>::Extra>,
{
	use futures::StreamExt;

	let mut builder = FSTreeBuilder::new();

	while let Some(entry_result) = dfs_stream.next().await {
		builder.process_entry(entry_result)?;

		if builder.should_invoke_callbacks() {
			let (dirs, files, bytes) = builder.stats_snapshot();
			progress_callback(dirs, files, bytes);

			let errors = builder.take_errors();
			if !errors.is_empty() {
				error_callback(errors);
			}
		}
	}

	let errors = builder.take_errors();
	if !errors.is_empty() {
		error_callback(errors);
	}

	builder.finalize()
}

type AncestorStack<DirExtra, FileExtra> = Vec<(
	UnfinalizedDirEntry<DirExtra>,
	Vec<Entry<DirExtra, FileExtra>>,
)>;

pub(super) struct FSTreeBuilder<DirExtra, FileExtra> {
	interner: StringInterner<DefaultBackend>,
	ancestor_stack: AncestorStack<DirExtra, FileExtra>,
	final_entries: Vec<Entry<DirExtra, FileExtra>>,
	errors: Vec<WalkError>,
	stats: FSStats,
	last_callback: std::time::Instant,
}

impl<DirExtra, FileExtra> FSTreeBuilder<DirExtra, FileExtra> {
	pub(super) fn new() -> Self {
		Self {
			interner: StringInterner::default(),
			ancestor_stack: Vec::new(),
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
		for<'a> <DFSE::WalkerDirEntry<'a> as DFSWalkerDirEntry>::Extra: Into<DirExtra>,
		for<'a> <DFSE::WalkerFileEntry<'a> as DFSWalkerFileEntry>::Extra: Into<FileExtra>,
	{
		let entry = match entry_result {
			Ok(e) => e,
			Err(err) => {
				self.errors.push(err);
				return Ok(false);
			}
		};

		adjust_stack_until_depth(
			&mut self.ancestor_stack,
			entry.depth(),
			&mut self.final_entries,
		)?;

		let name = match entry.name() {
			Ok(n) => n,
			Err(err) => {
				self.errors.push(err);
				return Ok(false);
			}
		};

		let name_symbol = self.interner.get_or_intern(name);

		match entry.entry_type() {
			EntryType::File(file) => {
				let size = match file.size() {
					Ok(s) => s,
					Err(e) => {
						self.errors.push(e);
						return Ok(false);
					}
				};
				self.stats.add_file(size);

				let extra_data = match file.into_extra_data() {
					Ok(data) => data,
					Err(e) => {
						self.errors.push(e);
						return Ok(false);
					}
				};

				self.ancestor_stack
					.last_mut()
					.expect("should have a parent directory for file entries")
					.1
					.push(Entry::File(FileEntry::new(name_symbol, extra_data.into())));
			}
			EntryType::Dir(dir) => {
				self.stats.add_dir();
				let extra_data = match dir.into_extra_data() {
					Ok(data) => data,
					Err(e) => {
						self.errors.push(e);
						return Ok(false);
					}
				};

				self.ancestor_stack.push((
					UnfinalizedDirEntry::new(name_symbol, extra_data.into()),
					Vec::new(),
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

	pub(super) fn take_errors(&mut self) -> Vec<WalkError> {
		std::mem::take(&mut self.errors)
	}

	pub(super) fn stats_snapshot(&self) -> (u64, u64, u64) {
		self.stats.snapshot()
	}

	pub(super) fn finalize(
		mut self,
	) -> Result<(super::FSTree<DirExtra, FileExtra>, FSStats), Error> {
		adjust_stack_until_depth(&mut self.ancestor_stack, 0, &mut self.final_entries)?;

		self.interner.shrink_to_fit();
		self.final_entries.shrink_to_fit();

		Ok((
			super::FSTree {
				interner: self.interner,
				entries: self.final_entries,
			},
			self.stats,
		))
	}
}

fn adjust_stack_until_depth<DirExtra, FileExtra>(
	stack: &mut AncestorStack<DirExtra, FileExtra>,
	target_depth: usize,
	final_entries: &mut Vec<Entry<DirExtra, FileExtra>>,
) -> Result<(), Error> {
	if stack.len() < target_depth {
		return Err(Error::custom(
			ErrorKind::Internal,
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
		let completed_parent = Entry::Dir(DirEntry::from_unfinalized(
			completed_parent,
			DirChildrenInfo::new(children_idx, children_count),
		));

		if let Some((_, parent_children)) = stack.last_mut() {
			parent_children.push(completed_parent);
		} else {
			final_entries.push(completed_parent);
		}
	}

	Ok(())
}

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
