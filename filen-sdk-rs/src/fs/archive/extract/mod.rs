//! Extracting an archive into the drive. See
//! [`Client::extract_archive`](crate::auth::Client::extract_archive).

mod client_impl;
pub(crate) mod codec;
mod engine;
mod input;
mod list;
mod report;

use filen_macros::js_type;

use crate::{
	Error, ErrorKind,
	fs::{
		archive::dispose::SourceDisposal,
		categories::{DirType, Normal},
		file::{RemoteFile, enums::RemoteFileType},
		name::ValidatedName,
	},
};

pub use crate::fs::{
	archive::{
		format::{ArchiveFormat, archive_default_name},
		password::ArchivePassword,
	},
	drive_job::counts::ItemCounts,
};
#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
pub(crate) use client_impl::check_entries;
pub use list::{
	ArchiveEntry, ArchiveEntryKind, ArchiveListing, ListCallback, ListFailed, ListPhase,
	ListTotals, ListUpdate, MAX_LISTED_BYTES, MAX_LISTED_ENTRIES, PasswordCheck,
};
pub use report::{
	ArchiveEntryId, ArchiveTotals, ExtractActiveFile, ExtractCallback, ExtractEvent, ExtractFailed,
	ExtractFailure, ExtractMisleadingName, ExtractPhase, ExtractRenameReason, ExtractRenamedEntry,
	ExtractReport, ExtractRetry, ExtractSkippedEntry, ExtractStage, ExtractTopLevelKey,
	ExtractUpdate, ExtractedTopLevel, OmittedRecords, RunState,
};

