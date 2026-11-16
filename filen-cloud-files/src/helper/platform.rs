//! What the helper needs from the OS and from the drive, behind two sealed seams, so its whole
//! command loop runs on fakes on every host: [`Platform`] (sync roots, the local file system, the
//! watcher, the key store) and [`DriveOpener`] (the cache, opened on a sealed session).

use std::{fmt, path::Path, sync::Arc, time::SystemTime};

use filen_mobile_native_cache::auth::FilenMobileCacheState;
use filen_types::fs::Uuid;

use crate::{
	auth::{KeyAccount, KeyStore, KeyStoreError, SessionStore},
	branding::Branding,
	dispatch::Dispatcher,
	drive::Drive,
	helper::{
		protocol::{ErrorCode, ReplyError, UserId},
		unregister::Cleared,
	},
	local::LocalEngine,
	os::{LocalFs, WatchSink},
	sealed,
};

/// A platform operation, for errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformOp {
	/// Reading the registration of a path.
	Inspect,
	/// Registering a sync root.
	Register,
	/// Connecting a sync root to the provider.
	Connect,
	/// Connecting a sync root that refuses every request.
	ConnectRefusing,
	/// Disconnecting a sync root.
	Disconnect,
	/// Unregistering a sync root.
	Unregister,
	/// Starting the root's watcher.
	Watch,
	/// Tidying an unregistered root.
	Tidy,
	/// Moving an unregistered root aside.
	SetAside,
	/// Showing a root in File Explorer.
	Brand,
	/// Removing what showed a root in File Explorer.
	Unbrand,
}

impl fmt::Display for PlatformOp {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Inspect => "reading the sync root's registration",
			Self::Register => "registering the sync root",
			Self::Connect => "connecting the sync root",
			Self::ConnectRefusing => "connecting the sync root to refuse every request",
			Self::Disconnect => "disconnecting the sync root",
			Self::Unregister => "unregistering the sync root",
			Self::Watch => "watching the sync root",
			Self::Tidy => "tidying the unregistered root",
			Self::SetAside => "moving the unregistered root aside",
			Self::Brand => "showing the root in File Explorer",
			Self::Unbrand => "removing the root from File Explorer",
		})
	}
}

/// Why a platform operation failed.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
	/// The OS refused it.
	#[error("{op} failed: {source}")]
	Failed {
		/// What was done.
		op: PlatformOp,
		/// What the OS answered.
		#[source]
		source: Box<dyn std::error::Error + Send + Sync>,
	},
}

/// `error` as a reply's error.
pub(crate) fn platform_error(error: &PlatformError) -> ReplyError {
	ReplyError::new(ErrorCode::Os, error.to_string())
}

/// A platform operation that failed, with what it was handed back unchanged.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct PlatformFailed<T: fmt::Debug> {
	/// What the operation was handed, as it was.
	pub held: T,
	/// Why it failed.
	#[source]
	pub error: PlatformError,
}

/// Why a path cannot be a sync root on this OS.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RootRefusal {
	/// Not a path the OS takes as a sync root: relative, UNC, a device path.
	#[error("the root is not a path a sync root can be: {0}")]
	Invalid(String),
	/// A drive's root directory.
	#[error("the root is a drive's root directory")]
	DriveRoot,
	/// Not on an NTFS volume, which the Cloud Files filter needs.
	#[error("the root is on a {0} volume, not NTFS")]
	NotNtfs(String),
}

/// A registration the OS already holds for a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExistingRoot {
	/// Whether this provider registered it.
	pub ours: bool,
	/// The account root its identity carries, when it is ours and the identity reads.
	pub account_root: Option<Uuid>,
}

/// What removing a root from File Explorer may touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrandingScope {
	/// Everything [`Platform::brand`] wrote for the user's root, as the Windows user this process
	/// runs as: what the helper, run as the user, removes.
	User,
	/// Only the machine-wide sync root list entry of the user's root, of the Windows user this
	/// process runs as, when it names exactly the root; the per-user navigation-pane entry stays, as
	/// this process's user hive may be another account's: what an elevated process (the
	/// uninstaller) removes.
	MachineOnly,
}

