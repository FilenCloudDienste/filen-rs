use std::{fs::FileTimes, path::Path, time::SystemTime};

use chrono::{DateTime, SubsecRound, Utc};

use crate::io::{HasFileInfo, RemoteDirectory, RemoteFile};

#[cfg(windows)]
// thanks Microsoft!
fn nt_time_to_unix_time(nt_time: u64) -> DateTime<Utc> {
	if nt_time == 0 {
		return std::time::SystemTime::UNIX_EPOCH.into();
	}
	let unix_millis = nt_time / super::WINDOWS_TICKS_PER_MILLI - super::MILLIS_TO_UNIX_EPOCH;
	(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(unix_millis)).into()
}

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

#[cfg(unix)]
fn secs_and_nsecs_to_chrono_time(secs: i64, nsecs: i64) -> DateTime<Utc> {
	// default to 0 on conversion errors, nsecs should never be negative or more than 999,999,999
	DateTime::<Utc>::from_timestamp(secs, nsecs.try_into().unwrap_or_default())
		.unwrap_or_default()
		.round_subsecs(3)
}

impl FilenMetaExt for std::fs::Metadata {
	fn size(&self) -> u64 {
		#[cfg(windows)]
		return std::os::windows::fs::MetadataExt::file_size(self);
		#[cfg(unix)]
		return std::os::unix::fs::MetadataExt::size(self);
	}

	fn modified(&self) -> DateTime<Utc> {
		#[cfg(windows)]
		return nt_time_to_unix_time(std::os::windows::fs::MetadataExt::last_write_time(self));
		#[cfg(unix)]
		return secs_and_nsecs_to_chrono_time(
			std::os::unix::fs::MetadataExt::mtime(self),
			std::os::unix::fs::MetadataExt::mtime_nsec(self),
		);
	}

	fn created(&self) -> DateTime<Utc> {
		match std::fs::Metadata::created(self) {
			Ok(creation) => chrono::DateTime::<Utc>::from(creation).round_subsecs(3),
			Err(_) => FilenMetaExt::modified(self),
		}
	}

	fn accessed(&self) -> DateTime<Utc> {
		#[cfg(windows)]
		return nt_time_to_unix_time(std::os::windows::fs::MetadataExt::last_access_time(self));
		#[cfg(unix)]
		return secs_and_nsecs_to_chrono_time(
			std::os::unix::fs::MetadataExt::atime(self),
			std::os::unix::fs::MetadataExt::atime_nsec(self),
		);
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

// underscore so clippy doesn't complain on linux about unused variable
fn get_file_times(_created: Option<SystemTime>, modified: Option<SystemTime>) -> FileTimes {
	#[cfg(target_vendor = "apple")]
	use std::os::darwin::fs::FileTimesExt;
	#[cfg(windows)]
	use std::os::windows::fs::FileTimesExt;
	let mut times = FileTimes::new();
	#[cfg(any(target_vendor = "apple", windows))]
	{
		if let Some(created) = _created {
			times = times.set_created(created);
		}
	}

	if let Some(modified) = modified {
		times = times.set_modified(modified);
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
		println!("Setting times for dir {:?}", &tmp_dir);

		set_file_times_for_dir(&tmp_dir, file_times).unwrap();

		let metadata = std::fs::metadata(&tmp_dir).unwrap();
		#[cfg(any(target_vendor = "apple", windows))]
		{
			assert_eq!(FilenMetaExt::created(&metadata), dt_created);
		}
		assert_eq!(FilenMetaExt::modified(&metadata), dt_modified);

		let file_path = tmp_dir.join("test_file.txt");
		let file = std::fs::File::create(&file_path).unwrap();

		file.set_times(file_times).unwrap();
		let metadata = std::fs::metadata(&file_path).unwrap();
		#[cfg(any(target_vendor = "apple", windows))]
		{
			assert_eq!(FilenMetaExt::created(&metadata), dt_created);
		}
		assert_eq!(FilenMetaExt::modified(&metadata), dt_modified);
	}
}
