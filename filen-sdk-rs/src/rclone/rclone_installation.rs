//! Manages an installation of rclone.
//! Responsible for downloading the binary, writing config files
//! and executing Rclone commands.

use hex_literal::hex;
use port_check::free_local_ipv4_port;
use sha2::{Digest as _, Sha256};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::{
	path::{Path, PathBuf},
	process::{ExitStatus, Stdio},
};
use sysinfo::Disks;
use tokio::{
	fs,
	io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
	process::{Child, Command},
};
use tracing::{debug, info, warn};

use crate::{
	Error, ErrorKind, auth::Client, error::ResultExt as _, rclone::rclone_rc_api::RcloneApiClient,
};

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
	) -> Result<Self, Error> {
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
	pub async fn execute(&self, args: &[&str]) -> Result<ExitStatus, Error> {
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

	/// Executes an rclone command in the background, exposing the Rclone RC API and piping stdout/stderr to logs.
	pub async fn execute_in_background(
		&self,
		args: &[&str],
	) -> Result<(Child, RcloneApiClient), Error> {
		let rc_port = free_local_ipv4_port().ok_or_else(|| {
			Error::custom(ErrorKind::IO, "Failed to find free port for Rclone RC")
		})?;
		let rc_username = "filen-rclone-wrapper".to_string();
		let rc_password = {
			let random_bytes: [u8; 16] = rand::random();
			random_bytes
				.iter()
				.map(|b| format!("{:02x}", b))
				.collect::<String>()
		};
		debug!(
			"Executing rclone with args: {} --rc --rc-addr :{} --rc-min-tls-version 1.2 (RC credentials passed via environment)",
			args.join(" "),
			rc_port
		);
		let mut process = self
			.configured_rclone(args)
			// credentials go through the environment because argv is world-readable via /proc/<pid>/cmdline
			.env("RCLONE_RC_USER", &rc_username)
			.env("RCLONE_RC_PASS", &rc_password)
			.args([
				"--rc",
				"--rc-addr",
				&format!("127.0.0.1:{}", rc_port),
				"--rc-min-tls-version",
				"1.2",
			])
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true)
			.spawn()
			.context("Failed to execute rclone command")?;

		// pipe stdout and stderr to logs
		let process_stdout = process.stdout.take().ok_or_else(|| {
			Error::custom(
				ErrorKind::Internal,
				"Failed to take stdout of rclone process",
			)
		})?;
		tokio::spawn(async move {
			let mut reader = BufReader::new(process_stdout).lines();
			while let Ok(Some(line)) = reader.next_line().await {
				info!("[rclone stdout] {}", line);
			}
		});
		let process_stderr = process.stderr.take().ok_or_else(|| {
			Error::custom(
				ErrorKind::Internal,
				"Failed to take stderr of rclone process",
			)
		})?;
		tokio::spawn(async move {
			let mut reader = BufReader::new(process_stderr).lines();
			while let Ok(Some(line)) = reader.next_line().await {
				info!("[rclone stderr] {}", line);
			}
		});

		Ok((
			process,
			RcloneApiClient::new(rc_port, rc_username, rc_password),
		))
	}

	fn configured_rclone(&self, args: &[&str]) -> Command {
		let mut cmd = Command::new(&self.rclone_binary_path);
		cmd.args(["--config", self.rclone_config_path.to_str().unwrap()])
			.args(args.iter().filter(|arg| !arg.is_empty()));
		cmd
	}

	pub fn construct_cache_args(
		config_dir: &Path,
		cache_size: Option<String>,
	) -> Result<String, Error> {
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
				.ok_or_else(|| Error::custom(ErrorKind::Conversion, "Failed to format cache path"))?,
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
async fn ensure_rclone_binary(
	config_dir: &Path,
	disallow_downloading: bool,
) -> Result<PathBuf, Error> {
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
			return Err(Error::custom(
				ErrorKind::IO,
				format!(
					"Unsupported platform/architecture: {} {}",
					std::env::consts::OS,
					std::env::consts::ARCH
				),
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
			return Err(Error::custom(ErrorKind::IO, "Rclone not found"));
		}

		info!(
			"Downloading Rclone binary from {}...",
			rclone_zip_download_url
		);
		let expected_zip_checksum = match (std::env::consts::OS, std::env::consts::ARCH) {
			("windows", "x86_64") => RCLONE_CHECKSUM_WINDOWS_AMD64,
			("windows", "aarch64") => RCLONE_CHECKSUM_WINDOWS_ARM64,
			("linux", "x86_64") => RCLONE_CHECKSUM_LINUX_AMD64,
			("linux", "aarch64") => RCLONE_CHECKSUM_LINUX_ARM64,
			("macos", "x86_64") => RCLONE_CHECKSUM_OSX_AMD64,
			("macos", "aarch64") => RCLONE_CHECKSUM_OSX_ARM64,
			_ => unreachable!(),
		};
		let zip_part_path = config_dir.join(format!("{}.zip{}", rclone_zip_name, PART_SUFFIX));
		let binary_part_path = part_path(&rclone_binary_path);
		let downloaded = async {
			download_verified(
				&rclone_zip_download_url,
				&expected_zip_checksum,
				&zip_part_path,
			)
			.await?;
			extract_rclone(
				&zip_part_path,
				format!("{}/rclone{}", rclone_zip_name, file_ending),
				&binary_part_path,
			)
			.await
		}
		.await;
		// The zip is only needed to extract from, and a failed attempt's parts are never reused.
		remove_if_present(&zip_part_path).await;
		if let Err(e) = downloaded {
			remove_if_present(&binary_part_path).await;
			return Err(e);
		}
		fs::rename(&binary_part_path, &rclone_binary_path)
			.await
			.context("Failed to move the Rclone binary into place")?;
		prune_other_versions(config_dir, &rclone_binary_path).await;
	}

	debug!("Using Rclone binary at {}", rclone_binary_path.display());
	Ok(rclone_binary_path)
}

/// What a file being written is called until it is complete. Nothing reads a path with it: a
/// download or extraction cut short by a crash leaves only such a file, never a truncated binary
/// at the path that is taken as installed.
const PART_SUFFIX: &str = ".part";

fn part_path(path: &Path) -> PathBuf {
	let mut name = path.as_os_str().to_os_string();
	name.push(PART_SUFFIX);
	PathBuf::from(name)
}

/// Streams `url` into `path`, checking it against `expected_sha256` as it is written, without
/// holding the release in memory. A mismatch is an error, and the file is left for the caller to
/// remove.
async fn download_verified(
	url: &str,
	expected_sha256: &[u8; 32],
	path: &Path,
) -> Result<(), Error> {
	let mut response = reqwest::get(url)
		.await
		.context("Failed to download Rclone binary")?
		.error_for_status()
		.context("Failed to download Rclone binary")?;
	let mut file = fs::File::create(path)
		.await
		.context("Failed to create the Rclone download file")?;
	let mut hasher = Sha256::new();
	while let Some(chunk) = response
		.chunk()
		.await
		.context("Failed to read Rclone binary response")?
	{
		hasher.update(&chunk);
		file.write_all(&chunk)
			.await
			.context("Failed to write the Rclone download")?;
	}
	file.flush()
		.await
		.context("Failed to write the Rclone download")?;
	if hasher.finalize().as_slice() != expected_sha256 {
		return Err(Error::custom(
			ErrorKind::Response,
			"Downloaded Rclone zip's checksum doesn't match!",
		));
	}
	Ok(())
}

/// Extracts the zip entry `entry` of the zip at `zip_path` to `binary_path`, executable on Unix.
async fn extract_rclone(zip_path: &Path, entry: String, binary_path: &Path) -> Result<(), Error> {
	let (zip_path, binary_path) = (zip_path.to_path_buf(), binary_path.to_path_buf());
	tokio::task::spawn_blocking(move || -> Result<(), Error> {
		let zip_file =
			std::fs::File::open(&zip_path).context("Failed to open the downloaded Rclone zip")?;
		let mut zip = zip::ZipArchive::new(zip_file).map_err(|e| {
			Error::custom_with_source(ErrorKind::Response, e, Some("Failed to read Rclone zip"))
		})?;
		let mut rclone_file = zip.by_name(&entry).map_err(|e| {
			Error::custom_with_source(
				ErrorKind::Response,
				e,
				Some("Failed to find Rclone file in zip"),
			)
		})?;
		let mut options = std::fs::OpenOptions::new();
		options.write(true).create(true).truncate(true);
		#[cfg(unix)]
		options.mode(0o755);
		let mut binary = options
			.open(&binary_path)
			.context("Failed to create the Rclone binary")?;
		std::io::copy(&mut rclone_file, &mut binary).context("Failed to read Rclone file")?;
		Ok(())
	})
	.await
	.map_err(|e| {
		Error::custom_with_source(
			ErrorKind::Internal,
			e,
			Some("The Rclone extraction task failed"),
		)
	})?
}

/// Removes every other managed release from `dir`: older versions' binaries, and the parts a
/// cut-short download left. Only what a managed download names `rclone-v*` is touched; a file
/// that cannot be removed is logged and left.
async fn prune_other_versions(dir: &Path, keep: &Path) {
	let mut entries = match fs::read_dir(dir).await {
		Ok(entries) => entries,
		Err(e) => {
			warn!(
				"Failed to list {} to prune old Rclone versions: {e}",
				dir.display()
			);
			return;
		}
	};
	loop {
		let entry = match entries.next_entry().await {
			Ok(Some(entry)) => entry,
			Ok(None) => return,
			Err(e) => {
				warn!(
					"Failed to list {} to prune old Rclone versions: {e}",
					dir.display()
				);
				return;
			}
		};
		let path = entry.path();
		if path == keep || !entry.file_name().to_string_lossy().starts_with("rclone-v") {
			continue;
		}
		if let Err(e) = fs::remove_file(&path).await {
			warn!(
				"Failed to remove the old Rclone file {}: {e}",
				path.display()
			);
		}
	}
}

async fn remove_if_present(path: &Path) {
	match fs::remove_file(path).await {
		Ok(()) => {}
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
		Err(e) => warn!("Failed to remove {}: {e}", path.display()),
	}
}

async fn write_rclone_config(
	rclone_binary_path: &Path,
	client: Option<&Client>,
	config_dir: &Path,
) -> Result<PathBuf, Error> {
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

	write_private_file(&rclone_config_path, &rclone_config_content)
		.context("Failed to write Rclone config file")?;
	debug!(
		"Wrote Rclone config file at {}",
		rclone_config_path.display()
	);

	Ok(rclone_config_path)
}

async fn obscure_password_for_rclone(
	rclone_binary_path: &Path,
	password: &str,
) -> Result<String, Error> {
	// the password goes through stdin (`-`) because argv is world-readable via /proc/<pid>/cmdline
	let mut child = Command::new(rclone_binary_path)
		.args(["obscure", "-"])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.kill_on_drop(true)
		.spawn()
		.context("Failed to spawn rclone to obscure password for Rclone config")?;
	let mut stdin = child.stdin.take().ok_or_else(|| {
		Error::custom(
			ErrorKind::Internal,
			"Failed to take stdin of rclone obscure process",
		)
	})?;
	stdin
		.write_all(password.as_bytes())
		.await
		.context("Failed to write password to rclone obscure")?;
	drop(stdin);
	let output = child
		.wait_with_output()
		.await
		.context("Failed to obscure password for Rclone config")?;
	if !output.status.success() {
		return Err(Error::custom(
			ErrorKind::IO,
			format!("rclone obscure exited with {}", output.status),
		));
	}
	let obscured_password = String::from_utf8(output.stdout).map_err(|e| {
		Error::custom_with_source(
			ErrorKind::Conversion,
			e,
			Some("Failed to read obscured password for Rclone config"),
		)
	})?;
	Ok(obscured_password.trim().to_string())
}

fn get_available_disk_space(path: &Path) -> Result<u64, Error> {
	Disks::new_with_refreshed_list()
		.list()
		.iter()
		.find(|disk| path.starts_with(disk.mount_point()))
		.map(|disk| disk.available_space())
		.ok_or_else(|| {
			Error::custom(
				ErrorKind::IO,
				format!(
					"Failed to get available disk space for path: {}",
					path.display()
				),
			)
		})
}

/// Write `contents` to `path`, creating or truncating it with owner-only permissions
/// (mode `0o600` on Unix). The auth config holds the master keys, private key and API key,
/// so it must never be left group- or world-readable for other users on the machine.
fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
	use std::io::Write;

	let mut options = std::fs::OpenOptions::new();
	options.write(true).create(true).truncate(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.mode(0o600);
	}
	let mut file = options.open(path)?;
	// A pre-existing file keeps its previous mode through create+truncate, so tighten it
	// explicitly while it is still empty, before any secret bytes are written.
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
	}
	file.write_all(contents.as_bytes())
}
// copied from filen-cli/src/auth.rs

