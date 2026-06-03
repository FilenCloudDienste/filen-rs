//! Manages an installation of rclone.
//! Responsible for downloading the binary, writing config files
//! and executing Rclone commands.

use anyhow::{Context, Result};
use filen_sdk_rs::auth::Client;
use hex_literal::hex;
use log::{debug, info};
use port_check::free_local_ipv4_port;
use sha2::{Digest as _, Sha256};
use std::{
	io::Read,
	path::{Path, PathBuf},
	process::{ExitStatus, Stdio},
};
use sysinfo::Disks;
use tokio::{
	fs,
	io::{AsyncBufReadExt as _, BufReader},
	process::{Child, Command},
};

use crate::rclone_rc_api::RcloneApiClient;

pub struct RcloneInstallationConfig {
	/// Where the Rclone config and other files (cache, ...) will be stored
	pub config_dir: PathBuf,
	/// Where the Rclone binary will be stored (can be separate)
	pub rclone_binary_dir: PathBuf,
}

impl RcloneInstallationConfig {
	pub fn new(config_dir: &Path) -> Self {
		Self {
			config_dir: config_dir.to_path_buf(),
			rclone_binary_dir: config_dir.to_path_buf(),
		}
	}
}

pub struct RcloneInstallation {
	rclone_binary_path: PathBuf,
	rclone_config_path: PathBuf,
}

impl RcloneInstallation {
	/// Checks if Rclone is already downloaded. If not, it will be downloaded on initialization.
	pub async fn check_already_downloaded(config: &RcloneInstallationConfig) -> bool {
		ensure_rclone_binary(&config.rclone_binary_dir, true)
			.await
			.is_ok()
	}

	/// Initializes Rclone, downloading the binary if necessary.
	/// If `client` is provided, an authenticated Filen remote "filen" is configured.
	pub async fn initialize(
		config: &RcloneInstallationConfig,
		client: Option<&Client>,
	) -> Result<Self> {
		fs::create_dir_all(&config.config_dir)
			.await
			.context("Failed to create Rclone installation directory")?;
		let rclone_binary_path = ensure_rclone_binary(&config.rclone_binary_dir, false).await?;
		let rclone_config_path =
			write_rclone_config(&rclone_binary_path, client, &config.config_dir).await?;
		Ok(Self {
			rclone_binary_path,
			rclone_config_path,
		})
	}

	/// Executes an rclone command and waits for it to finish.
	pub async fn execute(&self, args: &[&str]) -> Result<ExitStatus> {
		debug!("Executing rclone with args: {}", args.join(" "));
		let status = self
			.configured_rclone(args)
			.kill_on_drop(true)
			.spawn()
			.context("Failed to execute rclone command")?
			.wait()
			.await
			.context("Failed to wait for rclone command")?;
		Ok(status)
	}

	/// Executes an rclone command in the background, exposing the Rclone RC API.
	pub async fn execute_in_background(&self, args: &[&str]) -> Result<(Child, RcloneApiClient)> {
		let rc_port = free_local_ipv4_port()
			.ok_or(anyhow::anyhow!("Failed to find free port for Rclone RC"))?;
		debug!(
			"Executing rclone with args: {} --rc --rc-no-auth --rc-addr 127.0.0.1:{}",
			args.join(" "),
			rc_port
		);
		let process = self
			.configured_rclone(args)
			.args([
				"--rc",
				"--rc-no-auth", // otherwise, certain endpoints are inaccessible
				"--rc-addr",
				&format!("127.0.0.1:{}", rc_port),
			])
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true)
			.spawn()
			.context("Failed to execute rclone command")?;

		Ok((process, RcloneApiClient::new(rc_port)))
	}

	pub fn pipe_output_to_logs(process: &mut Child) {
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
	}

	fn configured_rclone(&self, args: &[&str]) -> Command {
		let mut cmd = Command::new(&self.rclone_binary_path);
		cmd.args(["--config", self.rclone_config_path.to_str().unwrap()])
			.args(args.iter().filter(|arg| !arg.is_empty()));
		cmd
	}

	pub fn construct_cache_args(config_dir: &Path, cache_size: Option<String>) -> Result<String> {
		let cache_path = config_dir.join("network-drive-rclone/cache");

		let cache_size = match cache_size {
			Some(cache_size) => cache_size,
			None => {
				// calculate cache size from available disk space
				let available_disk_space = get_available_disk_space(&cache_path)?;
				let os_disk_buffer: u64 = 5 * 1024 * 1024 * 1024; // 5 GiB
				let cache_size = match available_disk_space.checked_sub(os_disk_buffer) {
					Some(size) => size,
					None => available_disk_space,
				};
				format!("{}B", cache_size)
			}
		};

		Ok(format!(
			"--vfs-cache-mode full --cache-dir {} --vfs-cache-max-size {} --vfs-cache-min-free-space 5Gi --vfs-cache-max-age 720h --vfs-cache-poll-interval 1m --dir-cache-time 3s --cache-info-age 5s",
			cache_path
				.to_str()
				.ok_or(anyhow::anyhow!("Failed to format cache path"))?,
			cache_size
		).to_string())
	}
}

