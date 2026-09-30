//! Compressing drive items into an archive and extracting an archive into the drive. Nothing can
//! run on the server (items are end-to-end encrypted), so an archive's codec runs in the SDK:
//! entries are downloaded and decrypted, (de)compressed, then encrypted and uploaded.
//!
//! The vocabulary the archive jobs share with copying (their control, sources, plan records,
//! counts and run state) is exported from [`fs::copy`](crate::fs::copy), where it first shipped.

mod bytes;
mod compress;
pub(crate) mod config;
mod decode;
mod dispose;
mod encode;
mod entry_path;
mod error;
mod extract;
mod format;
mod hash;
mod input;
#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
mod js_impl;
mod limits;
mod names;
mod password;
mod sevenz;
mod tar_iter;
#[cfg(test)]
mod test_support;
mod worker;
mod zip;

pub use compress::{
	CompressActiveFile, CompressCallback, CompressConfig, CompressCounts, CompressEvent,
	CompressFailed, CompressFormat, CompressPhase, CompressReport, CompressRequest,
	CompressSources, CompressUpdate, HashMismatch,
};
pub use config::ArchiveConfig;
pub use dispose::{DisposalOutcome, KeptReason, SourceDisposal, SourceDisposition};
pub use encode::Compression;
pub use extract::{
	ArchiveEntry, ArchiveEntryId, ArchiveEntryKind, ArchiveSource, DuplicateEntries,
	EntrySelection, ExpansionLimit, ExtractActiveFile, ExtractCallback, ExtractConfig,
	ExtractEvent, ExtractFailed, ExtractFailure, ExtractMisleadingName, ExtractPhase,
	ExtractRenameReason, ExtractRenamedEntry, ExtractReport, ExtractRequest, ExtractRetry,
	ExtractRoot, ExtractSkipReason, ExtractSkippedEntry, ExtractStage, ExtractTopLevelKey,
	ExtractUpdate, ExtractWhat, ExtractedTopLevel, ListCallback, ListConfig, ListFailed, ListPhase,
	ListReport, ListTotals, ListUpdate, ListedPath, MAX_LISTED_BYTES, MAX_LISTED_ENTRIES,
	OmittedRecords, PasswordCheck,
};
pub use format::{ArchiveFormat, StreamCodec, archive_default_name};
pub use password::ArchivePassword;
pub use sevenz::write::{SevenZEncryption, SevenZMethod};
pub use zip::{crypto::AesStrength, write::ZipMethod};
