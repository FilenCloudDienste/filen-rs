//! [cli-doc] updates
//! The updater checks for new releases from https://github.com/FilenCloudDienste/filen-cli-releases
//! when the CLI is run (unless skipped in the 5mins since the last check, or through the `--skip-update` flag).
//! The executable will be replaced in place, with the filename updated if it contains the version number.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::ui::UI;

const BUILD_TARGET: &str = env!("BUILD_TARGET"); // injected in build.rs

const LAST_CHECK_VALIDITY: std::time::Duration = std::time::Duration::from_mins(5);

/// Checks for updates by querying the GitHub releases API and downloading
/// and installing the latest release if a newer version is available.
/// This function skips the update check in debug builds.
/// It also doesn't check for some time if the last check was recent enough.
/// If the current executable's name contains the version string, the new
/// executable will be saved with the updated version in its name, and the
/// old executable will be deleted. Otherwise, the current executable will
/// be replaced in place.
pub(crate) async fn check_for_updates(
	ui: &mut UI,
	force_update_check: bool,
	config_path: &std::path::Path,
) -> Result<()> {
	let (is_testing, version) = match std::env::var("FILEN_CLI_TESTING_UPDATE") {
		Ok(val) if val == "1" => (true, "0.0.0-test"),
		_ => (false, env!("CARGO_PKG_VERSION")),
	};

	if cfg!(debug_assertions) && !is_testing {
		log::info!("Skipping update check in debug build");
		return Ok(());
	}

	let write_update_check: Option<_>;
	if force_update_check {
		log::info!("Update check forced via --force-update-check");
		write_update_check = None;
	} else {
		let last_checked_config = LastCheckedConfig::new(config_path);
		let last_checked = match last_checked_config.read().await {
			Ok(timestamp) => chrono::DateTime::from_timestamp(timestamp, 0)
				.ok_or(anyhow::anyhow!("Failed to parse timestamp"))?,
			Err(e) => {
				log::warn!("Failed to read last update check: {}", e);
				chrono::DateTime::<chrono::Utc>::MIN_UTC
			}
		};
		if last_checked.timestamp() + LAST_CHECK_VALIDITY.as_secs() as i64
			> chrono::Utc::now().timestamp()
		{
			log::info!(
				"Skipping update check; last checked at {} UTC",
				last_checked.format("%Y-%m-%d %H:%M:%S")
			);
			return Ok(());
		} else if last_checked == chrono::DateTime::<chrono::Utc>::MIN_UTC {
			log::info!("No previous update check found, proceeding with update check");
		} else {
			log::info!(
				"Last update check at {} UTC, proceeding with update check",
				last_checked.format("%Y-%m-%d %H:%M:%S")
			);
		}
		write_update_check = Some(move || async move { last_checked_config.write().await });
	}

	let github_api = GitHubApiClient::new();
	let latest_release = github_api.get_latest_release().await?;
	let latest_tag = latest_release.tag_name.trim_start_matches('v');
	if latest_tag == version {
		log::info!("Up to date: {}", version);
	} else {
		ui.print(&format!("Updating from v{} to v{}...", version, latest_tag));
		let asset = latest_release
			.assets
			.iter()
			.find(|asset| asset.name.contains(BUILD_TARGET))
			.ok_or_else(|| {
				anyhow::anyhow!(
					"No suitable release asset found for target: {}",
					BUILD_TARGET
				)
			})?;
		let tempdir = tempfile::tempdir()?;
		let download_path = &tempdir.path().join(&asset.name);
		download_file(&asset.browser_download_url, download_path).await?;
		let curent_executable =
			std::env::current_exe().context("Fialed to get current executable")?;
		let current_executable_name = curent_executable
			.file_name()
			.ok_or(anyhow::anyhow!("Failed to get current executable name"))?
			.to_str()
			.ok_or(anyhow::anyhow!("Failed to get current executable name"))?;
		if current_executable_name.contains(version) {
			let update_path = curent_executable
				.with_file_name(current_executable_name.replace(version, latest_tag));
			tokio::fs::rename(download_path, &update_path).await?;
			ui.print_muted(&format!("Downloaded update to {}", update_path.display()));
			self_replace::self_delete().context("Failed to delete old binary")?;
		} else {
			self_replace::self_replace(download_path)
				.context("Failed to replace binary with update")?;
			ui.print_muted(
				"Update installed successfully. It will take effect the next time you run the application.",
			);
		}
	}

	if let Some(write_update_check) = write_update_check {
		write_update_check().await?;
	}

	Ok(())
}

async fn download_file(url: &str, destination: &std::path::Path) -> Result<()> {
	let response = reqwest::get(url)
		.await
		.with_context(|| format!("Failed to download file from {}", url))?;
	let mut file = tokio::fs::File::create(destination).await?;
	let content = response.bytes().await?;
	tokio::io::copy(&mut content.as_ref(), &mut file).await?;
	Ok(())
}

// last checked config

struct LastCheckedConfig {
	path: std::path::PathBuf,
}

impl LastCheckedConfig {
	fn new(config_dir: &std::path::Path) -> Self {
		Self {
			path: config_dir.join("last_update_check"),
		}
	}

	async fn read(&self) -> Result<i64> {
		let content = fs::read_to_string(&self.path).await.with_context(|| {
			format!(
				"Failed to read last update check from {}",
				self.path.display()
			)
		})?;
		let timestamp = content
			.parse::<i64>()
			.with_context(|| format!("Failed to parse last update check timestamp: {}", content))?;
		Ok(timestamp)
	}

	async fn write(&self) -> Result<()> {
		let now = chrono::Utc::now().timestamp();
		fs::write(&self.path, now.to_string())
			.await
			.with_context(|| {
				format!(
					"Failed to write last update check to {}",
					self.path.display()
				)
			})?;
		log::info!(
			"Wrote last update check timestamp to {}",
			&self.path.display()
		);
		Ok(())
	}
}

// github api

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GitHubRelease {
	pub(crate) prerelease: bool,
	pub(crate) tag_name: String,
	pub(crate) assets: Vec<GitHubReleaseAsset>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GitHubReleaseAsset {
	pub(crate) name: String,
	pub(crate) browser_download_url: String,
	pub(crate) digest: String,
}

pub(crate) struct GitHubApiClient {
	client: reqwest::Client,
}

impl GitHubApiClient {
	pub(crate) fn new() -> Self {
		Self {
			client: reqwest::Client::new(),
		}
	}

	pub(crate) async fn get_latest_release(&self) -> Result<GitHubRelease> {
		self.client
			.get(
				"https://api.github.com/repos/FilenCloudDienste/filen-cli-releases/releases/latest",
			)
			.header("User-Agent", "filen-cli")
			.send()
			.await
			.context("Failed to send request to GitHub API for releases")?
			.json::<GitHubRelease>()
			.await
			.map_err(|e| anyhow::anyhow!("Failed to parse GitHub releases response: {}", e))
	}
}
