use hex_literal::hex;
use port_check::free_local_ipv4_port;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::{
	borrow::Cow,
	path::{Path, PathBuf},
	process::{ExitStatus, Stdio},
};
use sysinfo::Disks;
use tokio::{
	fs,
	io::{AsyncBufReadExt, BufReader},
	process::{Child, Command},
};

use anyhow::{Context, Result};
use filen_sdk_rs::auth::Client;
use log::{debug, info, trace};

use crate::rclone_rc_api::{RcloneApiClient, VfsListResponse};

mod rclone_rc_api;

pub struct MountedNetworkDrive {
	/// The path where the network drive is mounted
	pub mount_point: String,
	/// The `rclone mount` child process. Will be killed on drop.
	pub process: Child,
	api_client: RcloneApiClient,
}

/// Mount Filen as a network drive using Rclone.
/// Downloads Rclone binary if necessary, writes config file, and starts the `rclone mount` process.
pub async fn mount_network_drive(
	client: &Client,
	config_dir: &Path,
	mount_point: Option<&str>,
	read_only: bool,
) -> Result<MountedNetworkDrive> {
	let rclone_binary_path = ensure_rclone_binary(config_dir).await?;
	let rclone_config_path = write_rclone_config(&rclone_binary_path, client, config_dir).await?;
	let mount_point = resolve_mount_point(mount_point).await?;
	let rc_port =
		free_local_ipv4_port().ok_or(anyhow::anyhow!("Failed to find free port for Rclone RC"))?;
	let process = start_rclone_mount_process(
		&config_dir.join("network-drive-rclone/cache"),
		&rclone_binary_path,
		&rclone_config_path,
		&mount_point,
		&rc_port,
		read_only,
		None,
	)
	.await?;
	Ok(MountedNetworkDrive {
		mount_point: mount_point.to_string(),
		process,
		api_client: RcloneApiClient::new(rc_port),
	})
}

async fn resolve_mount_point(mount_point: Option<&str>) -> Result<Cow<'_, str>> {
	Ok(match std::env::consts::FAMILY {
		"windows" => match mount_point {
			None => Cow::Borrowed("X:\\"),
			Some(mount_point) => {
				let drive_letter = Regex::new(r"^([A-Z])\:?\\?$")
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
					return Ok(Cow::Owned(drive_path));
				} else {
					return Err(anyhow::anyhow!(
						"Drive letter {} is not available",
						drive_letter
					));
				}
			}
		},
		_ => {
			let mount_point = mount_point.unwrap_or("/tmp/filen");
			fs::create_dir_all(mount_point)
				.await
				.context("Failed to create mount point directory")?;
			Cow::Borrowed(mount_point)
		}
	})
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
#[cfg(not(windows))]
pub async fn get_available_drive_letters() -> Result<Vec<String>> {
	panic!("get_available_drive_letters is only supported on Windows");
}

