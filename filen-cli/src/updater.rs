use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ui::UI;

const FILEN_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_TARGET: &str = env!("BUILD_TARGET"); // injected in build.rs

/// Checks for updates by querying the GitHub releases API and downloading
/// and installing the latest release if a newer version is available.
/// This function skips the update check in debug builds.
/// If the current executable's name contains the version string, the new
/// executable will be saved with the updated version in its name, and the
/// old executable will be deleted. Otherwise, the current executable will
/// be replaced in place.
pub(crate) async fn check_for_updates(ui: &mut UI) -> Result<()> {
	// todo: don't check again in quick succession of runs (cache last check time in config file)

	if cfg!(debug_assertions) {
		log::info!("Skipping update check in debug build");
		return Ok(());
	}

	let github_api = GitHubApiClient::new();
	let latest_release = github_api.get_latest_release().await?;
	let latest_tag = latest_release.tag_name.trim_start_matches('v');
	if latest_tag == FILEN_CLI_VERSION {
		log::info!("Up to date: {}", FILEN_CLI_VERSION);
	} else {
		ui.print(&format!(
			"Updating from v{} to v{}...",
			FILEN_CLI_VERSION, latest_tag
		));
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
		if current_executable_name.contains(FILEN_CLI_VERSION) {
			let update_path = curent_executable
				.with_file_name(current_executable_name.replace(FILEN_CLI_VERSION, latest_tag));
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