#[cfg(test)]
mod tests {
	use super::*;

	/// The managed binary's name on every platform, so a test can plant its leftovers here.
	fn every_platform_binary_name() -> Vec<String> {
		let mut names = Vec::new();
		for (os, ending) in [("linux", ""), ("osx", ""), ("windows", ".exe")] {
			for arch in ["amd64", "arm64"] {
				names.push(format!("rclone-v{RCLONE_VERSION}-{os}-{arch}{ending}"));
			}
		}
		names
	}

	#[tokio::test]
	async fn a_part_file_left_by_a_crash_is_never_taken_as_the_binary() {
		let dir = tempfile::tempdir().unwrap();
		for name in every_platform_binary_name() {
			std::fs::write(
				dir.path().join(format!("{name}{PART_SUFFIX}")),
				b"truncated",
			)
			.unwrap();
		}

		let error = ensure_rclone_binary(dir.path(), true).await.unwrap_err();

		assert_eq!(error.kind(), ErrorKind::IO);
		assert_eq!(
			error.to_string(),
			"Error of kind IO: context: Rclone not found"
		);
	}

	#[tokio::test]
	async fn pruning_keeps_the_current_binary_and_what_is_not_a_managed_release() {
		let dir = tempfile::tempdir().unwrap();
		let keep = dir
			.path()
			.join(format!("rclone-v{RCLONE_VERSION}-osx-arm64"));
		for name in [
			format!("rclone-v{RCLONE_VERSION}-osx-arm64"),
			format!("rclone-v{RCLONE_VERSION}-osx-arm64{PART_SUFFIX}"),
			format!("rclone-v{RCLONE_VERSION}-osx-arm64.zip{PART_SUFFIX}"),
			"rclone-v1.0.0-osx-arm64".to_string(),
			"rclone.conf".to_string(),
		] {
			std::fs::write(dir.path().join(name), b"x").unwrap();
		}

		prune_other_versions(dir.path(), &keep).await;

		let mut left = std::fs::read_dir(dir.path())
			.unwrap()
			.map(|entry| entry.unwrap().file_name().into_string().unwrap())
			.collect::<Vec<_>>();
		left.sort();
		assert_eq!(
			left,
			[
				format!("rclone-v{RCLONE_VERSION}-osx-arm64"),
				"rclone.conf".to_string()
			]
		);
	}
}
