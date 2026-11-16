//! A sync root's life: its path and provider, registration, connection to a dispatcher,
//! disconnection and unregistration.

use std::{
	ffi::OsString,
	fmt, mem,
	os::windows::ffi::{OsStrExt, OsStringExt},
	path::{Path, PathBuf},
	sync::{Arc, PoisonError, RwLock},
};

use filen_types::fs::Uuid;
use tokio::runtime::RuntimeFlavor;
use tracing::warn;

use super::{
	LOG_TARGET,
	ffi::{self, Registration},
	local::WinLocalFs,
	responder::{WinData, WinDehydrate, WinDelete, WinPopulate, WinRename},
};
use crate::{
	dispatch::Dispatcher,
	drive::Drive,
	failure::Failure,
	identity::PlaceholderIdentity,
	local::LocalEngine,
	os::{
		CancelRequest, Completed, DataResponder, DehydrateRequest, DehydrateResponder,
		DeleteRequest, DeleteResponder, FetchDataRequest, LocalEvent, PopulateRequest,
		PopulateResponder, RenameRequest, RenameResponder,
		cfapi::{
			CfCall, E_FAIL, ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT, ERROR_FILE_NOT_FOUND,
			ERROR_NOT_A_CLOUD_SYNC_ROOT, ERROR_PATH_NOT_FOUND, Hresult, MAX_PROVIDER_NAME_UNITS,
			MAX_PROVIDER_VERSION_UNITS, ProviderInfoError, SyncRootIdentity, SyncRootPathError,
			SyncRootStandard, WideNul, WinError, parse_sync_root_info, provider_units,
			sync_root_units,
		},
	},
};

/// The `WinError` of `call` when `hr` is a failure.
fn check(call: CfCall, hr: Hresult) -> Result<(), WinError> {
	if hr.is_ok() {
		Ok(())
	} else {
		Err(WinError { call, code: hr })
	}
}

/// A directory a sync root can be registered at: an absolute path on a local drive (`X:\...` or
/// `\\?\X:\...`), kept as the UTF-16 cfapi takes, converted without loss. `Debug` shows it as
/// text, lossily.
#[derive(Clone, PartialEq, Eq)]
pub struct SyncRootPath(WideNul);

impl SyncRootPath {
	/// `path`, refused when it is empty, holds a NUL, or is relative, UNC or a device path.
	pub fn from_path(path: &Path) -> Result<Self, SyncRootPathError> {
		let units: Vec<u16> = path.as_os_str().encode_wide().collect();
		sync_root_units(&units).map(Self)
	}

	/// The path.
	pub fn as_path(&self) -> PathBuf {
		PathBuf::from(OsString::from_wide(self.0.without_nul()))
	}
}

impl fmt::Debug for SyncRootPath {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		fmt::Debug::fmt(&self.0, f)
	}
}

/// Who serves a sync root: a name and version Windows shows, and a GUID that never changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
	name: WideNul,
	version: WideNul,
	id: u128,
}

impl ProviderInfo {
	/// Filen's provider GUID. Part of every registration's identity: never change it once shipped.
	pub const FILEN_ID: u128 = 0xB895_1154_1E5E_45B3_A367_0589_E4DE_5FDC;

	/// The provider `name` at `version`, with the GUID `id`; each string 1 to 255 UTF-16 units,
	/// with no NUL.
	pub fn new(name: &str, version: &str, id: u128) -> Result<Self, ProviderInfoError> {
		Ok(Self {
			name: provider_units(name, MAX_PROVIDER_NAME_UNITS)?,
			version: provider_units(version, MAX_PROVIDER_VERSION_UNITS)?,
			id,
		})
	}
}

/// The Cloud Files platform's version (`CF_PLATFORM_INFO`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformInfo {
	/// The Windows build number.
	pub build: u32,
	/// The build's revision.
	pub revision: u32,
	/// The platform's integration number.
	pub integration: u32,
}

/// The Cloud Files platform's version.
pub fn platform_info() -> Result<PlatformInfo, WinError> {
	let info = ffi::platform_info().map_err(|code| WinError {
		call: CfCall::PlatformInfo,
		code,
	})?;
	Ok(PlatformInfo {
		build: info.BuildNumber,
		revision: info.RevisionNumber,
		integration: info.IntegrationNumber,
	})
}

/// A registration that failed, with the path back.
#[derive(Debug, thiserror::Error)]
#[error("registering the sync root {path:?} failed")]
pub struct RegisterFailed {
	/// The path that was not registered.
	pub path: SyncRootPath,
	/// Why.
	#[source]
	pub error: WinError,
}

