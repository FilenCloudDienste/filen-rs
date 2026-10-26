//! Mounting the daemon's remotes as network drives over rc.
//!
//! A mount uses the system's backend ([`fuse_status`]), `Filen:` (or `FilenRO:` when read-only)
//! with the VFS options every role shares, and is recorded in the daemon's pidfile and watchdog
//! before it is reported, so a crashed host's mount is taken down with its rclone.

#[cfg(unix)]
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;
use tracing::warn;

#[cfg(unix)]
use crate::rclone::recovery::{MountState, force_unmount, mount_state};
use crate::{
	Error, ErrorKind,
	rclone::{
		RcloneDaemon, RcloneRefusal,
		daemon::{FILEN_READ_ONLY_REMOTE, FILEN_REMOTE, stopped},
		fuse::{FuseProvider, FuseStatus, fuse_status},
		vfs::vfs_opt,
	},
};

/// How long a mount has to appear once rc has mounted it.
const MOUNTED_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a mount has to go once rc has unmounted it, before it is forced off.
const UNMOUNTED_TIMEOUT: Duration = Duration::from_secs(5);
const MOUNT_POLL: Duration = Duration::from_millis(100);

/// What to mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountOptions {
	/// On Unix an existing empty directory, by an absolute path that is not a symlink. On Windows
	/// a free drive letter: `X`, `X:` or `X:\`.
	pub mount_point: String,
	/// Whether the drive is read-only. It then uses its own remote and VFS cache.
	pub read_only: bool,
}

/// A mount that is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
	/// Where it is mounted: the directory, or on Windows the drive, as `X:`.
	pub mount_point: String,
	/// The backend it is mounted with.
	pub provider: FuseProvider,
}

impl RcloneDaemon {
	/// Mounts the drive at `options.mount_point` and waits up to 30 s for it to appear.
	///
	/// Refused with [`RcloneRefusal::FuseMissing`] without a mount backend, and on a mount point
	/// that is unusable ([`RcloneRefusal::MountPointInvalid`]), not empty
	/// ([`RcloneRefusal::MountPointNotEmpty`]), already mounted
	/// ([`RcloneRefusal::MountPointInUse`]) or, on Windows, a drive letter that is taken
	/// ([`RcloneRefusal::DriveLetterInUse`]). A stale mount left on the mount point by a crashed
	/// rclone is forced off first. Mounts and unmounts run one at a time.
	pub async fn mount(&self, options: MountOptions) -> Result<MountInfo, Error> {
		let provider = match fuse_status().await {
			FuseStatus::Ready { provider, .. } => provider,
			FuseStatus::Missing { .. } => return Err(RcloneRefusal::FuseMissing.into()),
		};
		let mut guard = self.guard.lock().await;
		let guard = guard.as_mut().ok_or_else(stopped)?;
		let mount_point = usable_mount_point(&options.mount_point, &guard.record.mounts).await?;
		let fs = format!(
			"{}:",
			if options.read_only {
				FILEN_READ_ONLY_REMOTE
			} else {
				FILEN_REMOTE
			}
		);
		self.rc
			.mount_mount(
				&fs,
				&mount_point,
				provider.mount_type(),
				&vfs_opt(&self.vfs, options.read_only),
				&mount_opt(provider),
			)
			.await?;
		let recorded = async {
			wait_mounted(&mount_point).await?;
			// One copy for the record, one for the caller.
			guard.record.mounts.push(mount_point.clone());
			guard.record_mounts(&self.pidfile)
		}
		.await;
		if let Err(e) = recorded {
			guard.record.mounts.retain(|known| *known != mount_point);
			if let Err(unmount) = self.rc.mount_unmount(&mount_point).await {
				warn!("unmounting {mount_point} after a failed mount failed: {unmount}");
			}
			return Err(e);
		}
		Ok(MountInfo {
			mount_point,
			provider,
		})
	}

	/// Unmounts the drive this daemon mounted at `mount_point`, forcing it off when it does not go
	/// within 5 s.
	pub async fn unmount(&self, mount_point: &str) -> Result<(), Error> {
		let mut guard = self.guard.lock().await;
		let guard = guard.as_mut().ok_or_else(stopped)?;
		if !guard.record.mounts.iter().any(|known| known == mount_point) {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!("{mount_point} is not mounted by this rclone daemon"),
			));
		}
		if let Err(e) = self.rc.mount_unmount(mount_point).await {
			warn!("rclone could not unmount {mount_point}, forcing it off: {e}");
		}
		wait_unmounted(mount_point).await?;
		guard.record.mounts.retain(|known| known != mount_point);
		guard.record_mounts(&self.pidfile)
	}
}

