use std::time::Duration;

pub mod client_impl;

const WINDOWS_TICKS_PER_SECOND: u64 = 10_000_000;
const SEC_TO_UNIX_EPOCH: u64 = 11_644_473_600; // 11644473600 seconds from 1601-01-01 to 1970-01-01

pub trait FilenMetaExt {
	/// Returns the size of the file in bytes.
	fn size(&self) -> u64;
	fn modified(&self) -> std::time::SystemTime;
	fn created(&self) -> std::time::SystemTime;
}

#[cfg(windows)]
// thanks Microsoft!
fn nt_time_to_unix_time(nt_time: u64) -> std::time::SystemTime {
	if nt_time == 0 {
		return std::time::SystemTime::UNIX_EPOCH;
	}
	let unix_seconds = nt_time / WINDOWS_TICKS_PER_SECOND - SEC_TO_UNIX_EPOCH;
	std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(unix_seconds as u64)
}

impl FilenMetaExt for std::fs::Metadata {
	fn size(&self) -> u64 {
		#[cfg(windows)]
		return std::os::windows::fs::MetadataExt::file_size(self);
		#[cfg(unix)]
		return std::os::unix::fs::MetadataExt::size(self);
	}

	fn modified(&self) -> std::time::SystemTime {
		#[cfg(windows)]
		return nt_time_to_unix_time(std::os::windows::fs::MetadataExt::last_write_time(self));
		#[cfg(unix)]
		return std::time::SystemTime::UNIX_EPOCH
			+ Duration::from_secs(std::os::unix::fs::MetadataExt::mtime(self) as u64);
	}

	fn created(&self) -> std::time::SystemTime {
		#[cfg(windows)]
		return nt_time_to_unix_time(std::os::windows::fs::MetadataExt::creation_time(self));
		#[cfg(unix)]
		return FilenMetaExt::modified(self);
	}
}
