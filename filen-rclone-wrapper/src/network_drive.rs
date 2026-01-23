use std::{borrow::Cow, path::Path, process::ExitStatus};
use sysinfo::Disks;
use tokio::{fs, process::Child};

use anyhow::{Context, Result};
use filen_sdk_rs::auth::Client;
use log::{debug, trace};

use crate::{
	rclone_installation::RcloneInstallation,
	rclone_rc_api::{RcloneApiClient, VfsListResponse},
};

pub struct NetworkDrive {
	/// The path where the network drive is mounted
	pub mount_point: String,
	/// The `rclone mount` child process. Will be killed on drop.
	pub process: Child,
	api_client: RcloneApiClient,
}

impl NetworkDrive {
	/// Mount Filen as a network drive using Rclone.
	/// Downloads Rclone binary if necessary, writes config file, and starts the `rclone mount` process.
	pub async fn mount(
		client: &Client,
		config_dir: &Path,
		mount_point: Option<&str>,
		read_only: bool,
	) -> Result<NetworkDrive> {
		let rclone = RcloneInstallation::initialize(client, config_dir).await?;
		let mount_point = resolve_mount_point(mount_point).await?;
		let cache_path = config_dir.join("network-drive-rclone/cache");

		// calculate cache size from available disk space
		let available_disk_space = get_available_disk_space(&cache_path)?;
		let os_disk_buffer: u64 = 5 * 1024 * 1024 * 1024; // 5 GiB
		let cache_size = match available_disk_space.checked_sub(os_disk_buffer) {
			Some(size) => size,
			None => available_disk_space,
		};

		// stringify args
		let cache_path = cache_path
			.to_str()
			.ok_or(anyhow::anyhow!("Failed to format cache path"))?;
		let cache_size_formatted = format!("{}Gi", cache_size);

		// construct args
		let mut args = vec![
			"mount",
			"filen:",
			&mount_point,
			"--vfs-cache-mode",
			"full",
			"--cache-dir",
			cache_path,
			"--vfs-cache-max-size",
			cache_size_formatted.as_str(),
			"--vfs-cache-min-free-space",
			"5Gi",
			"--vfs-cache-max-age",
			"720h",
			"--vfs-cache-poll-interval",
			"1m",
			"--dir-cache-time",
			"3s",
			"--cache-info-age",
			"5s",
			"--no-gzip-encoding",
			"--use-mmap",
			"--disable-http2",
			"--file-perms",
			"0666",
			"--dir-perms",
			"0777",
			"--use-server-modtime",
			"--vfs-read-chunk-size",
			"128Mi",
			"--buffer-size",
			"0",
			"--vfs-read-ahead",
			"1024Mi",
			"--vfs-read-chunk-size-limit",
			"0",
			"--no-checksum",
			"--transfers",
			"16",
			"--vfs-fast-fingerprint",
			"--devname",
			"Filen",
		];
		if read_only {
			args.push("--read-only");
		}
		let log_file_path: Option<&Path> = None; // todo: add option for log file
		if let Some(log_file_path) = log_file_path {
			args.push("--log-file");
			args.push(
				log_file_path
					.to_str()
					.ok_or(anyhow::anyhow!("Failed to format log file path"))?,
			);
		}
		#[cfg(target_family = "windows")]
		{
			args.extend_from_slice(&[
				"--volname",
				"\\\\Filen\\Filen",
				"-o",
				"FileSecurity=D:P(A;;FA;;;WD)",
				"--network-mode",
			]);
		}
		#[cfg(not(target_family = "windows"))]
		{
			args.extend_from_slice(&["--volname", "Filen"]);
		}
		let macfuse_installed = false; // todo
		if std::env::consts::OS == "macos" {
			if macfuse_installed {
				args.extend_from_slice(&["-o", "jail_symlinks"]);
			} else {
				args.extend_from_slice(&[
				/* "-o",
				"nomtime",
				"-o",
				"backend=nfs",
				"-o",
				"location=Filen",
				"-o",
				"nonamedattr", */
				// todo: should these be reintroduced? generally, check what these args are for and if they're needed
			]);
			}
		}

		debug!(
			"Starting Rclone mount process with args: {}",
			args.join(" ")
		);
		let (process, api) = rclone
			.execute_in_background(&args)
			.await
			.context("Failed to run Rclone mount process")?;

		Ok(NetworkDrive {
			mount_point: mount_point.to_string(),
			process,
			api_client: api,
		})
	}
}

async fn resolve_mount_point(mount_point: Option<&str>) -> Result<Cow<'_, str>> {
	#[cfg(windows)]
	{
		match mount_point {
			None => Ok(Cow::Borrowed("X:\\")),
			Some(mount_point) => {
				let drive_letter = regex::Regex::new(r"^([A-Z])\:?\\?$")
					.unwrap()
					.captures(mount_point);
				let Some(drive_letter) = drive_letter else {
					return Err(anyhow::anyhow!(
						"Invalid Windows mount point (not a drive letter): {}",
						mount_point
					));
				};
				let drive_letter = drive_letter.get(1).unwrap().as_str().to_uppercase();
				let drive_path = format!("{}:\\", drive_letter);
				if get_available_drive_letters()
					.await
					.context("Failed to get available drive letters")?
					.contains(&drive_path)
				{
					Ok(Cow::Owned(drive_path))
				} else {
					Err(anyhow::anyhow!(
						"Drive letter {} is not available",
						drive_letter
					))
				}
			}
		}
	}
	#[cfg(not(windows))]
	{
		let mount_point = mount_point.unwrap_or("/tmp/filen");
		if !fs::try_exists(mount_point)
			.await
			.context("Failed to check if mount point directory exists")?
		{
			fs::create_dir_all(mount_point)
				.await
				.context("Failed to create missing mount point directory")?;
		}
		Ok(Cow::Borrowed(mount_point))
	}
}

