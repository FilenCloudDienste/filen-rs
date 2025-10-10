use port_check::free_local_ipv4_port;
use std::path::{Path, PathBuf};
use sysinfo::Disks;
use tokio::{
	fs,
	process::{Child, Command},
};

use anyhow::{Context, Result};
use filen_sdk_rs::auth::Client;
use log::{debug, info};

pub struct MountedNetworkDrive {
	/// The path where the network drive is mounted
	pub mount_point: String,
	/// The `rclone mount` child process. Will be killed on drop.
	pub process: Child,
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
	let mount_point = mount_point.unwrap_or(match std::env::consts::FAMILY {
		"windows" => "X:\\",
		_ => "/tmp/filen",
	});
	let process = start_rclone_mount_process(
		&config_dir.join("network-drive-rclone/cache"),
		&rclone_binary_path,
		&rclone_config_path,
		mount_point,
		read_only,
		None,
	)
	.await?;
	Ok(MountedNetworkDrive {
		mount_point: mount_point.to_string(),
		process,
	})
}

/// Returns the path to the rclone binary, downloading it if necessary
async fn ensure_rclone_binary(config_dir: &Path) -> Result<PathBuf> {
	// determine download url
	let rclone_binary_download_url = match std::env::consts::OS {
		"windows" => {
			"https://github.com/FilenCloudDienste/filen-rclone/releases/download/v1.70.0-filen.12/rclone-v1.70.0-filen.12-windows-arm64.exe"
		}
		// todo: add other platforms/archictures, use proper download location (GitHub or CDN?); check checksums?
		os => {
			return Err(anyhow::anyhow!("No Rclone binary for target: {}", os));
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
		fs::write(&rclone_binary_path, &bytes).await?;

		// todo: on unix, make binaries executable
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
	read_only: bool,
	log_file_path: Option<&Path>,
) -> Result<Child> {
	// calculate cache size from available disk space
	let available_disk_space = get_available_disk_space(cache_path)?;
	let os_disk_buffer = 5 * 1024 * 1024 * 1024; // 5 GiB
	let available_cache_size = (available_disk_space as i64) - os_disk_buffer;
	let cache_size = match available_cache_size {
		s if s > 0 => available_cache_size / 1024 / 1024 / 1024, // in GiB
		_ => 5,
	};

	let rclone_port =
		free_local_ipv4_port().ok_or(anyhow::anyhow!("Failed to find free port for Rclone RC"))?;

	// stringify args
	let cache_path = cache_path
		.to_str()
		.ok_or(anyhow::anyhow!("Failed to format cache path"))?;
	let cache_size_formatted = format!("{}Gi", cache_size);
	let rc_addr_formatted = format!("127.0.0.1:{}", rclone_port);

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
	let process = Command::new(rclone_binary_path)
		.args(args)
		/* .stdout(Stdio::null())
		.stderr(Stdio::null()) */
		// todo: redirect stdout so it can be logged nicer or not at all
		.kill_on_drop(true)
		.spawn()
		.context("Failed to start Rclone mount process")?;
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
