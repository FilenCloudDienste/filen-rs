//! Manages an installation of rclone.
//! Responsible for downloading the binary, writing config files
//! and executing Rclone commands.

use port_check::free_local_ipv4_port;
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
use tracing::{debug, info};

use crate::{
	Error, ErrorKind,
	auth::Client,
	error::ResultExt as _,
	rclone::{binary::managed, rclone_rc_api::RcloneApiClient},
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
		managed::ensure(&config.rclone_binary_dir, false)
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
		let rclone_binary_path = managed::ensure(&config.rclone_binary_dir, true).await?;
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
