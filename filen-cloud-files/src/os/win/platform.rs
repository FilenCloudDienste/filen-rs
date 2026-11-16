//! The helper's platform on Windows: sync roots through cfapi, the root's file system, the notify
//! watcher and Credential Manager.

use std::{fs::File, os::windows::fs::OpenOptionsExt, path::Path, sync::Arc, time::SystemTime};

use filen_types::fs::Uuid;
use tracing::{debug, warn};
use windows_sys::Win32::Storage::FileSystem::{
	FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
	FILE_SHARE_WRITE,
};

use crate::{
	auth::{CredentialManagerKeyStore, KeyAccount, KeyStoreError},
	branding::Branding,
	dispatch::Dispatcher,
	drive::Drive,
	helper::{
		BrandingScope, Cleared, ExistingRoot, Platform, PlatformError, PlatformFailed, PlatformOp,
		RootRefusal, UserId, clear_plain_root, path_taken, set_plain_root_aside,
	},
	local::LocalEngine,
	os::{
		LocalWatcher, WatchSink,
		cfapi::{
			SyncRootOwner, SyncRootPathError, WinError, explorer::sweep_keeps, sync_root_owner,
		},
		win::{
			ConnectedRoot, LOG_TARGET, ProviderInfo, RegisteredRoot, SyncRootPath, WinLocalFs,
			explorer,
			ffi::item_info,
			platform_info, register,
			sync_root::sync_root_info,
			system::{is_elevated, volume_file_system},
			take_over,
		},
	},
	sealed,
};

/// The Windows platform the helper serves sync roots on.
#[derive(Debug, Default)]
pub struct WinPlatform;

impl sealed::Sealed for WinPlatform {}

/// `error` as the failure of `op`.
fn failed(op: PlatformOp, error: impl std::error::Error + Send + Sync + 'static) -> PlatformError {
	PlatformError::Failed {
		op,
		source: Box::new(error),
	}
}

impl Platform for WinPlatform {
	type Local = WinLocalFs;
	type Keys = CredentialManagerKeyStore;
	type Registered = RegisteredRoot;
	type Connected = ConnectedRoot;
	type Watcher = LocalWatcher;

	fn platform_build(&self) -> Option<u32> {
		platform_info().ok().map(|info| info.build)
	}

	fn check_root(&self, root: &Path) -> Result<(), RootRefusal> {
		SyncRootPath::from_path(root).map_err(|e| RootRefusal::Invalid(e.to_string()))?;
		if root.parent().is_none() {
			return Err(RootRefusal::DriveRoot);
		}
		let (file_system, at) =
			volume_file_system(root).map_err(|e| RootRefusal::Invalid(e.to_string()))?;
		if !file_system.eq_ignore_ascii_case("NTFS") {
			debug!(target: LOG_TARGET, %file_system, ?at, "the root's volume is not NTFS");
			return Err(RootRefusal::NotNtfs(file_system));
		}
		Ok(())
	}

	/// Read with `CfGetSyncRootInfoByPath` at the root, or at its nearest existing ancestor: a root
	/// inside another sync root, or one that is not this provider's, is not ours.
	fn existing_registration(&self, root: &Path) -> Result<Option<ExistingRoot>, PlatformError> {
		let inspect = |e: WinErrorOrIo| failed(PlatformOp::Inspect, e);
		let Some(at) = root.ancestors().find(|at| at.exists()) else {
			return Ok(None);
		};
		let at_path = SyncRootPath::from_path(at).map_err(|e| inspect(e.into()))?;
		let Some(info) = sync_root_info(&at_path).map_err(|e| inspect(e.into()))? else {
			return Ok(None);
		};
		if at != root {
			return Ok(Some(ExistingRoot {
				ours: false,
				account_root: None,
			}));
		}
		let file_id = File::options()
			.access_mode(FILE_READ_ATTRIBUTES)
			.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
			.custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
			.open(root)
			.and_then(|file| item_info(&file))
			.map_err(|e| inspect(e.into()))?
			.file_id;
		let name: Vec<u16> = PROVIDER_NAME.encode_utf16().collect();
		let owner = sync_root_owner(&info, &name, ProviderInfo::FILEN_ID, file_id);
		debug!(target: LOG_TARGET, ?root, ?owner, root_file_id = info.file_id, file_id, "existing registration");
		Ok(Some(match owner {
			SyncRootOwner::Ours { account_root } => ExistingRoot {
				ours: true,
				account_root: Some(account_root),
			},
			SyncRootOwner::Other => ExistingRoot {
				ours: false,
				account_root: None,
			},
		}))
	}

	/// A new registration when the OS holds none for `root`; this provider's own registration for
	/// the same account is taken over as it is (see [`take_over`]). Another provider's, another
	/// account's, or one of ours whose account cannot be read is refused: the placeholders in it may
	/// belong to another account.
	fn register(&self, root: &Path, account_root: Uuid) -> Result<RegisteredRoot, PlatformError> {
		let path = SyncRootPath::from_path(root).map_err(|e| failed(PlatformOp::Register, e))?;
		let provider = ProviderInfo::new(
			PROVIDER_NAME,
			env!("CARGO_PKG_VERSION"),
			ProviderInfo::FILEN_ID,
		)
		.map_err(|e| failed(PlatformOp::Register, e))?;
		match self.existing_registration(root)? {
			None => {
				register(path, &provider, account_root).map_err(|e| failed(PlatformOp::Register, e))
			}
			Some(ExistingRoot {
				ours: true,
				account_root: Some(registered),
			}) if registered == account_root => Ok(take_over(path)),
			Some(existing) => Err(failed(
				PlatformOp::Register,
				TakeOverRefused {
					ours: existing.ours,
				},
			)),
		}
	}

