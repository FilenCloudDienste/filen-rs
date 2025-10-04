use std::{
	path::{Path, PathBuf},
	process::Stdio,
};
use tokio::{
	fs,
	process::{Child, Command},
};

use anyhow::{Context, Result};
use filen_sdk_rs::auth::Client;
use log::{debug, info};

/// Mount Filen as a network drive using Rclone.
/// Downloads Rclone binary if necessary, writes config file, and starts the `rclone mount` process.
pub async fn mount_network_drive(
	client: &Client,
	password: &str,
	config_dir: &Path,
	mount_point: Option<&str>,
) -> Result<Child> {
	let rclone_binary_path = ensure_rclone_binary(config_dir).await?;
	let rclone_config_path =
		write_rclone_config(&rclone_binary_path, client, password, config_dir).await?;
	start_rclone_mount_process(
		&rclone_binary_path,
		&rclone_config_path,
		mount_point.unwrap_or("F:"), // todo: actual default mount point based on OS
	)
	.await
}

/// Returns the path to the rclone binary, downloading it if necessary
async fn ensure_rclone_binary(config_dir: &Path) -> Result<PathBuf> {
	// determine download url
	let rclone_binary_download_url = match std::env::consts::OS {
		"windows" => {
			"https://github.com/FilenCloudDienste/filen-rclone/releases/download/v1.70.0-filen.12/rclone-v1.70.0-filen.12-windows-arm64.exe"
		}
		// todo: add other platforms/archictures, use proper download location
		os => {
			return Err(anyhow::anyhow!("No Rclone binary for target: {}", os));
		}
	};

	let rclone_binary_dir = config_dir.join("network-drive-rlone");
	let rclone_file_name = rclone_binary_download_url.rsplit_once('/').unwrap().1;
	let rclone_binary_path = rclone_binary_dir.join(rclone_file_name);

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

		// make binary executable
		#[cfg(unix)]
		{
			let mut perms = std::fs::metadata(&rclone_binary_path)?.permissions();
			perms.set_mode(0o755);
			std::fs::set_permissions(&rclone_binary_path, perms)?;
		}
		// todo: test if binaries are actually made executable on unix
	}

	debug!("Using Rclone binary at {}", rclone_binary_path.display());
	Ok(rclone_binary_path)
}

async fn write_rclone_config(
	rclone_binary_path: &Path,
	client: &Client,
	password: &str,
	config_dir: &Path,
) -> Result<PathBuf> {
	let rclone_config_dir = config_dir.join("network-drive-rlone");
	let rclone_config_path = rclone_config_dir.join("rclone.conf");

	let rclone_config_content = format!(
		"[filen]\ntype = filen\nemail = {}\npassword = {}\napi_key = {}",
		client.email(),
		obscure_password_for_rclone(rclone_binary_path, password).await?,
		obscure_password_for_rclone(rclone_binary_path, &client.to_stringified().api_key).await?
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
	rclone_binary_path: &Path,
	rclone_config_path: &Path,
	mount_point: &str,
) -> Result<Child> {
	let process = Command::new(rclone_binary_path)
		.args([
			"mount",
			"filen:",
			mount_point,
			"--config",
			rclone_config_path.to_str().unwrap(),
			// todo: other args...
		])
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		// todo: redirect stdout so it can be logged nicer or not at all
		.spawn()
		.context("Failed to start Rclone mount process")?;
	Ok(process)
}