/// The OS as the helper uses it. Sealed: only this crate's Windows layer and its fake implement
/// it. Every method is synchronous and may block, so the helper calls each off the async workers.
pub trait Platform: sealed::Sealed + Send + Sync + 'static {
	/// The sync root's file system.
	type Local: LocalFs;
	/// The store of a user's sealing key.
	type Keys: KeyStore + 'static;
	/// A registered root, not connected.
	type Registered: fmt::Debug + Send + 'static;
	/// A connected root.
	type Connected: fmt::Debug + Send + 'static;
	/// A running watcher of the root; dropping it stops it.
	type Watcher: Send + 'static;

	/// The Cloud Files platform's build, when known.
	fn platform_build(&self) -> Option<u32>;

	/// Whether `root` can be a sync root on this OS: an absolute local path, not a drive's root, on
	/// an NTFS volume (checked on the root or its nearest existing ancestor).
	fn check_root(&self, root: &Path) -> Result<(), RootRefusal>;

	/// The registration the OS holds for `root`, if any.
	fn existing_registration(&self, root: &Path) -> Result<Option<ExistingRoot>, PlatformError>;

	/// Registers `root` for the account whose root directory is `account_root`, or takes over this
	/// provider's registration of it.
	fn register(&self, root: &Path, account_root: Uuid) -> Result<Self::Registered, PlatformError>;

	/// The file system of the sync root at `root`, which [`Platform::check_root`] took. Blocking.
	fn local_fs(&self, root: &Path) -> Arc<Self::Local>;

	/// Connects `registered`, so the OS sends its requests to `dispatcher` and `engine`.
	fn connect<D: Drive>(
		&self,
		registered: Self::Registered,
		dispatcher: Arc<Dispatcher<D>>,
		engine: Arc<LocalEngine<D, Self::Local>>,
	) -> Result<Self::Connected, PlatformFailed<Self::Registered>>;

	/// Connects `registered` so that it refuses every request: no data or listing is served, and
	/// every delete, rename and dehydration is vetoed, so the tree holds still while it is looked
	/// at with no session.
	fn connect_refusing(
		&self,
		registered: Self::Registered,
	) -> Result<Self::Connected, PlatformFailed<Self::Registered>>;

	/// Disconnects `connected`.
	fn disconnect(
		&self,
		connected: Self::Connected,
	) -> Result<Self::Registered, PlatformFailed<Self::Connected>>;

	/// Unregisters `registered`. The OS deletes the root's placeholders that hold no data.
	fn unregister(
		&self,
		registered: Self::Registered,
	) -> Result<(), PlatformFailed<Self::Registered>>;

	/// After `root` was unregistered, removes its empty ordinary directories, deepest first, and
	/// the root itself when empty; a root that is not there is gone already.
	fn clear_unregistered(&self, root: &Path) -> Result<Cleared, PlatformError>;

	/// Whether anything is at `path`, as an aside path must be free.
	fn exists(&self, path: &Path) -> bool;

	/// Renames the unregistered root `root` to `aside`; refused when anything is at `aside`.
	fn set_aside(&self, root: &Path, aside: &Path) -> Result<(), PlatformError>;

	/// Watches `root`, handing `sink` every event and the watcher's failure.
	fn watch(&self, root: &Path, sink: WatchSink) -> Result<Self::Watcher, PlatformError>;

	/// The store of `account`'s sealing key.
	fn key_store(&self, account: &KeyAccount) -> Result<Self::Keys, KeyStoreError>;

	/// Whether this process runs elevated.
	fn elevated(&self) -> bool;

	/// The time now, which dates a root an unregister sets aside.
	fn now(&self) -> SystemTime;

	/// Shows `root`, `user`'s registered root, in File Explorer as `branding` says: its name, its
	/// status icons and commands, and its navigation-pane entry. Writes every value each time, so
	/// a repeat is harmless; a failure may leave part of it written, which [`Platform::unbrand`]
	/// removes.
	fn brand(&self, root: &Path, user: UserId, branding: &Branding) -> Result<(), PlatformError>;

	/// Removes what [`Platform::brand`] wrote for `user`'s root at `root`, within `scope`; what was
	/// not there is gone already.
	fn unbrand(&self, root: &Path, user: UserId, scope: BrandingScope)
	-> Result<(), PlatformError>;
}

/// Opens the drive on a sealed session. Sealed, as [`Drive`] is.
pub trait DriveOpener: sealed::Sealed + Send + Sync + 'static {
	/// The drive it opens.
	type Drive: Drive;

	/// The drive on `store`'s session. Blocking: call it off the async workers.
	fn open(&self, store: &SessionStore) -> Arc<Self::Drive>;
}

/// Opens the cache with its existing constructor ([`SessionStore::open_cache`]).
#[derive(Debug, Default)]
pub struct CacheOpener;

impl sealed::Sealed for CacheOpener {}

impl DriveOpener for CacheOpener {
	type Drive = FilenMobileCacheState;

	fn open(&self, store: &SessionStore) -> Arc<Self::Drive> {
		// Reason for the Arc: the dispatcher, the local engine and the helper share the cache.
		Arc::new(store.open_cache())
	}
}