	fn local_fs(&self, root: &Path) -> Arc<WinLocalFs> {
		let path = SyncRootPath::from_path(root)
			.expect("the helper checks every root with check_root first (should be impossible)");
		// Reason for the Arc: the engine's blocking calls each take a handle.
		Arc::new(WinLocalFs::new(&path))
	}

	fn connect<D: Drive>(
		&self,
		registered: RegisteredRoot,
		dispatcher: Arc<Dispatcher<D>>,
		engine: Arc<LocalEngine<D, WinLocalFs>>,
	) -> Result<ConnectedRoot, PlatformFailed<RegisteredRoot>> {
		registered
			.connect_with_local(dispatcher, engine)
			.map_err(|e| PlatformFailed {
				held: e.root,
				error: failed(PlatformOp::Connect, e.error),
			})
	}

	fn connect_refusing(
		&self,
		registered: RegisteredRoot,
	) -> Result<ConnectedRoot, PlatformFailed<RegisteredRoot>> {
		registered.connect_refusing().map_err(|e| PlatformFailed {
			held: e.root,
			error: failed(PlatformOp::ConnectRefusing, e.error),
		})
	}

	fn disconnect(
		&self,
		connected: ConnectedRoot,
	) -> Result<RegisteredRoot, PlatformFailed<ConnectedRoot>> {
		connected.disconnect().map_err(|e| PlatformFailed {
			held: e.root,
			error: failed(PlatformOp::Disconnect, e.error),
		})
	}

	fn unregister(&self, registered: RegisteredRoot) -> Result<(), PlatformFailed<RegisteredRoot>> {
		registered.unregister().map_err(|e| PlatformFailed {
			held: e.root,
			error: failed(PlatformOp::Unregister, e.error),
		})
	}

	fn clear_unregistered(&self, root: &Path) -> Result<Cleared, PlatformError> {
		clear_plain_root(root).map_err(|e| failed(PlatformOp::Tidy, e))
	}

	fn exists(&self, path: &Path) -> bool {
		path_taken(path)
	}

	fn set_aside(&self, root: &Path, aside: &Path) -> Result<(), PlatformError> {
		set_plain_root_aside(root, aside).map_err(|e| failed(PlatformOp::SetAside, e))
	}

	fn watch(&self, root: &Path, sink: WatchSink) -> Result<LocalWatcher, PlatformError> {
		LocalWatcher::start_signalling(root, sink).map_err(|e| failed(PlatformOp::Watch, e))
	}

	fn key_store(&self, account: &KeyAccount) -> Result<CredentialManagerKeyStore, KeyStoreError> {
		CredentialManagerKeyStore::new(account)
	}

	/// Taken as elevated when the token cannot be read, so the helper refuses to start rather than
	/// run with rights it should not have.
	fn elevated(&self) -> bool {
		is_elevated().unwrap_or_else(|error| {
			warn!(target: LOG_TARGET, %error, "reading the process's elevation failed; taken as elevated");
			true
		})
	}

	fn now(&self) -> SystemTime {
		SystemTime::now()
	}

	fn brand(&self, root: &Path, user: UserId, branding: &Branding) -> Result<(), PlatformError> {
		explorer::brand(root, user, branding).map_err(|e| failed(PlatformOp::Brand, e))
	}

	fn unbrand(
		&self,
		root: &Path,
		user: UserId,
		scope: BrandingScope,
	) -> Result<(), PlatformError> {
		explorer::unbrand(root, user, scope).map_err(|e| failed(PlatformOp::Unbrand, e))
	}

	/// A root whose registration cannot be read counts as live, as does one on a volume not
	/// present (see [`sweep_keeps`]).
	fn sweep_branding(&self) -> Result<u64, PlatformError> {
		explorer::sweep(|root| {
			sweep_keeps(&self.existing_registration(root), || {
				root.ancestors().last().is_some_and(Path::exists)
			})
		})
		.map_err(|e| failed(PlatformOp::SweepBranding, e))
	}
}

/// The provider name every root of the helper is registered with, which tells this provider's
/// registrations from others'.
const PROVIDER_NAME: &str = "Filen";

/// Why a registration the OS holds for a root was not taken over.
#[derive(Debug, thiserror::Error)]
#[error("{}", if *ours {
	"the root is registered for another account, or for one this build cannot read"
} else {
	"another provider registered the root, or it is inside another sync root"
})]
struct TakeOverRefused {
	ours: bool,
}

/// A Windows error or an I/O error, for a platform error's source.
#[derive(Debug, thiserror::Error)]
enum WinErrorOrIo {
	#[error(transparent)]
	Win(#[from] WinError),
	#[error(transparent)]
	Io(#[from] std::io::Error),
	#[error(transparent)]
	Path(#[from] SyncRootPathError),
}
