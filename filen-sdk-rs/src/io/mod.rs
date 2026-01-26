pub mod client_impl;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod dir_download;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod dir_upload;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod fs_tree;
#[cfg(feature = "uniffi")]
mod js_impl;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod meta_ext;

pub use crate::fs::{
	dir::RemoteDirectory,
	file::{RemoteFile, traits::HasFileInfo},
};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use dir_download::DirDownloadCallback;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use dir_upload::DirUploadCallback;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use meta_ext::FilenMetaExt;

const WINDOWS_TICKS_PER_MILLI: u64 = 10_000;
const MILLIS_TO_UNIX_EPOCH: u64 = 11_644_473_600_000; // 11644473600000 milliseconds from 1601-01-01 to 1970-01-01

// only public for tests
pub fn unix_time_to_nt_time(dt: chrono::DateTime<chrono::Utc>) -> u64 {
	let duration_since_epoch = dt.timestamp_millis() as u64 + MILLIS_TO_UNIX_EPOCH;
	duration_since_epoch * WINDOWS_TICKS_PER_MILLI
}
