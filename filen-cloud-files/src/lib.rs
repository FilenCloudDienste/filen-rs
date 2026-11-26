//! The platform-independent core of Filen's Windows files-on-demand provider, built on the Windows
//! Cloud Files API (cfapi): what a placeholder's file identity holds, which remote names Windows
//! can hold, the values a placeholder is created with, the seam through which the OS layer hands
//! the core its requests and the core answers them, and the dispatcher that answers them from the
//! cache: populating directories and hydrating byte ranges, and the store that seals the session
//! the desktop app pushes into the auth.json the cache reads. Only the Windows layer, built on
//! Windows alone, calls Windows; the `test-seams` feature exposes a fake OS and an in-memory key
//! store the tests drive the core with.

#![warn(unreachable_pub, unused_qualifications)]
// Stdout carries the helper's protocol alone.
#![deny(clippy::print_stdout)]

mod auth;
mod branding;
mod dispatch;
mod drive;
mod failure;
mod helper;
mod hydrate;
mod identity;
mod local;
mod log_safe;
mod names;
mod os;
mod placeholder;
mod populate;
mod root_path;
#[cfg(any(test, feature = "test-seams"))]
pub mod test_support;
mod thumbnails;

/// Seals the OS-facing traits: only this crate implements them, so the core can rely on what each
/// implementation does.
mod sealed {
	pub trait Sealed {}
}

// Exported in tests too, where the Credential Manager store is built on every host.
pub use auth::{
	AuthPayload, AuthPayloadError, DataDir, DataDirError, KeyAccount, KeyAccountError,
	KeyLengthError, KeyStore, KeyStoreError, SEALING_KEY_LEN, SealingKey, SessionError,
	SessionOwner, SessionStore,
};
#[cfg(any(windows, test))]
pub use auth::{CredentialError, CredentialManagerKeyStore};
pub use branding::{
	AppPath, Branding, BrandingError, CommandLabel, DisplayName, ExplorerCommands, IconPath,
};
pub use dispatch::{CacheDispatcher, CloudFilesCallback, DispatchConfig, Dispatcher};
pub use drive::Drive;
pub use failure::Failure;
pub use helper::{
	AbandonReport, ActivityEntry, ActivityKind, BrandingScope, CacheOpener, Cleared, DriveOpener,
	ErrorCode, ExistingRoot, ExitReason, FatalCode, Hello, HelperConfig, IdentifiedItem, InMessage,
	IncompleteItem, ItemPath, KeptEarlier, KeptId, KeptItem, KeptItemReason, MAX_LINE_BYTES,
	MAX_LIST_ENTRIES, MAX_MESSAGE_CHARS, MIN_BUILD, OutMessage, PROTOCOL_VERSION, ParseError,
	PhaseName, Platform, PlatformError, PlatformFailed, PlatformOp, Registration, ReplyError,
	ReplyValue, Request, RequestId, RootDir, RootDirError, RootListing, RootRefusal, SkippedEntry,
	SkippedPlace, StateDir, StateDirError, StateLock, StateReport, StatusEvent,
	THUMBNAIL_SERVER_ARG, THUMBNAIL_SERVER_IDLE, UnregisterAll, UnregisterAllEntry,
	UnregisterReport, UserId, fatal, install_stderr_logging, parse_line, run_helper,
	serve_refusing_thumbnails, serve_stdio, unregister_all, unregister_all_stdio,
};
pub use identity::{IdentityBlob, IdentityError, MAX_IDENTITY_LEN, PlaceholderIdentity};
pub use local::{
	CacheLocalEngine, ConflictKind, KeptReason, LocalConfig, LocalEngine, LocalReport, LocalSkip,
	RemoteApplied, RemoteHints, UndoneOp, VetoedOp,
};
pub use names::{NameRejection, PlaceholderName};
pub use os::{
	AlignedChunk, ByteRange, CancelRequest, CancelledRange, CheckedHash, Completed, DataResponder,
	DehydrateReason, DehydrateRequest, DehydrateResponder, DeleteRequest, DeleteResponder,
	EntryOutcome, FetchDataRequest, FileSnapshot, FileStamp, IfPinned, IncompleteCopy, LocalEntry,
	LocalError, LocalEvent, LocalFileId, LocalFs, LocalKind, LocalWatcher, OptionalRange, OsError,
	PinAttr, PlaceholderState, PopulateRequest, PopulateResponder, RenameRequest, RenameResponder,
	RenameTarget, RequestedRange, TransferKey, UnsupportedKind, Updated, WatchError, WatchSignal,
	WatchSink,
};
// Exported in tests too, where `os::cfapi` is built on every host.
#[cfg(any(windows, test))]
pub use os::cfapi::{CfCall, Hresult, ProviderInfoError, SyncRootPathError, WinError};
#[cfg(windows)]
pub use os::win::{
	ComSecurity, ConnectError, ConnectFailed, ConnectedRoot, DisconnectFailed, PlatformInfo,
	ProviderInfo, RegisterFailed, RegisteredRoot, SyncRootPath, ThumbnailServer, UnregisterFailed,
	WinLocalFs, WinPlatform, WinSettled, attach_parent_console, is_elevated, platform_info,
	register, take_over, unregister_path, volume_file_system,
};
pub use placeholder::{
	FileSize, FileTime, NegativeSize, PlaceholderSpec, SpecKind, UnrepresentableTime,
};
pub use populate::{SkipReason, SkippedItem};
pub use root_path::{CaseKey, RootPath, RootPathError};
pub use thumbnails::{
	Thumbnail, ThumbnailAlpha, ThumbnailCancel, ThumbnailHandler, ThumbnailRefusal,
};
