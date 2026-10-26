//! rclone, run as a child process for network drive mounts and `rclone serve`.
//!
//! [`RcloneDaemon`] runs one `rclone rcd` and drives it over its remote control API: mounts and
//! serves. `rclone-managed` adds what filen-cli uses besides: a pinned rclone release downloaded
//! into a directory (`RcloneBinary::Managed`), and `run_rclone`, which runs any rclone command in
//! the foreground with the account as its `filen` remote.

mod binary;
mod daemon;
mod fuse;
// The JS bindings' surface: the desktop one (napi), and UniFFI for the typings it is checked against.
#[cfg(any(feature = "uniffi", feature = "napi"))]
mod js_impl;
mod mount;
#[cfg(feature = "rclone-managed")]
mod passthrough;
mod private_file;
mod rc;
mod recovery;
mod refusal;
mod serve;
mod system;
mod vfs;
mod watchdog;

pub use binary::RcloneBinary;
pub use daemon::{
	RcloneDaemon, RcloneDaemonConfig, RcloneExit, RcloneStats, RcloneTransfer, StopOptions,
	StopReport,
};
pub use fuse::{FuseProvider, FuseStatus, fuse_status};
#[cfg(any(feature = "uniffi", feature = "napi"))]
pub use js_impl::RcloneDaemonOptions;
#[cfg(feature = "napi")]
pub(crate) use js_impl::{JsRcloneDaemon, rclone_refusal};
pub use mount::{MountInfo, MountOptions, available_drive_letters};
#[cfg(feature = "rclone-managed")]
pub use passthrough::run_rclone;
pub use recovery::{StaleRecovery, clear_cache, force_unmount, recover_stale, stop_rclone_under};
pub use refusal::RcloneRefusal;
pub use serve::{ServeAuth, ServeInfo, ServeOptions, ServeProtocol, ServeTls};
pub use vfs::VfsTuning;