/// Lists available drive letters, e.g. `C:\` (Windows)
#[cfg(windows)]
pub async fn get_available_drive_letters() -> Result<Vec<String>> {
	let mut available_letters = Vec::new();
	for letter in "ABCDEFGHIJKLMNOPQRSTUVWXYZ".chars() {
		let drive_path = format!("{}:\\", letter);
		if !fs::try_exists(&drive_path).await? {
			available_letters.push(drive_path);
		}
	}
	Ok(available_letters)
}

fn get_available_disk_space(path: &Path) -> Result<u64> {
	Disks::new_with_refreshed_list()
		.list()
		.iter()
		.find(|disk| path.starts_with(disk.mount_point()))
		.map(|disk| disk.available_space())
		.ok_or_else(|| {
			anyhow::anyhow!(
				"Failed to get available disk space for path: {}",
				path.display()
			)
		})
}

#[derive(PartialEq)]
pub enum NetworkDriveStatus {
	/// The network drive is not accessible, e.g. during startup.
	Unavailable,
	/// The network drive is active and accessible.
	Active,
	/// The underlying rclone process has exited.
	Exited { status_code: ExitStatus },
}

impl NetworkDrive {
	pub async fn is_active(&mut self) -> NetworkDriveStatus {
		if let Ok(Some(status_code)) = self.process.try_wait() {
			return NetworkDriveStatus::Exited { status_code };
		}
		if let Ok(exists) = fs::try_exists(self.mount_point.clone()).await
			&& !exists
		{
			trace!("Mount point is inaccessible");
			return NetworkDriveStatus::Unavailable;
		}
		if !self
			.api_client
			.vfs_list()
			.await
			.unwrap_or(VfsListResponse { vfses: vec![] })
			.vfses
			.contains(&String::from("filen:"))
		{
			trace!("Rclone VFS is not active");
			return NetworkDriveStatus::Unavailable;
		}
		NetworkDriveStatus::Active
	}

	pub async fn wait_until_active(&mut self) -> Result<()> {
		let mut i = 0;
		loop {
			match self.is_active().await {
				NetworkDriveStatus::Active => return Ok(()),
				NetworkDriveStatus::Exited { status_code } => {
					return Err(anyhow::anyhow!(
						"Rclone mount process has exited with status code: {}",
						status_code
					));
				}
				_ => {
					if i >= 300 {
						return Err(anyhow::anyhow!(
							"Timed out waiting for network drive to become active"
						));
					}
					i += 1;
					tokio::time::sleep(std::time::Duration::from_millis(100)).await;
					// todo: better backoff strategy?
				}
			}
		}
	}
}

#[derive(Debug, Clone)]
pub struct NetworkDriveStats {
	pub uploads_in_progress: i32,
	pub uploads_queued: i32,
	pub errored_files: i32,
	pub transfers: Vec<NetworkDriveTransfer>,
}

#[derive(Debug, Clone)]
pub struct NetworkDriveTransfer {
	pub name: String,
	pub size: i64,
	pub speed: f64,
}

impl NetworkDrive {
	pub async fn get_stats(&self) -> Result<NetworkDriveStats> {
		let core_stats = self.api_client.core_stats().await?;
		let vfs_stats = self.api_client.vfs_stats().await?;
		Ok(NetworkDriveStats {
			uploads_in_progress: vfs_stats.disk_cache.uploads_in_progress,
			uploads_queued: vfs_stats.disk_cache.uploads_queued,
			errored_files: vfs_stats.disk_cache.errored_files,
			transfers: core_stats
				.transferring
				.unwrap_or_default()
				.into_iter()
				.map(|t| NetworkDriveTransfer {
					name: t.name,
					size: t.size,
					speed: t.speed,
				})
				.collect(),
		})
	}
}

#[cfg(windows)]
#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn test_resolve_mount_point() {
		assert_eq!(
			resolve_mount_point(Some("F:\\"))
				.await
				.unwrap()
				.into_owned(),
			"F:\\"
		);
		assert_eq!(
			resolve_mount_point(Some("Z:")).await.unwrap().into_owned(),
			"Z:\\"
		);
		assert_eq!(
			resolve_mount_point(Some("Y")).await.unwrap().into_owned(),
			"Y:\\"
		);
		assert_eq!(
			resolve_mount_point(None).await.unwrap().into_owned(),
			"X:\\"
		);
		assert!(resolve_mount_point(Some("a")).await.is_err());
		assert!(resolve_mount_point(Some("AB")).await.is_err());
		assert!(resolve_mount_point(Some("/path")).await.is_err());
	}
}
