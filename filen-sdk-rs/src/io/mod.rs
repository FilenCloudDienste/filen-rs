pub mod client_impl;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod dir_download;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod dir_upload;
pub(crate) mod fs_tree;
#[cfg(feature = "uniffi")]
mod js_impl;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod meta_ext;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod canonical_path;

use chrono::{DateTime, Utc};

pub use crate::fs::{
	dir::RemoteDirectory,
	file::{AnonymousRemoteFile, RemoteFile, traits::HasFileInfo},
};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use dir_download::{CategoryDirDownloadExtPub, DirDownloadCallback};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use dir_upload::DirUploadCallback;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use meta_ext::FilenMetaExt;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) use canonical_path::CanonicalPath;

/// Windows NT times (FILETIME) count 100 ns ticks from 1601-01-01.
const NT_TICKS_PER_SEC: u64 = 10_000_000;
/// Seconds from 1601-01-01, where NT times count from, to the Unix epoch.
const NT_EPOCH_TO_UNIX_SECS: i64 = 11_644_473_600;
const WINDOWS_TICKS_PER_MILLI: u64 = NT_TICKS_PER_SEC / 1000;
const MILLIS_TO_UNIX_EPOCH: u64 = NT_EPOCH_TO_UNIX_SECS as u64 * 1000;

// only public for tests
pub fn unix_time_to_nt_time(dt: DateTime<Utc>) -> u64 {
	let duration_since_epoch = dt.timestamp_millis() as u64 + MILLIS_TO_UNIX_EPOCH;
	duration_since_epoch * WINDOWS_TICKS_PER_MILLI
}

/// An NT time as a time, to the tick, if chrono can hold it.
#[cfg(feature = "archive")]
pub(crate) fn nt_time_to_datetime(ticks: u64) -> Option<DateTime<Utc>> {
	let secs = i64::try_from(ticks / NT_TICKS_PER_SEC).ok()? - NT_EPOCH_TO_UNIX_SECS;
	DateTime::from_timestamp(secs, (ticks % NT_TICKS_PER_SEC) as u32 * 100)
}

/// A time as an NT time, to the tick, if it is after 1601. Unlike [`unix_time_to_nt_time`], it
/// keeps sub-millisecond ticks and refuses a time NT times cannot hold.
#[cfg(feature = "archive")]
pub(crate) fn datetime_to_nt_time(time: DateTime<Utc>) -> Option<u64> {
	let secs = u64::try_from(time.timestamp().checked_add(NT_EPOCH_TO_UNIX_SECS)?).ok()?;
	secs.checked_mul(NT_TICKS_PER_SEC)?
		.checked_add(u64::from(time.timestamp_subsec_nanos() / 100))
}

#[cfg(all(test, feature = "archive"))]
mod tests {
	use super::*;

	#[test]
	fn nt_times_start_in_1601() {
		let epoch = DateTime::from_timestamp(0, 0).unwrap();
		assert_eq!(datetime_to_nt_time(epoch), Some(116_444_736_000_000_000));
		let before_1601 = DateTime::from_timestamp(-NT_EPOCH_TO_UNIX_SECS - 1, 0).unwrap();
		assert_eq!(datetime_to_nt_time(before_1601), None);
		let time = DateTime::from_timestamp(1_700_000_000, 123_456_700).unwrap();
		assert_eq!(
			nt_time_to_datetime(datetime_to_nt_time(time).unwrap()),
			Some(time)
		);
		assert_eq!(unix_time_to_nt_time(epoch), 116_444_736_000_000_000);
	}
}
