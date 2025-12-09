#[cfg(target_family = "wasm")]
use core::panic;
#[cfg(not(target_family = "wasm"))]
use std::time::Duration;
use std::{fs::FileTimes, path::Path, time::SystemTime};

#[cfg(unix)]
use chrono::SubsecRound;
use chrono::{DateTime, Utc};

pub mod client_impl;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod dir_upload;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod fs_tree;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use dir_upload::DirUploadCallback;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use fs_tree::WalkError;

use crate::fs::{
	dir::RemoteDirectory,
	file::{RemoteFile, traits::HasFileInfo},
};

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

impl RemoteDirectory {
	pub(crate) fn set_dir_times(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
		let file_times = get_file_times(self.created().map(Into::into), None);
		set_file_times_for_dir(path, file_times)
	}
}

impl RemoteFile {
	pub(crate) fn get_file_times(&self) -> FileTimes {
		get_file_times(
			self.created().map(Into::into),
			self.last_modified().map(Into::into),
		)
	}
}

fn set_file_times_for_dir(path: &Path, times: FileTimes) -> Result<(), std::io::Error> {
	let dir_as_file = {
		#[cfg(windows)]
		{
			use std::os::windows::fs::OpenOptionsExt;

			std::fs::OpenOptions::new()
				.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
				.write(true)
				.open(path)?
		}
		#[cfg(unix)]
		{
			std::fs::OpenOptions::new().read(true).open(path)?
		}
	};
	dir_as_file.set_times(times)
}

#[cfg(windows)]
fn get_file_times(created: Option<SystemTime>, modified: Option<SystemTime>) -> FileTimes {
	use std::os::windows::fs::FileTimesExt;
	let mut times = FileTimes::new();
	if let Some(created) = created {
		times = times.set_created(created);
	}
	if let Some(modified) = modified {
		times = times.set_modified(modified);
	}
	times
}

#[cfg(unix)]
fn get_file_times(created: Option<SystemTime>, modified: Option<SystemTime>) -> FileTimes {
	let mut times = FileTimes::new();
	if let Some(modified) = modified {
		times = times.set_modified(modified);
	} else if let Some(created) = created {
		times = times.set_modified(created);
	}
	times
}

#[cfg(test)]
mod tests {
	use std::env;

	use chrono::TimeZone;

	use super::*;

	#[test]
	fn set_file_and_folder_times() {
		let dt_created = Utc.with_ymd_and_hms(2020, 5, 1, 12, 0, 0).unwrap();
		let dt_modified = Utc.with_ymd_and_hms(2021, 6, 2, 13, 30, 0).unwrap();

		let file_times = get_file_times(Some(dt_created.into()), Some(dt_modified.into()));
		let tmp_dir = env::temp_dir().join("test_times");
		let _ = std::fs::remove_dir_all(&tmp_dir);
		std::fs::create_dir(&tmp_dir).unwrap();

		set_file_times_for_dir(&tmp_dir, file_times).unwrap();

		let metadata = std::fs::metadata(&tmp_dir).unwrap();
		#[cfg(windows)]
		{
			assert_eq!(FilenMetaExt::created(&metadata), dt_created);
		}
		assert_eq!(FilenMetaExt::modified(&metadata), dt_modified);

		let file_path = tmp_dir.join("test_file.txt");
		let file = std::fs::File::create(&file_path).unwrap();

		file.set_times(file_times).unwrap();
		let metadata = std::fs::metadata(&file_path).unwrap();
		#[cfg(windows)]
		{
			assert_eq!(FilenMetaExt::created(&metadata), dt_created);
		}
		assert_eq!(FilenMetaExt::modified(&metadata), dt_modified);
	}
}
