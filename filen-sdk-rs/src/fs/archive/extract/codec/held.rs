//! An archive's parsed index, carried from the codec job that read it into later jobs on the same
//! archive, which then start at their entries instead of reading the archive's head and index
//! again. No job stays alive in between: the index is handed back to the driver when its job ends
//! and handed in to the next one.

use std::sync::Arc;

use crate::fs::archive::{
	format::ArchiveFormat,
	sevenz::read::{HeldKeys, SevenZIndex},
	zip::read::ZipIndex,
};

/// The parsed index of a zip or 7z.
#[derive(Debug)]
pub(crate) enum HeldIndex {
	Zip(ZipIndex),
	SevenZ {
		index: SevenZIndex,
		/// The keys derived for the index and the entries read with it, under the job's
		/// password: a later job under the same password derives none of them again.
		keys: HeldKeys,
	},
}

impl HeldIndex {
	/// What the archive is.
	#[cfg_attr(
		not(test),
		expect(
			dead_code,
			reason = "the archive reader, which holds indexes, reads it"
		)
	)]
	pub(crate) fn format(&self) -> ArchiveFormat {
		match self {
			Self::Zip(_) => ArchiveFormat::Zip,
			Self::SevenZ { .. } => ArchiveFormat::SevenZ,
		}
	}
}

/// Where a codec job gets the archive's index from.
#[derive(Debug, Clone)]
#[cfg_attr(
	not(test),
	expect(
		dead_code,
		reason = "the archive reader, which holds indexes, starts such jobs"
	)
)]
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
