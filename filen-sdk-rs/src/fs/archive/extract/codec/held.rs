//! An archive's parsed index, carried from the codec job that read it into later jobs on the same
//! archive, which then start at their entries instead of reading the archive's head and index
//! again. No job stays alive in between: the index is handed back to the driver when its job ends
//! and handed in to the next one.

use std::{mem, num::NonZeroU64, sync::Arc};

use crate::fs::archive::{
	sevenz::read::{HeldKeys, SevenZIndex},
	zip::read::ZipIndex,
};

/// The parsed index of a zip or 7z, or what listing a tar found of its members.
#[derive(Debug)]
pub(crate) enum HeldIndex {
	Zip(ZipIndex),
	/// A tar's, which a codec job cannot start from: a tar is read from its start.
	Tar(TarTable),
	SevenZ {
		index: SevenZIndex,
		/// The keys derived for the index and the entries read with it, under the job's
		/// password: a later job under the same password derives none of them again.
		keys: HeldKeys,
	},
}

impl HeldIndex {
	/// The heap it takes, in bytes.
	pub(crate) fn heap_bytes(&self) -> u64 {
		match self {
			Self::Zip(index) => index.heap_bytes(),
			Self::Tar(table) => (table.members.capacity() * mem::size_of::<TarSpan>()) as u64,
			Self::SevenZ { index, keys } => index.heap_bytes().saturating_add(keys.heap_bytes()),
		}
	}
}

/// Where a tar's members lie, by ordinal: 16 bytes each.
#[derive(Debug)]
pub(crate) struct TarTable {
	/// By ordinal; for a compressed tar, offsets into what it decodes to.
	pub(crate) members: Vec<TarSpan>,
}

/// Where a tar member's data lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TarSpan {
	/// Where its data starts in the tar: after its header, so never 0. `None` for a member
	/// that holds no file's data to read alone: a directory, link or device, or a sparse file,
	/// which one offset cannot describe.
	pub(crate) data: Option<NonZeroU64>,
	pub(crate) size: u64,
}

/// Where a codec job gets the archive's index from.
#[derive(Debug, Clone)]
pub(crate) enum IndexUse {
	/// It reads the archive's head and index itself, as any job does on its own.
	Read,
	/// It reads them, and hands the index to the driver once it is done with it
	/// ([`WorkerEvent::Indexed`](crate::fs::archive::worker::WorkerEvent::Indexed)).
	ReadAndReturn,
	/// An earlier job on the same archive, under the same password, read it: the job reads
	/// neither the head nor the index, and starts at its entries.
	Held(Arc<HeldIndex>),
}