/// rc's `mountOpt` for `provider`, the old desktop client's per-platform options.
fn mount_opt(provider: FuseProvider) -> Value {
	match provider {
		FuseProvider::MacFuse => json!({
			"DeviceName": "Filen",
			"VolumeName": "Filen",
			"NoAppleXattr": true,
			"ExtraOptions": ["jail_symlinks"],
		}),
		FuseProvider::NfsMount | FuseProvider::Fuse3 => json!({
			"DeviceName": "Filen",
			"VolumeName": "Filen",
		}),
		FuseProvider::WinFsp => json!({
			"DeviceName": "Filen",
			"VolumeName": r"\\Filen\Filen",
			"NetworkMode": true,
			"ExtraOptions": [
				"FileSecurity=D:P(A;;FA;;;WD)",
				"FileInfoTimeout=5000",
				"DirInfoTimeout=5000",
			],
		}),
	}
}

/// `mount_point` once it is known to be usable, in the form rc mounts it at. `mounted` are the
/// daemon's own mounts.
#[cfg(unix)]
async fn usable_mount_point(mount_point: &str, mounted: &[String]) -> Result<String, Error> {
	let invalid = |why: &str| {
		Error::from(RcloneRefusal::MountPointInvalid).with_context(format!("{mount_point}: {why}"))
	};
	let path = Path::new(mount_point);
	// A control character would also break the pidfile's and the watchdog's lists of mounts.
	if !path.is_absolute() || mount_point.chars().any(char::is_control) {
		return Err(invalid("not an absolute path"));
	}
	if mounted.iter().any(|known| known == mount_point) {
		return Err(RcloneRefusal::MountPointInUse.into());
	}
	// First, and with a timeout: `lstat` on a stale mount blocks.
	match mount_state(path).await? {
		MountState::NotMounted => {}
		MountState::Mounted => return Err(RcloneRefusal::MountPointInUse.into()),
		MountState::Stale => {
			warn!("forcing the stale mount on {mount_point} off before mounting there");
			force_unmount(path).await?;
		}
	}
	let metadata = match tokio::fs::symlink_metadata(path).await {
		Ok(metadata) => metadata,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(invalid("missing")),
		Err(e) => return Err(Error::from(e).with_context(format!("failed to stat {mount_point}"))),
	};
	if metadata.file_type().is_symlink() || !metadata.is_dir() {
		return Err(invalid("not a directory"));
	}
	let mut entries = tokio::fs::read_dir(path)
		.await
		.map_err(|e| Error::from(e).with_context(format!("failed to list {mount_point}")))?;
	if entries
		.next_entry()
		.await
		.map_err(|e| Error::from(e).with_context(format!("failed to list {mount_point}")))?
		.is_some()
	{
		return Err(RcloneRefusal::MountPointNotEmpty.into());
	}
	Ok(mount_point.to_owned())
}