/// Returns the path to the rclone binary, downloading it if necessary
async fn ensure_rclone_binary(config_dir: &Path) -> Result<PathBuf> {
	// determine download url
	let platform_str = match std::env::consts::OS {
		"windows" => Some("windows"),
		"linux" => Some("linux"),
		"macos" => Some("macos"),
		_ => None,
	};
	let arch_str = match std::env::consts::ARCH {
		"x86_64" => Some("amd64"),
		"aarch64" => Some("arm64"),
		_ => None,
	};
	let rclone_binary_download_url = match (platform_str, arch_str) {
		(Some(platform), Some(arch)) => format!(
			"https://github.com/FilenCloudDienste/filen-rclone/releases/download/v1.70.0-filen.13/rclone-v1.70.0-filen.13-{}-{}{}",
			platform,
			arch,
			if platform == "windows" { ".exe" } else { "" }
		),
		_ => {
			return Err(anyhow::anyhow!(
				"Unsupported platform/architecture: {} {}",
				std::env::consts::OS,
				std::env::consts::ARCH
			));
		}
	};

	let rclone_binary_dir = config_dir.join("network-drive-rclone");
	let rclone_file_name = rclone_binary_download_url.rsplit_once('/').unwrap().1;
	let rclone_binary_path = rclone_binary_dir.join(rclone_file_name);
	debug!("Rclone binary path: {}", rclone_binary_path.display());

	// download binary if it doesn't exist
	if rclone_binary_path.exists() {
		info!("Rclone binary already exists, skipping download");
	} else {
		info!(
			"Downloading Rclone binary from {}...",
			rclone_binary_download_url
		);
		let response = reqwest::get(rclone_binary_download_url)
			.await
			.context("Failed to download Rclone binary")?;
		let bytes = response
			.bytes()
			.await
			.context("Failed to read Rclone binary response")?;
		fs::create_dir_all(rclone_binary_dir)
			.await
			.context("Failed to create Rclone binary directory")?;

		// verify checksum
		let downloaded_checksum = Sha256::digest(&bytes);
		let expected_checksum = match (std::env::consts::OS, std::env::consts::ARCH) {
			("windows", "x86_64") => {
				hex!("98abf27aaef7709b828a70444a86dbecf3785339b7cc0fe2bd4109456178bb7f")
			}
			("windows", "aarch64") => {
				hex!("7cc58e796ccdbca7fe8c61fb2e452c79c8b6d7f7f805500dc5f4c484a7c34489")
			}
			("linux", "x86_64") => {
				hex!("b955ffcca0705ac9ce807b238650bdcc22a109f4c3474e77c4a163fa208a6214")
			}
			("linux", "aarch64") => {
				hex!("106ba687b5662eaad2120f3241e36c185743e4755f30069c49393c479a2c9b8f")
			}
			("macos", "x86_64") => {
				hex!("9143b88e35b105a3b4c04073fcad5008d80c65fce8a997afa2316428fd783d51")
			}
			("macos", "aarch64") => {
				hex!("67aa11b8112f293b93d02aec97cc4bb7463ebc1c775b3c31a7d9017146d2d844")
			}
			_ => unreachable!(),
		};
		if downloaded_checksum.as_slice() != expected_checksum {
			return Err(anyhow::anyhow!(
				"Downloaded Rclone binary's checksum doesn't match!"
			));
		}

		fs::write(&rclone_binary_path, &bytes).await?;

		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			let mut perms = fs::metadata(&rclone_binary_path)
				.await
				.context("Failed to get Rclone binary metadata")?
				.permissions();
			perms.set_mode(0o755);
			fs::set_permissions(&rclone_binary_path, perms)
				.await
				.context("Failed to set Rclone binary permissions")?;
		}
	}

	debug!("Using Rclone binary at {}", rclone_binary_path.display());
	Ok(rclone_binary_path)
}

async fn write_rclone_config(
	rclone_binary_path: &Path,
	client: &Client,
	config_dir: &Path,
) -> Result<PathBuf> {
	let rclone_config_dir = config_dir.join("network-drive-rlone");
	let rclone_config_path = rclone_config_dir.join("rclone.conf");

	let client_sdk_config = client.to_sdk_config();
	let rclone_config_content = format!(
		// using "internal" fields to avoid needing the plaintext password here
		// ref: https://github.com/FilenCloudDienste/filen-rclone/blob/784979078ecd573af0d9809d2c5a07bafaad2c41/backend/filen/filen.go#L136
		"[filen]\ntype = filen\npassword = {}\nemail = {}\nmaster_keys = {}\napi_key = {}\npublic_key = {}\nprivate_key = {}\nauth_version = {}\nbase_folder_uuid = {}\n",
		obscure_password_for_rclone(rclone_binary_path, "INTERNAL").await?,
		client.email(),
		client_sdk_config.master_keys.join("|"),
		obscure_password_for_rclone(rclone_binary_path, &client_sdk_config.api_key).await?,
		client_sdk_config.public_key,
		client_sdk_config.private_key,
		client_sdk_config.auth_version as u8,
		client_sdk_config.base_folder_uuid
	);

	fs::create_dir_all(&rclone_config_dir)
		.await
		.context("Failed to create Rclone config directory")?;
	fs::write(&rclone_config_path, rclone_config_content)
		.await
		.context("Failed to write Rclone config file")?;
	debug!(
		"Wrote Rclone config file at {}",
		rclone_config_path.display()
	);

	Ok(rclone_config_path)
}

