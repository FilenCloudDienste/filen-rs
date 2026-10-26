//! Managed rclone: downloads a pinned rclone release, writes an `rclone.conf` for the client's
//! `filen` remote, and runs `rclone mount` (a network drive) or `rclone serve` as a child process.

pub mod network_drive;
pub mod rclone_installation;
mod rclone_rc_api;
pub mod serve;