/// Where an archive's entries are created.
#[derive(Debug, Clone)]
pub enum ExtractRoot {
	/// In a new folder in the destination, called `name`, or by default the name
	/// [`archive_default_name`] makes of the archive's (`photos.tar.gz` → `photos`). A name the
	/// destination holds gets the next keep-both name. A single compressed file ignores this and is always
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
	/// its metadata, every extracted file's data checked against a checksum the archive carries
	/// for it (a tar's own data needs none: the hash covers it), and every extracted item
	/// confirmed with the server. Otherwise it is kept and the report says why: so a brotli or
	/// LZMA-alone archive, or an lz4, xz or zstd one written without its checksum, is always
	/// kept, as [`KeptReason::Unconfirmed`](crate::fs::archive::KeptReason::Unconfirmed).
	Dispose {
		file: RemoteFile,
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
	/// Some of the archive's entries into `destination`: those `ids` names (from
	/// [`Client::list_archive`](crate::auth::Client::list_archive), or a failure's
	/// [`ExtractRetry`]), everything below a directory among them, and the directories that hold
	/// them.
	///
	/// Each lands at its path in the archive less `base`, a directory of the archive as drive
	/// names: with `base` `[photos]`, the entry `photos/2024/a.jpg` lands at `2024/a.jpg` in the
	/// root. An empty `base` keeps the archive's paths. No id, or an id of another archive,
	/// fails the job before anything runs. A zip's or 7z's ids are checked against its
	/// index before anything is created: an id it does not hold, of an entry not below `base`, or
	/// of a file at `base` itself fails the job. A tar's members are only known as it is read: a
	/// member chosen that is not below `base` fails the job when it is reached, and an id the tar
	/// does not hold once it is read to its end, what was extracted until then staying.
	///
	/// A chosen directory of a tar brings what the tar stores after it below it: every tool
	/// stores a directory before its contents, and what came before is gone by the time the
	/// directory is reached. A zip's or 7z's brings everything below it, wherever it is stored.
	///
	/// The archive is never removed afterwards: part of it is not extracted.
	Entries {
		archive: RemoteFileType<'static>,
		ids: Vec<ArchiveEntryId>,
		base: Vec<ValidatedName>,
		destination: DirType<'static, Normal>,
		root: ExtractRoot,
	},
}

/// How much more than it reads a compressed archive may decode to: at most `ratio` times the
/// compressed bytes read so far, but always at least `floor` bytes. Stops a decompression bomb
/// before it costs its full output in time and storage.
///
/// A tar's hard links, each extracted as a copy of the file it names, are held to the same
/// bound, compressed or not: what they copy in all may not pass it either, so a small tar of
/// one file and many links to it fails with
/// [`ErrorKind::ArchiveTooLarge`](crate::ErrorKind) rather than upload that file each time.
///
/// Only ever passed in, so the bindings take either number type for both, as they do for
/// their other sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(import, no_default)]
pub struct ExpansionLimit {
	#[cfg_attr(feature = "wasm-full", tsify(type = "number | bigint"))]
	pub ratio: u64,
	#[cfg_attr(feature = "wasm-full", tsify(type = "number | bigint"))]
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
	/// Storage still free on the account, if the caller knows it: an extraction whose files
	/// would reach it fails with [`ErrorKind::MaxStorageReached`](crate::ErrorKind). A zip or 7z
	/// states its files' sizes in its index, so one stating that much for the files it will
	/// extract (those skipped for their path or method left out) fails before anything is
	/// created; a tar or single compressed file is only known as it is read, so it is checked as
	/// it goes, and what was extracted so far is kept. A zip entry found overlapping another
	/// only once it is read still counts up front, so such a zip may be refused though it fits.
	pub max_bytes: Option<u64>,
	/// Most directories and files created; an archive with more fails with
	/// [`ErrorKind::ArchiveTooLarge`](crate::ErrorKind).
	pub max_items: Option<u64>,
	/// `None` turns the check off.
	pub expansion_limit: Option<ExpansionLimit>,
	/// For an archive with encrypted entries. Checked before anything is created on a 7z's
	/// encrypted header, or else by reading the encrypted entry quickest to read in full, when
	/// that takes at most 16 MiB of the archive. Otherwise it is checked as entries are
	/// extracted: a wrong password found then fails the job with
	/// [`ErrorKind::ArchiveWrongPassword`](crate::ErrorKind), and when no file was extracted by
	/// then, the folders created so far go to the trash (a folder holding a file someone else put
	/// there meanwhile stays).
	pub password: Option<ArchivePassword>,
	/// Leaves out the metadata macOS writes beside files where it cannot keep it with them:
	/// everything in a `__MACOSX` folder (Finder's zips) and AppleDouble files (`._name`, told by
	/// the four bytes they start with), reported skipped as
	/// [`ExtractSkipReason::MacMetadata`]. Left out on purpose, they keep nothing from removing
	/// the archive once the rest is extracted. `true` by default; `false` extracts them as
	/// ordinary files.
	pub skip_mac_metadata: bool,
}

impl Default for ExtractConfig {
	fn default() -> Self {
		Self {
			max_bytes: None,
			max_items: None,
			expansion_limit: Some(ExpansionLimit::DEFAULT),
			password: None,
			skip_mac_metadata: true,
		}
	}
}

/// The error for an extraction whose files, `bytes` in all, reach `max_bytes`; `None` while
/// they fit. Files holding no bytes fit whatever the limit: empty files and directories take
/// no storage, and the check as data is written only ever runs on some.
pub(crate) fn storage_exceeded(max_bytes: Option<u64>, bytes: u64) -> Option<Error> {
	let max = max_bytes?;
	(bytes > 0 && bytes >= max).then(|| {
		Error::custom(
			ErrorKind::MaxStorageReached,
			format!("the extraction needs more than the {max} bytes that are free"),
		)
	})
}

/// Why an archive entry was not extracted.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, tsify::Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints),
	serde(
		tag = "type",
		rename_all = "camelCase",
		rename_all_fields = "camelCase"
	)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum ExtractSkipReason {
	/// A symbolic link, which the drive cannot hold; `target` is the stored target, cut to at most
	/// 4096 bytes.
	Symlink { target: String },
	/// A tar hard link, a second name for the earlier entry at `target` (its path as stored, cut
	/// to at most 4096 bytes), with no data of its own, when no file was extracted for `target`
	/// to copy: it was skipped or failed, or is no file of this archive.
	Hardlink { target: String },
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
	/// macOS metadata left out (see [`ExtractConfig::skip_mac_metadata`]).
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
