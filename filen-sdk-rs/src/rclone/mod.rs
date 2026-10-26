//! rclone, run as a child process for network drive mounts and `rclone serve`.
//!
//! `rclone-managed` adds the managed installation filen-cli uses: it downloads a pinned rclone
//! release (`RcloneBinary::Managed`), writes an `rclone.conf` for the client's `filen`
//! remote, and runs `rclone mount` (a network drive) or `rclone serve` as a child process.

mod binary;
#[cfg(feature = "rclone-managed")]
pub mod network_drive;
#[cfg(feature = "rclone-managed")]
pub mod rclone_installation;
#[cfg(feature = "rclone-managed")]
mod rclone_rc_api;
mod refusal;
#[cfg(feature = "rclone-managed")]
pub mod serve;

pub use binary::RcloneBinary;
pub use refusal::RcloneRefusal;
