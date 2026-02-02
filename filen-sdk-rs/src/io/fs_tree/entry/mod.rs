#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(super) mod local;
pub(super) mod remote;

use string_interner::DefaultSymbol;

use super::WalkError;

pub(super) trait DFSWalkerEntry {
	type WalkerDirEntry: DFSWalkerDirEntry;
	type WalkerFileEntry: DFSWalkerFileEntry;
	type CompareStrategy: CompareStrategy<
			<Self::WalkerDirEntry as DFSWalkerDirEntry>::Extra,
			<Self::WalkerFileEntry as DFSWalkerFileEntry>::Extra,
		>;
	fn depth(&self) -> usize;
	fn name(&self) -> Result<&str, WalkError>;
	fn into_entry_type(self) -> EntryType<Self::WalkerDirEntry, Self::WalkerFileEntry>;
}

pub(super) trait DFSWalkerFileEntry {
	type Extra: Clone + 'static;
	fn into_extra_data(self) -> Self::Extra;
	fn size(&self) -> Result<u64, WalkError>;
}

pub(super) trait DFSWalkerDirEntry {
	type Extra: Clone + 'static;
	fn into_extra_data(self) -> Self::Extra;
}

pub(super) enum EntryType<D, F> {
	Dir(D),
	File(F),
}

pub(crate) trait EntryName {
	fn name(&self) -> DefaultSymbol;
}

#[derive(Debug)]
pub(crate) struct FileEntry<Extra> {
	name: DefaultSymbol,
	extra_data: Extra,
	size: u64,
}

impl<Extra> FileEntry<Extra> {
	pub(crate) fn new(name: DefaultSymbol, extra_data: Extra, size: u64) -> Self {
		Self {
			name,
			extra_data,
			size,
		}
	}

	pub(crate) fn extra_data(&self) -> &Extra {
		&self.extra_data
	}

	pub(crate) fn size(&self) -> u64 {
		self.size
	}
}

impl<Extra> EntryName for FileEntry<Extra> {
	fn name(&self) -> DefaultSymbol {
		self.name
	}
}

pub(super) struct UnfinalizedDirEntry<Extra> {
	name: DefaultSymbol,
	extra_data: Extra,
}

impl<Extra> EntryName for UnfinalizedDirEntry<Extra> {
	fn name(&self) -> DefaultSymbol {
		self.name
	}
}

impl<Extra> UnfinalizedDirEntry<Extra> {
	pub(super) fn new(name: DefaultSymbol, extra_data: Extra) -> Self {
		Self { name, extra_data }
	}
}

pub(super) struct SecondPassFileEntry<'a, Extra> {
	name: &'a str,
	extra_data: Extra,
	size: u64,
}

impl<'a, Extra> SecondPassFileEntry<'a, Extra> {
	pub(super) fn new(name: &'a str, extra_data: Extra, size: u64) -> Self {
		Self {
			name,
			extra_data,
			size,
		}
	}
}

pub(super) struct SecondPassDirEntry<'a, DirExtra> {
	name: &'a str,
	extra_data: DirExtra,
}

impl<Extra> DFSWalkerFileEntry for SecondPassFileEntry<'_, Extra>
where
	Extra: Clone + 'static,
{
	type Extra = Extra;

	fn into_extra_data(self) -> Self::Extra {
		self.extra_data
	}

	fn size(&self) -> Result<u64, WalkError> {
		Ok(self.size)
	}
}

impl<'a, DirExtra> SecondPassDirEntry<'a, DirExtra> {
	pub(super) fn new(name: &'a str, extra_data: DirExtra) -> Self {
		Self { name, extra_data }
	}
}

impl<Extra> DFSWalkerDirEntry for SecondPassDirEntry<'_, Extra>
where
	Extra: Clone + 'static,
{
	type Extra = Extra;

	fn into_extra_data(self) -> Self::Extra {
		self.extra_data
	}
}

pub(super) struct SecondPassEntry<'a, DirExtra, FileExtra> {
	inner: SecondPassEntryInner<'a, DirExtra, FileExtra>,
	depth: usize,
}

impl<'a, DirExtra, FileExtra> SecondPassEntry<'a, DirExtra, FileExtra> {
	pub(super) fn file(file: SecondPassFileEntry<'a, FileExtra>, depth: usize) -> Self {
		Self {
			inner: SecondPassEntryInner::File(file),
			depth,
		}
	}

