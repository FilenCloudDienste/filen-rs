//! rclone, run as a child process for network drive mounts and `rclone serve`.
//!
//! [`RcloneDaemon`] runs one `rclone rcd` and drives it over its remote control API.
//! `rclone-managed` adds the managed installation filen-cli uses: it downloads a pinned rclone
//! release (`RcloneBinary::Managed`), writes an `rclone.conf` for the client's `filen`
//! remote, and runs `rclone mount` (a network drive) or `rclone serve` as a child process.

mod binary;
mod daemon;
#[cfg(feature = "rclone-managed")]
pub mod network_drive;
mod private_file;
mod rc;
#[cfg(feature = "rclone-managed")]
pub mod rclone_installation;
#[cfg(feature = "rclone-managed")]
mod rclone_rc_api;
mod recovery;
mod refusal;
#[cfg(feature = "rclone-managed")]
pub mod serve;
mod system;
mod watchdog;

pub use binary::RcloneBinary;
pub use daemon::{
	RcloneDaemon, RcloneDaemonConfig, RcloneExit, RcloneStats, RcloneTransfer, StopOptions,
	StopReport,
};
pub use recovery::{StaleRecovery, force_unmount, recover_stale, stop_rclone_under};
pub use refusal::RcloneRefusal;