/// An unregistration that failed, with the root back.
#[derive(Debug, thiserror::Error)]
#[error("unregistering the sync root {:?} failed", root.path())]
pub struct UnregisterFailed {
	/// The root, still registered.
	pub root: RegisteredRoot,
	/// Why.
	#[source]
	pub error: WinError,
}

/// Registers `path` as a sync root served by `provider` for the account whose root directory is
/// `account_root`, which the root's identity carries (so a root another account left behind is
/// refused). The root directory's file identity is the same root identity, so the root's own
/// callbacks carry it.
pub fn register(
	path: SyncRootPath,
	provider: &ProviderInfo,
	account_root: Uuid,
) -> Result<RegisteredRoot, RegisterFailed> {
	let sync_root_identity = SyncRootIdentity::new(provider.id, account_root);
	let root_identity = PlaceholderIdentity::Root { root: account_root }.encode();
	let registration = Registration {
		name: &provider.name,
		version: &provider.version,
		provider_id: provider.id,
		sync_root_identity: &sync_root_identity,
		root_identity: &root_identity,
	};
	match check(CfCall::Register, ffi::register(&path.0, &registration)) {
		Ok(()) => Ok(RegisteredRoot { path }),
		Err(error) => Err(RegisterFailed { path, error }),
	}
}

/// Takes over this provider's existing registration of `path`, one an earlier run made and the
/// caller checked is this provider's for the right account: the registration stays as it is.
/// Not registered again with `CF_REGISTER_FLAG_UPDATE`: measured (Windows 11, in the VM), a root
/// updated that way and then disconnected refused new files in its top directory with `0x801F0005`
/// (`ERROR_FLT_INVALID_NAME_REQUEST`), as a directory never listed does while disconnected, where a
/// root registered once took them.
pub fn take_over(path: SyncRootPath) -> RegisteredRoot {
	RegisteredRoot { path }
}

/// The registration of the sync root `path` is at or below, `None` when it is in none (or does not
/// exist).
pub(crate) fn sync_root_info(path: &SyncRootPath) -> Result<Option<SyncRootStandard>, WinError> {
	match ffi::sync_root_info(&path.0) {
		Ok(bytes) => parse_sync_root_info(&bytes).map(Some).ok_or(WinError {
			call: CfCall::SyncRootInfo,
			code: Hresult::new(E_FAIL),
		}),
		Err(code)
			if [
				ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT,
				ERROR_NOT_A_CLOUD_SYNC_ROOT,
				ERROR_FILE_NOT_FOUND,
				ERROR_PATH_NOT_FOUND,
			]
			.into_iter()
			.any(|none| code == Hresult::from_win32(none)) =>
		{
			Ok(None)
		}
		Err(code) => Err(WinError {
			call: CfCall::SyncRootInfo,
			code,
		}),
	}
}

/// Unregisters the sync root at `path`, one registered by an earlier run. Unregistering deletes
/// the root's dehydrated placeholders; hydrated files stay as ordinary files.
pub fn unregister_path(path: &SyncRootPath) -> Result<(), WinError> {
	check(CfCall::Unregister, ffi::unregister(&path.0))
}

/// A registered sync root. Dropping it leaves the registration in place on purpose: it persists
/// across runs by design, and unregistering deletes dehydrated placeholders, so it happens only
/// through [`RegisteredRoot::unregister`] or [`unregister_path`].
#[derive(Debug)]
pub struct RegisteredRoot {
	path: SyncRootPath,
}

impl RegisteredRoot {
	/// Where the root is.
	pub fn path(&self) -> &SyncRootPath {
		&self.path
	}

	/// Unregisters the root, deleting its dehydrated placeholders.
	pub fn unregister(self) -> Result<(), UnregisterFailed> {
		unregister_path(&self.path).map_err(|error| UnregisterFailed { root: self, error })
	}

	/// Connects the root, so the OS sends its data and population requests to `dispatcher`, and
	/// hands `local` every delete, rename and dehydration of a placeholder to allow or veto, and the
	/// closes and completions it looks at. Start `local` ([`LocalEngine::start`]) and the watcher
	/// once connected. Refused for a dispatcher on a current-thread runtime: each cfapi transfer
	/// blocks the worker it runs on, which only a multi-thread runtime hands off
	/// (`block_in_place`).
	pub fn connect_with_local<D: Drive>(
		self,
		dispatcher: Arc<Dispatcher<D>>,
		local: Arc<LocalEngine<D, WinLocalFs>>,
	) -> Result<ConnectedRoot, ConnectFailed> {
		let serving = Serving { dispatcher, local };
		if serving.dispatcher.runtime_flavor() != RuntimeFlavor::MultiThread {
			return Err(ConnectFailed {
				root: self,
				error: ConnectError::CurrentThreadRuntime,
			});
		}
		self.connect_to(Arc::new(serving))
	}

