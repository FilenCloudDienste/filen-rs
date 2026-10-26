//! The rclone daemon for the JS bindings: the desktop app's through napi-rs, and through UniFFI,
//! whose typings napi's are checked against.
//!
//! Everything outside `uniffi_impl` and `napi_impl` is plain Rust that names no binding type, so
//! each binding can put its own attributes on the same items. The module's own types carry
//! `#[js_type]` where they are defined; [`types`] holds the mirror of the daemon's config.
//!
//! Every call runs on the commander runtime as a task of its own: rclone's supervising task lives
//! on the runtime that started it, and a JS abort drops only the wait for a call, never a stop or
//! a mount half-way.

#[cfg(feature = "napi")]
mod napi_impl;
mod types;
#[cfg(feature = "uniffi")]
mod uniffi_impl;

use std::{path::Path, sync::Arc};

pub use self::types::RcloneDaemonOptions;
use crate::{
	Error,
	auth::Client,
	rclone::{
		FuseStatus, MountInfo, MountOptions, RcloneDaemon, RcloneDaemonConfig, RcloneExit,
		RcloneRefusal, RcloneStats, ServeInfo, ServeOptions, ServeProtocol, StaleRecovery,
		StopOptions, StopReport, available_drive_letters, clear_cache, force_unmount, fuse_status,
		recover_stale, stop_rclone_under,
	},
	runtime::{do_on_commander, spawned},
};

/// What a call that panicked fails with.
const PANICKED: &str = "an rclone daemon call panicked";

/// Runs the future `call` makes on the commander runtime, as a task of its own.
async fn detached<T, F, Fut>(call: F) -> Result<T, Error>
where
	T: Send + 'static,
	F: FnOnce() -> Fut + Send + 'static,
	Fut: Future<Output = Result<T, Error>> + Send + 'static,
{
	do_on_commander(move || spawned(call(), PANICKED)).await
}

/// The rclone refusal `error` carries, if it is one.
pub(crate) fn rclone_refusal(error: &Error) -> Option<RcloneRefusal> {
	error.downcast_ref::<RcloneRefusal>().cloned()
}

/// A running `rclone rcd`. Dropping (or garbage-collecting) the last handle kills rclone; `stop`
/// stops it gracefully.
// `Clone` for napi, whose async methods move a clone of the receiver into the promise: it shares
// the daemon, so a clone lives only while its call is in flight.
#[derive(Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct JsRcloneDaemon {
	daemon: Arc<RcloneDaemon>,
	/// The commander runtime the daemon was started on, which napi stops it on when its Node
	/// environment goes away. UniFFI has no use for it, as its daemons stop when dropped.
	#[cfg_attr(feature = "uniffi", allow(dead_code))]
	runtime: tokio::runtime::Handle,
}

impl JsRcloneDaemon {
	/// Starts a daemon for `client`'s account as `options` say. Every binding's start calls this.
	async fn start(client: Arc<Client>, options: RcloneDaemonOptions) -> Result<Self, Error> {
		let config = RcloneDaemonConfig::try_from(options)?;
		detached(move || async move {
			let daemon = RcloneDaemon::start(&client, config).await?;
			Ok(Self {
				daemon: Arc::new(daemon),
				runtime: tokio::runtime::Handle::current(),
			})
		})
		.await
	}
}