#[cfg(windows)]
async fn usable_mount_point(mount_point: &str, mounted: &[String]) -> Result<String, Error> {
	let letter = drive_letter(mount_point).ok_or_else(|| {
		Error::from(RcloneRefusal::MountPointInvalid)
			.with_context(format!("{mount_point}: not a drive letter"))
	})?;
	let drive = format!("{letter}:");
	let in_use = mounted.iter().any(|known| *known == drive)
		|| tokio::fs::try_exists(format!(r"{drive}\"))
			.await
			.unwrap_or(true);
	if in_use {
		return Err(RcloneRefusal::DriveLetterInUse.into());
	}
	Ok(drive)
}

/// The drive letter of `X`, `X:` or `X:\`, upper case only.
#[cfg(any(windows, test))]
fn drive_letter(mount_point: &str) -> Option<char> {
	let mut chars = mount_point.chars();
	let letter = chars.next().filter(char::is_ascii_uppercase)?;
	match chars.as_str() {
		"" | ":" | r":\" => Some(letter),
		_ => None,
	}
}

/// Waits until `mount_point` is a mount, failing after [`MOUNTED_TIMEOUT`].
async fn wait_mounted(mount_point: &str) -> Result<(), Error> {
	let deadline = Instant::now() + MOUNTED_TIMEOUT;
	while !is_mounted(mount_point).await? {
		if Instant::now() >= deadline {
			return Err(Error::custom(
				ErrorKind::IO,
				format!("{mount_point} was not mounted within {MOUNTED_TIMEOUT:?}"),
			));
		}
		tokio::time::sleep(MOUNT_POLL).await;
	}
	Ok(())
}

/// Waits until `mount_point` is no longer a mount, forcing it off after [`UNMOUNTED_TIMEOUT`].
async fn wait_unmounted(mount_point: &str) -> Result<(), Error> {
	let deadline = Instant::now() + UNMOUNTED_TIMEOUT;
	while is_mounted(mount_point).await? {
		if Instant::now() >= deadline {
			#[cfg(unix)]
			{
				force_unmount(Path::new(mount_point)).await?;
				return Ok(());
			}
			#[cfg(not(unix))]
			return Err(Error::custom(
				ErrorKind::IO,
				format!("{mount_point} was not unmounted within {UNMOUNTED_TIMEOUT:?}"),
			));
		}
		tokio::time::sleep(MOUNT_POLL).await;
	}
	Ok(())
}

/// Whether a mount is at `mount_point`: on Unix a directory on a device of its own (a stale one
/// counts), on Windows a drive whose root exists.
async fn is_mounted(mount_point: &str) -> Result<bool, Error> {
	#[cfg(unix)]
	{
		Ok(mount_state(Path::new(mount_point)).await? != MountState::NotMounted)
	}
	#[cfg(not(unix))]
	{
		Ok(tokio::fs::try_exists(format!(r"{mount_point}\"))
			.await
			.unwrap_or(false))
	}
}

/// The drive letters, from D to Z, no drive uses, such as `X`. Empty off Windows.
pub async fn available_drive_letters() -> Vec<String> {
	#[cfg(windows)]
	{
		let mut free = Vec::new();
		for letter in 'D'..='Z' {
			if !tokio::fs::try_exists(format!(r"{letter}:\"))
				.await
				.unwrap_or(true)
			{
				free.push(letter.to_string());
			}
		}
		free
	}
	#[cfg(not(windows))]
	Vec::new()
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::{FuseProvider, drive_letter, mount_opt};

	#[test]
	fn mount_options_per_provider() {
		assert_eq!(
			mount_opt(FuseProvider::MacFuse),
			json!({
				"DeviceName": "Filen",
				"VolumeName": "Filen",
				"NoAppleXattr": true,
				"ExtraOptions": ["jail_symlinks"],
			})
		);
		assert_eq!(
			mount_opt(FuseProvider::NfsMount),
			json!({ "DeviceName": "Filen", "VolumeName": "Filen" })
		);
		assert_eq!(
			mount_opt(FuseProvider::Fuse3),
			json!({ "DeviceName": "Filen", "VolumeName": "Filen" })
		);
		assert_eq!(
			mount_opt(FuseProvider::WinFsp),
			json!({
				"DeviceName": "Filen",
				"VolumeName": "\\\\Filen\\Filen",
				"NetworkMode": true,
				"ExtraOptions": [
					"FileSecurity=D:P(A;;FA;;;WD)",
					"FileInfoTimeout=5000",
					"DirInfoTimeout=5000",
				],
			})
		);
	}

	#[test]
	fn drive_letter_validation() {
		assert_eq!(drive_letter(r"F:\"), Some('F'));
		assert_eq!(drive_letter("Z:"), Some('Z'));
		assert_eq!(drive_letter("Y"), Some('Y'));
		assert_eq!(drive_letter("a"), None);
		assert_eq!(drive_letter("AB"), None);
		assert_eq!(drive_letter("/path"), None);
		assert_eq!(drive_letter(""), None);
		assert_eq!(drive_letter(r"X:\Filen"), None);
	}

	#[cfg(unix)]
	mod unix {
		use crate::{
			ErrorKind,
			rclone::{RcloneRefusal, mount::usable_mount_point},
		};

		async fn refusal(mount_point: &str, mounted: &[String]) -> RcloneRefusal {
			let error = usable_mount_point(mount_point, mounted).await.unwrap_err();
			assert_eq!(error.kind(), ErrorKind::IO, "{error}");
			error.downcast_ref::<RcloneRefusal>().unwrap().clone()
		}

		#[tokio::test]
		async fn an_empty_directory_is_usable() {
			let dir = tempfile::tempdir().unwrap();
			let mount_point = dir.path().to_str().unwrap();

			assert_eq!(
				usable_mount_point(mount_point, &[]).await.unwrap(),
				mount_point
			);
		}

		#[tokio::test]
		async fn non_empty_mount_point_refused() {
			let dir = tempfile::tempdir().unwrap();
			std::fs::write(dir.path().join("file"), b"x").unwrap();

			assert_eq!(
				refusal(dir.path().to_str().unwrap(), &[]).await,
				RcloneRefusal::MountPointNotEmpty
			);
		}

		#[tokio::test]
		async fn a_relative_missing_or_symlinked_mount_point_is_invalid() {
			let dir = tempfile::tempdir().unwrap();
			let target = dir.path().join("target");
			std::fs::create_dir(&target).unwrap();
			let link = dir.path().join("link");
			std::os::unix::fs::symlink(&target, &link).unwrap();
			let file = dir.path().join("file");
			std::fs::write(&file, b"x").unwrap();
			let newline = target.join("a\nb");

			for mount_point in [
				"relative/Filen".to_owned(),
				dir.path().join("missing").to_str().unwrap().to_owned(),
				link.to_str().unwrap().to_owned(),
				file.to_str().unwrap().to_owned(),
				newline.to_str().unwrap().to_owned(),
			] {
				assert_eq!(
					refusal(&mount_point, &[]).await,
					RcloneRefusal::MountPointInvalid,
					"{mount_point}"
				);
			}
		}

		#[tokio::test]
		async fn a_mount_point_the_daemon_uses_is_in_use() {
			let dir = tempfile::tempdir().unwrap();
			let mount_point = dir.path().to_str().unwrap();

			assert_eq!(
				refusal(mount_point, &[mount_point.to_owned()]).await,
				RcloneRefusal::MountPointInUse
			);
		}
	}
}
