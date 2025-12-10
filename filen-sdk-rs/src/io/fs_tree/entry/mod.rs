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
}

impl<Extra> FileEntry<Extra> {
	pub(crate) fn new(name: DefaultSymbol, extra_data: Extra) -> Self {
		Self { name, extra_data }
	}

	pub(crate) fn extra_data(&self) -> &Extra {
		&self.extra_data
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