	/// Connects the root so that it refuses every request, with no session needed: data and
	/// listings fail as not signed in (readers see `ERROR_CLOUD_FILE_AUTHENTICATION_FAILED`), and
	/// every delete, rename and dehydration is vetoed. While it is up the tree holds still and the
	/// provider's own process reads placeholders as they are, which a disconnected root disguises
	/// (measured).
	pub fn connect_refusing(self) -> Result<ConnectedRoot, ConnectFailed> {
		self.connect_to(Arc::new(Refusing))
	}

	/// Connects the root with `target` answering its callbacks.
	fn connect_to(self, target: Arc<dyn Requests>) -> Result<ConnectedRoot, ConnectFailed> {
		// Leaked: one small allocation per connect, so the context stays valid for any callback,
		// even one cfapi runs after the disconnect returns, which is not known not to happen.
		let slot: &'static ContextSlot = Box::leak(Box::new(ContextSlot {
			root: Box::from(self.path.0.without_nul()),
			target: RwLock::new(Some(target)),
		}));
		match ffi::connect(&self.path.0, slot) {
			Ok(key) => Ok(ConnectedRoot {
				root: self,
				connection: Connection {
					key,
					slot,
					connected: true,
				},
			}),
			Err(code) => {
				slot.clear();
				Err(ConnectFailed {
					root: self,
					error: ConnectError::Os(WinError {
						call: CfCall::Connect,
						code,
					}),
				})
			}
		}
	}
}

/// Why a root did not connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
	/// The dispatcher runs on a current-thread runtime.
	#[error("the dispatcher's runtime is not multi-thread")]
	CurrentThreadRuntime,
	/// cfapi refused the connection.
	#[error(transparent)]
	Os(#[from] WinError),
}

/// A connection that failed, with the root back.
#[derive(Debug, thiserror::Error)]
#[error("connecting the sync root {:?} failed", root.path())]
pub struct ConnectFailed {
	/// The root, still registered and not connected.
	pub root: RegisteredRoot,
	/// Why.
	#[source]
	pub error: ConnectError,
}

/// A disconnection that failed, with the connected root back.
#[derive(Debug, thiserror::Error)]
#[error("disconnecting the sync root {:?} failed", root.path())]
pub struct DisconnectFailed {
	/// The root, still connected.
	pub root: ConnectedRoot,
	/// Why.
	#[source]
	pub error: WinError,
}

/// A registered root connected to a dispatcher: the OS sends its requests to the dispatcher until
/// it is disconnected. Dropping it disconnects, best effort, once: a failure is logged and the
/// dropped root is gone, while cfapi may keep the connection, so a later connect of the same path
/// in this process may be refused. Shut down through [`ConnectedRoot::disconnect`], which hands
/// the root back on a failure.
///
/// Shut down in this order: [`Dispatcher::shutdown`], which fails every pending request while the
/// root is still connected; then [`ConnectedRoot::disconnect`]; then
/// [`RegisteredRoot::unregister`], only when the root is to be removed.
#[derive(Debug)]
pub struct ConnectedRoot {
	root: RegisteredRoot,
	connection: Connection,
}

impl ConnectedRoot {
	/// Where the root is.
	pub fn path(&self) -> &SyncRootPath {
		self.root.path()
	}

	/// Disconnects the root: the OS sends it no more requests, and a request still in flight
	/// finds no dispatcher and fails as transient.
	pub fn disconnect(mut self) -> Result<RegisteredRoot, DisconnectFailed> {
		match self.connection.disconnect() {
			Ok(()) => Ok(self.root),
			Err(error) => Err(DisconnectFailed { root: self, error }),
		}
	}
}

/// A live cfapi connection. Dropping it while connected disconnects.
struct Connection {
	key: i64,
	slot: &'static ContextSlot,
	connected: bool,
}

impl Connection {
	/// Disconnects, and on success empties the slot, dropping its dispatcher.
	fn disconnect(&mut self) -> Result<(), WinError> {
		check(CfCall::Disconnect, ffi::disconnect(self.key))?;
		self.connected = false;
		self.slot.clear();
		Ok(())
	}
}

impl fmt::Debug for Connection {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Connection")
			.field("key", &self.key)
			.field("connected", &self.connected)
			.finish_non_exhaustive()
	}
}

impl Drop for Connection {
	fn drop(&mut self) {
		if !self.connected {
			return;
		}
		if let Err(error) = self.disconnect() {
			warn!(target: LOG_TARGET, %error, "disconnecting a dropped sync root failed");
			// Callbacks still arriving find no dispatcher and fail their requests.
			self.slot.clear();
		}
	}
}

