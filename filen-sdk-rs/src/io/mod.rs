#[cfg(target_family = "wasm")]
use core::panic;
#[cfg(not(target_family = "wasm"))]
use std::time::Duration;

#[cfg(unix)]
use chrono::SubsecRound;
use chrono::{DateTime, Utc};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod dir_upload;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod fs_tree_builder;
// mod bulk_upload;
pub mod client_impl;

const WINDOWS_TICKS_PER_MILLI: u64 = 10_000;
const MILLIS_TO_UNIX_EPOCH: u64 = 11_644_473_600_000; // 11644473600000 milliseconds from 1601-01-01 to 1970-01-01

pub trait FilenMetaExt {
	/// Returns the size of the file in bytes.
	fn size(&self) -> u64;
	fn modified(&self) -> DateTime<Utc>;
	fn created(&self) -> DateTime<Utc>;
	fn accessed(&self) -> DateTime<Utc>;
	fn accessed_or_modified(&self) -> DateTime<Utc> {
		let accessed = self.accessed();
		if accessed.timestamp_millis() == 0 {
			self.modified()
		} else {
			accessed
		}
	}
}

#[cfg(windows)]
// thanks Microsoft!
fn nt_time_to_unix_time(nt_time: u64) -> DateTime<Utc> {
	if nt_time == 0 {
		return std::time::SystemTime::UNIX_EPOCH.into();
	}
	let unix_millis = nt_time / WINDOWS_TICKS_PER_MILLI - MILLIS_TO_UNIX_EPOCH;
	(std::time::SystemTime::UNIX_EPOCH + Duration::from_millis(unix_millis)).into()
}

// only public for tests
pub fn unix_time_to_nt_time(dt: DateTime<Utc>) -> u64 {
	let duration_since_epoch = dt.timestamp_millis() as u64 + MILLIS_TO_UNIX_EPOCH;
	duration_since_epoch * WINDOWS_TICKS_PER_MILLI
}

impl FilenMetaExt for std::fs::Metadata {
	fn size(&self) -> u64 {
		#[cfg(windows)]
		return std::os::windows::fs::MetadataExt::file_size(self);
		#[cfg(unix)]
		return std::os::unix::fs::MetadataExt::size(self);
		#[cfg(target_family = "wasm")]
		panic!("Cannot get file size on wasm32");
	}

	fn modified(&self) -> DateTime<Utc> {
		#[cfg(windows)]
		return nt_time_to_unix_time(std::os::windows::fs::MetadataExt::last_write_time(self));
		#[cfg(unix)]
		return DateTime::<Utc>::from(
			std::time::SystemTime::UNIX_EPOCH
				+ Duration::from_secs(std::os::unix::fs::MetadataExt::mtime(self) as u64)
				+ Duration::from_nanos(std::os::unix::fs::MetadataExt::mtime_nsec(self) as u64),
		)
		.round_subsecs(3);
		#[cfg(target_family = "wasm")]
		panic!("Cannot get file modified time on wasm32");
	}

	fn created(&self) -> DateTime<Utc> {
		#[cfg(windows)]
		return nt_time_to_unix_time(std::os::windows::fs::MetadataExt::creation_time(self));
		#[cfg(unix)]
		return FilenMetaExt::modified(self);
		#[cfg(target_family = "wasm")]
		panic!("Cannot get file created time on wasm32");
	}

	fn accessed(&self) -> DateTime<Utc> {
		#[cfg(windows)]
		return nt_time_to_unix_time(std::os::windows::fs::MetadataExt::last_access_time(self));
		#[cfg(unix)]
		return DateTime::<Utc>::from(
			std::time::SystemTime::UNIX_EPOCH
				+ Duration::from_secs(std::os::unix::fs::MetadataExt::atime(self) as u64)
				+ Duration::from_nanos(std::os::unix::fs::MetadataExt::atime_nsec(self) as u64),
		)
		.round_subsecs(3);
		#[cfg(target_family = "wasm")]
		panic!("Cannot get file accessed time on wasm32");
	}
}
