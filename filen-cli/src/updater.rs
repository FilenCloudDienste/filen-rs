//! [cli-doc] updates
//! The updater checks for new releases from https://github.com/FilenCloudDienste/filen-cli-releases
//! when the CLI is run (unless skipped in the 5mins since the last check, or through the `--skip-update` flag).
//! The executable will be replaced in place, with the filename updated if it contains the version number.

use anyhow::{Context, Result};
use semver::VersionReq;
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
/// Annnouncements targeted at the current version will also be displayed.
/// When not using REPL, updates are not automatically installed, only
/// a message is shown to the user.
pub(crate) async fn check_for_updates(
	ui: &mut UI,
	force_update_check: bool,
	config_path: &std::path::Path,
	is_repl: bool,
) -> Result<()> {
	let (is_testing, version) = match std::env::var("FILEN_CLI_TESTING_MOCK_VERSION") {
		Ok(val) if val != "off" => (true, val),
		_ => (false, env!("CARGO_PKG_VERSION").to_string()),
	};

	if cfg!(debug_assertions) && !is_testing {
		log::info!("Skipping update check in debug build");
		return Ok(());
	}

	// handle last checked config
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

	// access GitHub API
	let github_api = GitHubApiClient::new();
	let (latest_release, announcements) = tokio::join!(
		github_api.get_latest_release(),
		github_api.get_announcements(),
	);
	let latest_release = latest_release?;
	let announcements = announcements?;

	// display announcements
	{
		let version = semver::Version::parse(&version)?;
		dbg!(announcements.clone());
		announcements
			.into_iter()
			.filter(|announcement| announcement.version_range.matches(&version))
			.map(|announcement| announcement.message)
			.for_each(|announcement| {
				ui.print_announcement(&announcement);
			});
	}

	// handle updates
	let latest_tag = latest_release.tag_name.trim_start_matches('v');
	if latest_tag == version {
		log::info!("Up to date: {}", version);
	} else if !is_repl {
		ui.print_announcement(&format!(
			"Please update from v{} to v{} by invoking the CLI with no command specified (REPL).",
			version, latest_tag
		));
	} else {
		if !ui
			.prompt_confirm(
				&format!("Update now from v{} to v{}", version, latest_tag),
				true,
			)
			.context("Failed to read confirmation")?
		{
			return Err(UI::failure("Update rejected by user"));
		}
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
			std::env::current_exe().context("Failed to get current executable")?;
		let current_executable_name = curent_executable
			.file_name()
			.ok_or(anyhow::anyhow!("Failed to get current executable name"))?
			.to_str()
			.ok_or(anyhow::anyhow!("Failed to get current executable name"))?;
		if current_executable_name.contains(&version) {
			let update_path = curent_executable
				.with_file_name(current_executable_name.replace(&version, latest_tag));
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

// util

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
struct GitHubRelease {
	prerelease: bool,
	tag_name: String,
	assets: Vec<GitHubReleaseAsset>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GitHubReleaseAsset {
	name: String,
	browser_download_url: String,
	digest: String,
}

#[derive(Debug, Clone)]
struct Announcement {
	version_range: VersionReq,
	message: String,
}

struct GitHubApiClient {
	client: reqwest::Client,
}

impl GitHubApiClient {
	fn new() -> Self {
		Self {
			client: reqwest::Client::new(),
		}
	}

	async fn get_latest_release(&self) -> Result<GitHubRelease> {
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

	async fn get_announcements(&self) -> Result<Vec<Announcement>> {
		Ok(self
			.client
			.get(
				"https://raw.githubusercontent.com/FilenCloudDienste/filen-cli-releases/main/announcements",
			)
			.header("User-Agent", "filen-cli")
			.send()
			.await
			.context("Failed to send request to GitHub API to fetch announcements")?
			.text()
			.await
			.context("Failed to read announcements response text")?
			.lines()
			.filter_map(|l| {
				let (version_req, message) = l.split_once(": ")?;
				Some(Announcement {
					version_range: VersionReq::parse(version_req).ok()?,
					message: message.to_string(),
				})
			})
			.collect::<Vec<Announcement>>())
	}
}