	pub(super) fn dir(dir: SecondPassDirEntry<'a, DirExtra>, depth: usize) -> Self {
		Self {
			inner: SecondPassEntryInner::Dir(dir),
			depth,
		}
	}
}

enum SecondPassEntryInner<'a, DirExtra, FileExtra> {
	Dir(SecondPassDirEntry<'a, DirExtra>),
	File(SecondPassFileEntry<'a, FileExtra>),
}

pub(super) struct PanicCompareStrategy<DirExtra, FileExtra>(
	std::marker::PhantomData<(DirExtra, FileExtra)>,
);

impl<DirExtra, FileExtra> CompareStrategy<DirExtra, FileExtra>
	for PanicCompareStrategy<DirExtra, FileExtra>
{
	fn should_replace(
		_existing: &Entry<DirExtra, FileExtra>,
		_new: &Entry<DirExtra, FileExtra>,
	) -> bool {
		panic!("PanicCompareStrategy should never be used to compare entries");
	}
}

impl<'a, DirExtra, FileExtra> DFSWalkerEntry for SecondPassEntry<'a, DirExtra, FileExtra>
where
	DirExtra: Clone + 'static,
	FileExtra: Clone + 'static,
{
	type WalkerDirEntry = SecondPassDirEntry<'a, DirExtra>;
	type WalkerFileEntry = SecondPassFileEntry<'a, FileExtra>;
	type CompareStrategy = PanicCompareStrategy<DirExtra, FileExtra>;

	fn depth(&self) -> usize {
		self.depth
	}

	fn name(&self) -> Result<&str, WalkError> {
		match &self.inner {
			SecondPassEntryInner::Dir(d) => Ok(d.name),
			SecondPassEntryInner::File(f) => Ok(f.name),
		}
	}

	fn into_entry_type(self) -> EntryType<Self::WalkerDirEntry, Self::WalkerFileEntry> {
		match self.inner {
			SecondPassEntryInner::Dir(d) => EntryType::Dir(d),
			SecondPassEntryInner::File(f) => EntryType::File(f),
		}
	}
}

#[derive(Debug)]
pub(crate) struct DirEntry<Extra> {
	name: DefaultSymbol,
	children_info: DirChildrenInfo,
	extra_data: Extra,
}

impl<Extra> DirEntry<Extra> {
	pub(super) fn from_unfinalized(
		unfinalized: UnfinalizedDirEntry<Extra>,
		children_info: DirChildrenInfo,
	) -> Self {
		Self {
			name: unfinalized.name,
			children_info,
			extra_data: unfinalized.extra_data,
		}
	}

	pub(crate) fn extra_data(&self) -> &Extra {
		&self.extra_data
	}
}

impl<Extra> EntryName for DirEntry<Extra> {
	fn name(&self) -> DefaultSymbol {
		self.name
	}
}

impl<Extra> DirEntry<Extra> {
	pub(crate) fn children_info(&self) -> DirChildrenInfo {
		self.children_info
	}
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DirChildrenInfo {
	start_idx: u32,
	num_children: u32,
}

impl DirChildrenInfo {
	pub(super) fn new(start_idx: u32, num_children: u32) -> Self {
		Self {
			start_idx,
			num_children,
		}
	}

	pub(super) fn as_range(&self) -> std::ops::Range<usize> {
		let start = self.start_idx as usize;
		let end = start + self.num_children as usize;
		start..end
	}

	pub(super) fn into_num_children(self) -> u32 {
		self.num_children
	}
}

#[derive(Debug)]
pub(crate) enum Entry<DirExtra, FileExtra> {
	Dir(DirEntry<DirExtra>),
	File(FileEntry<FileExtra>),
}

impl<DirExtra, FileExtra> EntryName for Entry<DirExtra, FileExtra> {
	fn name(&self) -> DefaultSymbol {
		match self {
			Entry::File(f) => f.name(),
			Entry::Dir(d) => d.name(),
		}
	}
}

/// Trait for deciding which node wins in a name conflict.
pub(crate) trait CompareStrategy<D, F> {
	/// Returns `true` if `new` should replace `existing`.
	fn should_replace(existing: &Entry<D, F>, new: &Entry<D, F>) -> bool;
}

pub(super) enum UnfinalizedEntry<DirExtra, FileExtra> {
	Dir(UnfinalizedDirEntry<DirExtra>),
	File(FileEntry<FileExtra>),
}
