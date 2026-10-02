//! Extracting an archive into the drive. See
//! [`Client::extract_archive`](crate::auth::Client::extract_archive).

mod client_impl;
pub(super) mod codec;
mod engine;
mod list;
pub(super) mod report;
#[cfg(test)]
mod test_support;

use filen_macros::js_type;

use crate::{
	Error, ErrorKind,
	fs::{
		HasUUID,
		archive::SourceDisposal,
		categories::{DirType, Normal},
		drive_job::exceeds_limit,
		file::{RemoteFile, enums::RemoteFileType},
		name::ValidatedName,
	},
};

use codec::Selection;

pub use client_impl::{ExtractConfig, ListConfig};
pub use list::{
	ArchiveEntry, ArchiveEntryKind, EntryAccess, ListCallback, ListFailed, ListPhase, ListReport,
	ListTotals, ListUpdate, ListedPath, ListedSkipReason, MAX_LISTED_BYTES, MAX_LISTED_ENTRIES,
	PasswordCheck,
};
pub use report::{
	ArchiveEntryId, ExtractActiveFile, ExtractCallback, ExtractEvent, ExtractFailed,
	ExtractFailure, ExtractMisleadingName, ExtractPhase, ExtractRenameReason, ExtractRenamedEntry,
	ExtractReport, ExtractRetry, ExtractSkippedEntry, ExtractStage, ExtractTopLevelKey,
	ExtractUpdate, ExtractedTopLevel, OmittedRecords,
};

/// Where an archive's entries are created.
#[derive(Debug, Clone)]
pub enum ExtractRoot {
	/// In a new folder in the destination, called `name`, or by default the name
	/// [`archive_default_name`](super::archive_default_name) makes of the archive's
	/// (`photos.tar.gz` → `photos`). A name the destination holds gets the next keep-both name.
	/// A single compressed file ignores this and is always written straight into the
	/// destination.
	NewFolder {
		/// The new folder's name; `None` for the one made of the archive's.
		name: Option<ValidatedName>,
	},
	/// Straight into the destination; entries whose names it holds get keep-both names.
	Destination,
}

/// The archive to extract, and whether to remove it afterwards.
#[derive(Debug, Clone)]
pub enum ArchiveSource {
	/// Any file the client can read: the user's own, shared, or in a link.
	Keep(RemoteFileType<'static>),
	/// One of the user's own files, removed once the extraction is verified: completed with
	/// nothing failed, skipped or unaccounted, every extracted file's data checked against a
	/// checksum the archive carries for it, and every extracted item confirmed with the server.
	/// A tar or a single compressed file must also have been read in full and match the hash in
	/// its metadata (which covers a tar's own data, as it carries no checksum for it); a zip or
	/// a 7z is confirmed by its entries' checksums and every entry having been extracted, with
	/// no whole-archive hash, even for a permanent deletion. Otherwise it is kept and the report
	/// says why: so a brotli or LZMA-alone archive, or an lz4, xz or zstd one written without
	/// its checksum, is always kept, as
	/// [`KeptReason::Unconfirmed`](crate::fs::archive::KeptReason::Unconfirmed).
	Dispose {
		/// The archive.
		file: RemoteFile,
		/// How it is removed.
		how: SourceDisposal,
	},
}

/// What to extract, and where to.
#[derive(Debug, Clone)]
pub struct ExtractRequest {
	/// The archive, and which of its entries to extract.
	pub what: ExtractWhat,
	/// An existing directory of the user's drive.
	pub destination: DirType<'static, Normal>,
	/// Whether the entries go into a new folder or straight into `destination`.
	pub root: ExtractRoot,
}

/// Which of an archive's entries to extract.
#[derive(Debug, Clone)]
pub enum ExtractWhat {
	/// Every entry of the archive.
	All(ArchiveSource),
	/// Some of the archive's entries. The archive is never removed afterwards: part of it is
	/// not extracted.
	Entries(EntrySelection),
}

/// Some entries of one archive, chosen to extract: those the ids name (from
/// [`Client::list_archive`](crate::auth::Client::list_archive), or a failure's
/// [`ExtractRetry`]), everything below a directory among them, and the directories that hold
/// them.
///
/// Each lands at its path in the archive less `base`, a directory of the archive as drive
/// names: with `base` `[photos]`, the entry `photos/2024/a.jpg` lands at `2024/a.jpg` in the
/// root. An empty `base` keeps the archive's paths. A zip's or 7z's ids are checked against its
/// index before anything is created: an id it does not hold, of an entry not below `base`, or
/// of a file at `base` itself fails the job. A tar's members are only known as it is read: a
/// member chosen that is not below `base` fails the job when it is reached, and an id the tar
/// does not hold once it is read to its end, what was extracted until then staying.
///
/// A chosen directory of a tar brings what the tar stores after it below it: every tool
/// stores a directory before its contents, and what came before is gone by the time the
/// directory is reached. A zip's or 7z's brings everything below it, wherever it is stored.
/// A tar's hard link is extracted only along with the entry it names (see
/// [`ArchiveEntryKind::Hardlink`]).
#[derive(Debug, Clone)]
pub struct EntrySelection {
	archive: RemoteFileType<'static>,
	selection: Selection,
}

impl EntrySelection {
	/// The entries `ids` names of `archive`, below `base`. Fails with
	/// [`ErrorKind::InvalidState`] when `ids` is empty or names an entry of another archive.
	pub fn new(
		archive: RemoteFileType<'static>,
		ids: Vec<ArchiveEntryId>,
		base: Vec<ValidatedName>,
	) -> Result<Self, Error> {
		if ids.is_empty() {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				"no entry was chosen to extract",
			));
		}
		if ids.iter().any(|id| id.archive != archive.uuid()) {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				"an entry chosen to extract is of another archive",
			));
		}
		let selection = Selection::new(ids.into_iter().map(|id| u64::from(id.index)), base);
		Ok(Self { archive, selection })
	}

	/// The archive the entries are of.
	pub fn archive(&self) -> &RemoteFileType<'static> {
		&self.archive
	}

	pub(crate) fn into_parts(self) -> (RemoteFileType<'static>, Selection) {
		(self.archive, self.selection)
	}
}

