//! Extracting an archive into the drive. See
//! [`Client::extract_archive`](crate::auth::Client::extract_archive).

mod client_impl;
pub(crate) mod codec;
mod engine;
mod report;

use crate::fs::{
	archive::dispose::SourceDisposal,
	categories::{DirType, Normal},
	file::{RemoteFile, enums::RemoteFileType},
	name::ValidatedName,
};

pub use crate::fs::archive::format::archive_default_name;
pub use report::{
	ArchiveEntryId, ArchiveTotals, ExtractActiveFile, ExtractCallback, ExtractEvent, ExtractFailed,
	ExtractFailure, ExtractPhase, ExtractRenameReason, ExtractRenamedEntry, ExtractReport,
	ExtractSkippedEntry, ExtractStage, ExtractTopLevelKey, ExtractUpdate, ExtractedTopLevel,
	OmittedRecords, RunState,
};

/// Where an archive's entries are created.
#[derive(Debug, Clone)]
pub enum ExtractRoot {
	/// In a new folder in the destination, called `name`, or by default the archive's name
	/// without its archive extensions (`photos.tar.gz` → `photos`). A name the destination
	/// holds gets the next keep-both name. A single compressed file ignores this and is always
	/// written straight into the destination.
	NewFolder { name: Option<ValidatedName> },
	/// Straight into the destination; entries whose names it holds get keep-both names.
	Destination,
}

/// The archive to extract, and whether to remove it afterwards.
#[derive(Debug, Clone)]
pub enum ArchiveSource {
	/// Any file the client can read: the user's own, shared, or in a link.
	Keep(RemoteFileType<'static>),
	/// One of the user's own files, removed once the extraction is verified: completed with
	/// nothing failed, skipped or unaccounted, the archive read in full and matching the hash in
	/// its metadata, and every extracted item confirmed with the server. Otherwise it is kept
	/// and the report says why.
	Dispose {
		/// The archive.
		file: RemoteFile,
		/// How it is removed.
		how: SourceDisposal,
	},
}

/// What to extract, and where to.
#[derive(Debug, Clone)]
pub enum ExtractRequest {
	/// Every entry of the archive into `destination`, an existing directory of the user's drive.
	All {
		archive: ArchiveSource,
		destination: DirType<'static, Normal>,
		root: ExtractRoot,
	},
}

/// How much more than it reads a compressed archive may decode to: at most `ratio` times the
/// compressed bytes read so far, but always at least `floor` bytes. Stops a decompression bomb
/// before it costs its full output in time and storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpansionLimit {
	pub ratio: u64,
	pub floor: u64,
}

impl ExpansionLimit {
	/// A thousand times, and at least 256 MiB: more than ordinary data compresses to.
	pub const DEFAULT: Self = Self {
		ratio: 1000,
		floor: 256 << 20,
	};
}

#[derive(Debug, Clone)]
pub struct ExtractConfig {
	/// Storage still free on the account, if the caller knows it: an extraction whose uploads
	/// would reach it fails with [`ErrorKind::MaxStorageReached`](crate::ErrorKind), keeping
	/// what it extracted so far. A streaming archive's size is only known as it is read, so this
	/// is checked as it goes, not up front.
	pub max_bytes: Option<u64>,
	/// Most directories and files created; an archive with more fails with
	/// [`ErrorKind::ArchiveTooLarge`](crate::ErrorKind).
	pub max_items: Option<u64>,
	/// `None` turns the check off.
	pub expansion_limit: Option<ExpansionLimit>,
}

impl Default for ExtractConfig {
	fn default() -> Self {
		Self {
			max_bytes: None,
			max_items: None,
			expansion_limit: Some(ExpansionLimit::DEFAULT),
		}
	}
}

/// Why an archive entry was not extracted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractSkipReason {
	/// A symbolic link, which the drive cannot hold; `target` is the stored target, cut to at most
	/// 4096 bytes.
	Symlink {
		/// The link's target as stored, at most 4096 bytes.
		target: String,
	},
	/// A tar hard link, a second name for the earlier entry at `target` (its path as stored, cut
	/// to at most 4096 bytes), with no data of its own.
	Hardlink {
		/// The path of the entry it names, as stored, at most 4096 bytes.
		target: String,
	},
	/// A device node or FIFO.
	Device,
	/// A sparse file, stored with its holes left out.
	Sparse,
	/// A kind of entry the SDK does not extract, such as a tar multivolume continuation.
	UnsupportedType,
	/// A path longer than 4096 bytes.
	PathTooLong,
	/// A path more than 256 directories deep.
	PathTooDeep,
	/// A path that climbs out of the folder it is extracted into, or cannot be made into drive
	/// names.
	UnsafePath,
}