async fn obscure_password_for_rclone(rclone_binary_path: &Path, password: &str) -> Result<String> {
	let obscured_password = Command::new(rclone_binary_path)
		.args(["obscure", password])
		.output()
		.await
		.context("Failed to obscure password for Rclone config")?
		.stdout;
	let obscured_password = String::from_utf8(obscured_password)
		.context("Failed to read obscured password for Rclone config")?;
	let obscured_password = obscured_password
		.strip_prefix("=== filen-rclone ===\n")
		.unwrap_or(&obscured_password)
		.trim();
	Ok(obscured_password.to_string())
}

async fn start_rclone_mount_process(
	cache_path: &Path,
	rclone_binary_path: &Path,
	rclone_config_path: &Path,
	mount_point: &str,
	rc_port: &u16,
	read_only: bool,
	log_file_path: Option<&Path>,
) -> Result<Child> {
	// calculate cache size from available disk space
	let available_disk_space = get_available_disk_space(cache_path)?;
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
	let rc_addr_formatted = format!("127.0.0.1:{}", rc_port);

	// construct args
	let mut args = vec![
		"mount",
		"filen:",
		mount_point,
		"--config",
		rclone_config_path.to_str().unwrap(),
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
		"--rc",
		"--rc-addr",
		rc_addr_formatted.as_str(),
		"--devname",
		"Filen",
	];
	if read_only {
		args.push("--read-only");
	}
	if let Some(log_file_path) = log_file_path {
		args.push("--log-file");
		args.push(
			log_file_path
				.to_str()
				.ok_or(anyhow::anyhow!("Failed to format log file path"))?,
		);
	}
	if std::env::consts::FAMILY == "windows" {
		args.extend_from_slice(&[
			"--volname",
			"\\\\Filen\\Filen",
			"-o",
			"FileSecurity=D:P(A;;FA;;;WD)",
			"--network-mode",
		]);
	} else {
		args.extend_from_slice(&["--volname", "Filen"]);
	}
	let macfuse_installed = false; // todo
	if std::env::consts::OS == "macos" {
		if macfuse_installed {
			args.extend_from_slice(&["-o", "jail_symlinks"]);
		} else {
			args.extend_from_slice(&[
				"-o",
				"nomtime",
				"-o",
				"backend=nfs",
				"-o",
				"location=Filen",
				"-o",
				"nonamedattr",
			]);
		}
	}

	debug!(
		"Starting Rclone mount process with args: {}",
		args.join(" ")
	);
	let mut process = Command::new(rclone_binary_path)
		.args(args)
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.kill_on_drop(true)
		.spawn()
		.context("Failed to start Rclone mount process")?;

	// log stdout and stderr
	let process_stdout = process.stdout.take().unwrap();
	tokio::spawn(async move {
		let mut reader = BufReader::new(process_stdout).lines();
		while let Ok(Some(line)) = reader.next_line().await {
			info!("[rclone stdout] {}", line);
		}
	});
	let process_stderr = process.stderr.take().unwrap();
	tokio::spawn(async move {
		let mut reader = BufReader::new(process_stderr).lines();
		while let Ok(Some(line)) = reader.next_line().await {
			info!("[rclone stderr] {}", line);
		}
	});

	Ok(process)
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

impl MountedNetworkDrive {
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

impl MountedNetworkDrive {
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
