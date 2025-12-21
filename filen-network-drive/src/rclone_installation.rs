//! Manages an installation of filen-rclone.
//! Responsible for downloading the binary, writing config files
//! and executing Rclone commands.

use anyhow::{Context, Result};
use filen_sdk_rs::auth::Client;
use hex_literal::hex;
use log::{debug, info};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use tokio::{fs, process::Command};

pub struct RcloneInstallation {
	pub(crate) rclone_binary_path: PathBuf,
	pub(crate) rclone_config_path: PathBuf,
}

impl RcloneInstallation {
	pub async fn initialize(client: &Client, config_dir: &Path) -> Result<Self> {
		fs::create_dir_all(&config_dir)
			.await
			.context("Failed to create Rclone installation directory")?;
		let rclone_binary_path = ensure_rclone_binary(config_dir).await?;
		let rclone_config_path =
			write_rclone_config(&rclone_binary_path, client, config_dir).await?;
		Ok(Self {
			rclone_binary_path,
			rclone_config_path,
		})
	}

	pub async fn execute(&self, args: &[&str]) -> Command {
		let mut cmd = Command::new(&self.rclone_binary_path);
		cmd.args(["--config", self.rclone_config_path.to_str().unwrap()])
			.args(args);
		cmd
	}
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
			"https://github.com/FilenCloudDienste/filen-rclone/releases/download/v1.70.0-filen.14/rclone-v1.70.0-filen.14-{}-{}{}",
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

	let rclone_file_name = rclone_binary_download_url.rsplit_once('/').unwrap().1;
	let rclone_binary_path = config_dir.join(rclone_file_name);
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

		// verify checksum
		let downloaded_checksum = Sha256::digest(&bytes);
		let expected_checksum = match (std::env::consts::OS, std::env::consts::ARCH) {
			("windows", "x86_64") => {
				hex!("c855395339115d314b920c574610f559df03d2baaea096866b4532a59b5b7235")
			}
			("windows", "aarch64") => {
				hex!("ab24619f110ef14f30d4e8e77fcca8e17f5dbe7e413f907a0cf76b46fe3ffaf4")
			}
			("linux", "x86_64") => {
				hex!("265f641844a2aa17c202f3cf6c06bc98dcd9b6c3b3fa228c5c454a203105de07")
			}
			("linux", "aarch64") => {
				hex!("f70689b939c19bb6c81784f9003f09329b97d96be4ab2c70d1f2dc6e8fd417e5")
			}
			("macos", "x86_64") => {
				hex!("1210e92b8adc172356a9bd79216d6a4fe22875b957326292ac2da56f2af6b207")
			}
			("macos", "aarch64") => {
				hex!("cabaee158814afad05ebbe7f8469608e0ea3c4a1979063d52ff0289aaafdf89b")
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
	let rclone_config_path = config_dir.join("rclone.conf");

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