#[filen_macros::js_exports]
// On napi these are reached only through the wrappers napi-derive registers, which it does only
// outside `cfg(test)`.
#[cfg_attr(all(test, feature = "napi"), allow(dead_code))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl JsRcloneDaemon {
	/// rclone's process id.
	#[napi_export]
	pub fn pid(&self) -> u32 {
		self.daemon.pid()
	}

	/// Resolves once rclone has exited: `Stopped` after `stop`, `Crashed` with its exit code and
	/// the last lines of its stderr when it exited by itself.
	#[napi_export]
	pub async fn exited(&self) -> RcloneExit {
		self.daemon.exited().await
	}

	/// What the VFS caches, transfers, mounts and serves are doing now.
	#[napi_export]
	pub async fn stats(&self) -> Result<RcloneStats, Error> {
		let daemon = Arc::clone(&self.daemon);
		detached(move || async move { daemon.stats().await }).await
	}

	/// Mounts the drive and waits up to 30 s for it to appear (see [`RcloneDaemon::mount`]).
	#[napi_export]
	pub async fn mount(&self, options: MountOptions) -> Result<MountInfo, Error> {
		let daemon = Arc::clone(&self.daemon);
		detached(move || async move { daemon.mount(options).await }).await
	}

	/// Unmounts the drive this daemon mounted at `mount_point`.
	#[napi_export]
	pub async fn unmount(&self, mount_point: String) -> Result<(), Error> {
		let daemon = Arc::clone(&self.daemon);
		detached(move || async move { daemon.unmount(&mount_point).await }).await
	}

	/// Serves the drive once the serve is checked to refuse a request without credentials (see
	/// [`RcloneDaemon::start_serve`]).
	#[napi_export]
	pub async fn start_serve(&self, options: ServeOptions) -> Result<ServeInfo, Error> {
		let daemon = Arc::clone(&self.daemon);
		detached(move || async move { daemon.start_serve(options).await }).await
	}

	/// Stops the serve with `id`.
	#[napi_export]
	pub async fn stop_serve(&self, id: String) -> Result<(), Error> {
		let daemon = Arc::clone(&self.daemon);
		detached(move || async move { daemon.stop_serve(&id).await }).await
	}

	/// The protocols this rclone serves here.
	#[napi_export]
	pub async fn serve_types(&self) -> Result<Vec<ServeProtocol>, Error> {
		let daemon = Arc::clone(&self.daemon);
		detached(move || async move { daemon.serve_types().await }).await
	}

	/// Stops rclone gracefully (see [`RcloneDaemon::stop`]). Every other call fails afterwards.
	#[napi_export]
	pub async fn stop(&self, options: StopOptions) -> Result<StopReport, Error> {
		let daemon = Arc::clone(&self.daemon);
		detached(move || async move { daemon.stop(options).await }).await
	}
}

/// The mount backend this system has.
// On napi this and the functions below are reached only through the functions napi-derive
// registers outside `cfg(test)`.
#[cfg_attr(all(test, feature = "napi"), allow(dead_code))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub async fn rclone_fuse_status() -> Result<FuseStatus, Error> {
	detached(|| async { Ok(fuse_status().await) }).await
}

/// Recovers from a daemon on `state_dir` whose host ended without stopping it (see
/// [`recover_stale`]).
#[cfg_attr(all(test, feature = "napi"), allow(dead_code))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub async fn rclone_recover_stale(state_dir: String) -> Result<StaleRecovery, Error> {
	detached(move || async move { recover_stale(Path::new(&state_dir)).await }).await
}

/// Stops every process whose executable is inside `dir`, killing it after `grace_ms`; returns how
/// many it stopped (see [`stop_rclone_under`]).
#[cfg_attr(all(test, feature = "napi"), allow(dead_code))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub async fn rclone_stop_under(dir: String, grace_ms: u32) -> Result<u32, Error> {
	detached(move || async move { stop_rclone_under(Path::new(&dir), grace_ms).await }).await
}

/// Unmounts whatever is mounted at `path`, stale or busy; returns whether something was (see
/// [`force_unmount`]).
#[cfg_attr(all(test, feature = "napi"), allow(dead_code))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub async fn rclone_force_unmount(path: String) -> Result<bool, Error> {
	detached(move || async move { force_unmount(Path::new(&path)).await }).await
}

/// The drive letters, from D to Z, no drive uses, such as `X`. Empty off Windows.
#[cfg_attr(all(test, feature = "napi"), allow(dead_code))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub async fn rclone_available_drive_letters() -> Result<Vec<String>, Error> {
	detached(|| async { Ok(available_drive_letters().await) }).await
}

/// Deletes the VFS caches in `cache_dir`, unless the daemon on `state_dir` runs or the cache holds
/// files not uploaded yet (see [`clear_cache`]).
#[cfg_attr(all(test, feature = "napi"), allow(dead_code))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub async fn rclone_clear_cache(state_dir: String, cache_dir: String) -> Result<(), Error> {
	detached(move || async move { clear_cache(Path::new(&state_dir), Path::new(&cache_dir)).await })
		.await
}