const RCLONE_VERSION: &str = "1.74.2";
const RCLONE_CHECKSUM_LINUX_AMD64: [u8; 32] =
	hex!("72a806370072015ccbe4d81bcd348cc5eaf3beca6c65ba693fd43fb31fcca5b1");
const RCLONE_CHECKSUM_LINUX_ARM64: [u8; 32] =
	hex!("bc2b2eb8269b743ed7bcea869f3782cfb4931e41efa53fc8befc6dc8308b7a50");
const RCLONE_CHECKSUM_OSX_AMD64: [u8; 32] =
	hex!("fc24831eefa3918c278c4a10be4de78288422426e2f7e64509205167f845874d");
const RCLONE_CHECKSUM_OSX_ARM64: [u8; 32] =
	hex!("e170fc4f225cbe3685695c4761259fe5883115a2b022a2f39b7298f946b8d898");
const RCLONE_CHECKSUM_WINDOWS_AMD64: [u8; 32] =
	hex!("71f376f47428bd467bf92e8bfe7fb36f4c108a4fc4edd3df30fc74dd409c7eef");
const RCLONE_CHECKSUM_WINDOWS_ARM64: [u8; 32] =
	hex!("464c8abf9eab9dab843906aac90fbd63386eb07576cd8571d03fbd10483c763e");
// info: when updating the rclone version here, also update the expected SHA256 checksums by copying them from the supplied file

/// Returns the path to the rclone binary, downloading it if necessary.
/// When `disallow_downloading` is true, returns an error "Rclone not found" instead of downloading it.
async fn ensure_rclone_binary(config_dir: &Path, disallow_downloading: bool) -> Result<PathBuf> {
	// determine download url
	let platform_str = match std::env::consts::OS {
		"windows" => Some("windows"),
		"linux" => Some("linux"),
		"macos" => Some("osx"),
		_ => None,
	};
	let arch_str = match std::env::consts::ARCH {
		"x86_64" => Some("amd64"),
		"aarch64" => Some("arm64"),
		_ => None,
	};
	let rclone_zip_name = match (platform_str, arch_str) {
		(Some(platform), Some(arch)) => format!("rclone-v{}-{}-{}", RCLONE_VERSION, platform, arch),
		_ => {
			return Err(anyhow::anyhow!(
				"Unsupported platform/architecture: {} {}",
				std::env::consts::OS,
				std::env::consts::ARCH
			));
		}
	};
	let rclone_zip_download_url = format!(
		"https://github.com/rclone/rclone/releases/download/v{}/{}.zip",
		RCLONE_VERSION, rclone_zip_name
	);
	let file_ending = match std::env::consts::OS {
		"windows" => ".exe",
		_ => "",
	};

	let rclone_binary_path = config_dir.join(format!("{}{}", rclone_zip_name, file_ending));
	debug!("Rclone binary path: {}", rclone_binary_path.display());

	// download binary if it doesn't exist
	if rclone_binary_path.exists() {
		info!("Rclone binary already exists, skipping download");
	} else {
		if disallow_downloading {
			return Err(anyhow::anyhow!("Rclone not found"));
		}

		info!(
			"Downloading Rclone binary from {}...",
			rclone_zip_download_url
		);
		let zip_bytes = reqwest::get(rclone_zip_download_url)
			.await
			.context("Failed to download Rclone binary")?
			.bytes()
			.await
			.context("Failed to read Rclone binary response")?;

		// verify zip checksum before extracting
		let zip_checksum = Sha256::digest(&zip_bytes);
		let expected_zip_checksum = match (std::env::consts::OS, std::env::consts::ARCH) {
			("windows", "x86_64") => RCLONE_CHECKSUM_WINDOWS_AMD64,
			("windows", "aarch64") => RCLONE_CHECKSUM_WINDOWS_ARM64,
			("linux", "x86_64") => RCLONE_CHECKSUM_LINUX_AMD64,
			("linux", "aarch64") => RCLONE_CHECKSUM_LINUX_ARM64,
			("macos", "x86_64") => RCLONE_CHECKSUM_OSX_AMD64,
			("macos", "aarch64") => RCLONE_CHECKSUM_OSX_ARM64,
			_ => unreachable!(),
		};
		if zip_checksum.as_slice() != expected_zip_checksum {
			return Err(anyhow::anyhow!(
				"Downloaded Rclone zip's checksum doesn't match!"
			));
		}

		let mut zip = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))
			.context("Failed to read Rclone zip")?;
		zip.file_names()
			.for_each(|name| log::info!("Zip file contains: {}", name));
		let mut rclone_file = zip
			.by_name(format!("{}/rclone{}", rclone_zip_name, file_ending).as_str())
			.context("Failed to find Rclone file in zip")?;
		let mut bytes = Vec::new();
		rclone_file
			.read_to_end(&mut bytes)
			.context("Failed to read Rclone file")?;
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
	client: Option<&Client>,
	config_dir: &Path,
) -> Result<PathBuf> {
	let rclone_config_path = config_dir.join("rclone.conf");

	let rclone_config_content = if let Some(client) = client {
		let client_sdk_config = client.to_sdk_config();
		format!(
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
		)
	} else {
		debug!("No client provided, writing empty Rclone config");
		"".to_string()
	};

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
	Ok(obscured_password.trim().to_string())
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