/// How much more than it reads a compressed archive may decode to: at most `ratio` times the
/// compressed bytes read so far, but always at least `floor` bytes. Stops a decompression bomb
/// before it costs its full output in time and storage.
///
/// A tar's hard links, each extracted as a copy of the file it names, are held to the same
/// bound, compressed or not: what they copy in all may not pass it either, so a small tar of
/// one file and many links to it fails with `ArchiveTooLarge` rather than upload that file each
/// time.
///
/// Only ever passed in, so the bindings take either number type for both, as they do for
/// their other sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(import, no_default)]
pub struct ExpansionLimit {
	/// Bytes an archive may decode to per compressed byte read.
	#[cfg_attr(feature = "wasm-full", tsify(type = "number | bigint"))]
	pub ratio: u64,
	/// Bytes an archive may always decode to, however little of it was read.
	#[cfg_attr(feature = "wasm-full", tsify(type = "number | bigint"))]
	pub floor: u64,
}

impl Default for ExpansionLimit {
	/// A thousand times, and at least 256 MiB: more than ordinary data compresses to.
	fn default() -> Self {
		Self {
			ratio: 1000,
			floor: 256 << 20,
		}
	}
}

impl ExpansionLimit {
	/// Whether `decoded` bytes are within the limit for an archive of which `read` bytes were
	/// read.
	pub(crate) fn allows(self, read: u64, decoded: u64) -> bool {
		decoded <= self.floor.max(read.saturating_mul(self.ratio))
	}
}

/// The error for an extraction whose files, `bytes` in all, need more than `max_bytes`; `None`
/// while they fit.
pub(crate) fn storage_exceeded(max_bytes: Option<u64>, bytes: u64) -> Option<Error> {
	let max = max_bytes?;
	exceeds_limit(bytes, max).then(|| {
		Error::custom(
			ErrorKind::MaxStorageReached,
			format!("the extraction needs more than the {max} bytes that are free"),
		)
	})
}

/// Why an archive entry was not extracted.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, tagged, camel_case_fields, no_default)]
pub enum ExtractSkipReason {
	/// A symbolic link, which the drive cannot hold; `target` is the stored target, cut to at most
	/// 4096 bytes.
	Symlink {
		/// The link's target as stored, at most 4096 bytes.
		target: String,
	},
	/// A tar hard link, a second name for the earlier entry at `target` (its path as stored, cut
	/// to at most 4096 bytes), with no data of its own, when no file was extracted for `target`
	/// to copy: it was skipped or failed, or is no file of this archive.
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
	/// A zip entry whose data overlaps another's, which a well-formed zip never has.
	OverlappingData,
	/// Data compressed or encrypted in a way the SDK does not read (zip PPMd, encrypted LZMA
	/// and XZ zip entries, or a 7z coder other than LZMA, LZMA2, PPMd, BZip2, Deflate(64),
	/// zstd, the branch and delta filters and AES).
	UnsupportedMethod,
	/// A 7z deletion marker, which an update archive carries for a file it removed.
	AntiItem,
	/// macOS metadata left out (see the extraction's `skip_mac_metadata`).
	MacMetadata,
}

/// Names a zip lists more than once; the last entry of each name is extracted, as other zip
/// tools do.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct DuplicateEntries {
	/// Up to 100 of the names.
	pub names: Vec<String>,
	/// How many entries were left out for a later one of the same name.
	pub count: u64,
}