/// What a connection's callbacks reach: the dispatcher and the local engine, behind a trait so the
/// slot holds any drive's.
pub(super) trait Requests: Send + Sync + 'static {
	/// A `FETCH_DATA` request.
	fn fetch_data(&self, request: FetchDataRequest, responder: WinData);
	/// A `FETCH_PLACEHOLDERS` request.
	fn fetch_placeholders(&self, request: PopulateRequest, responder: WinPopulate);
	/// A `CANCEL_FETCH_DATA` request.
	fn cancel_fetch_data(&self, cancel: CancelRequest);
	/// A `NOTIFY_DELETE` request.
	fn notify_delete(&self, request: DeleteRequest, responder: WinDelete);
	/// A `NOTIFY_RENAME` request.
	fn notify_rename(&self, request: RenameRequest, responder: WinRename);
	/// A `NOTIFY_DEHYDRATE` request.
	fn notify_dehydrate(&self, request: DehydrateRequest, responder: WinDehydrate);
	/// A delete or rename the platform reports finished.
	fn completed(&self, completed: Completed);
	/// A path a notification says to look at.
	fn local_event(&self, event: LocalEvent);
}

/// What one connection serves.
struct Serving<D: Drive> {
	dispatcher: Arc<Dispatcher<D>>,
	local: Arc<LocalEngine<D, WinLocalFs>>,
}

impl<D: Drive> Requests for Serving<D> {
	fn fetch_data(&self, request: FetchDataRequest, responder: WinData) {
		self.dispatcher.on_fetch_data(request, responder);
	}

	fn fetch_placeholders(&self, request: PopulateRequest, responder: WinPopulate) {
		self.dispatcher.on_fetch_placeholders(request, responder);
	}

	fn cancel_fetch_data(&self, cancel: CancelRequest) {
		self.dispatcher.on_cancel_fetch_data(cancel);
	}

	fn notify_delete(&self, request: DeleteRequest, responder: WinDelete) {
		self.local.on_notify_delete(request, responder);
	}

	fn notify_rename(&self, request: RenameRequest, responder: WinRename) {
		self.local.on_notify_rename(request, responder);
	}

	fn notify_dehydrate(&self, request: DehydrateRequest, responder: WinDehydrate) {
		self.local.on_notify_dehydrate(request, responder);
	}

	fn completed(&self, completed: Completed) {
		self.local.on_completion(completed);
	}

	fn local_event(&self, event: LocalEvent) {
		self.local.on_local_event(event);
	}
}

/// What a connection that refuses every request serves: nothing.
struct Refusing;

impl Requests for Refusing {
	fn fetch_data(&self, _request: FetchDataRequest, responder: WinData) {
		responder.fail(Failure::NotAuthenticated);
	}

	fn fetch_placeholders(&self, _request: PopulateRequest, responder: WinPopulate) {
		responder.fail(Failure::NotAuthenticated);
	}

	fn cancel_fetch_data(&self, _cancel: CancelRequest) {}

	fn notify_delete(&self, _request: DeleteRequest, responder: WinDelete) {
		responder.veto(Failure::NotAuthenticated);
	}

	fn notify_rename(&self, _request: RenameRequest, responder: WinRename) {
		responder.veto(Failure::NotAuthenticated);
	}

	fn notify_dehydrate(&self, _request: DehydrateRequest, responder: WinDehydrate) {
		responder.veto();
	}

	fn completed(&self, _completed: Completed) {}

	fn local_event(&self, _event: LocalEvent) {}
}

/// The context a connection's callbacks receive: the sync root's path, and what it serves while
/// connected, `None` after. One is leaked per connection, so a callback can always read it.
pub(super) struct ContextSlot {
	/// The sync root's path as it was registered, without its NUL: a callback's path is named
	/// below it.
	root: Box<[u16]>,
	target: RwLock<Option<Arc<dyn Requests>>>,
}

impl ContextSlot {
	/// The sync root's path.
	pub(super) fn root(&self) -> &[u16] {
		&self.root
	}

	/// What the connection serves, while connected. The lock is held only to clone the `Arc`; a
	/// poisoned lock is recovered, as the slot is only ever replaced whole.
	pub(super) fn target(&self) -> Option<Arc<dyn Requests>> {
		self.target
			.read()
			.unwrap_or_else(PoisonError::into_inner)
			.clone()
	}

	/// Empties the slot, dropping what it served after the lock is released.
	fn clear(&self) {
		let target = mem::take(&mut *self.target.write().unwrap_or_else(PoisonError::into_inner));
		drop(target);
	}
}
