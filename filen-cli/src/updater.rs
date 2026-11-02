//! [cli-doc] updates
//! The updater checks for new releases from https://github.com/FilenCloudDienste/filen-cli-releases
//! when the CLI is run (unless skipped in the 5mins since the last check, or through the `--skip-update` flag).
//! The executable will be replaced in place, with the filename updated if it contains the version number.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
	io::{self, Write},
	path::Path,
};

use anyhow::{Context, Result};
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use tempfile::TempPath;
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
	automatically_update: bool,
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
	} else if !is_repl && !automatically_update {
		ui.print_announcement(&format!(
			"Please update from v{} to v{} by invoking the CLI with no command specified (REPL).",
			version, latest_tag
		));
	} else {
		if automatically_update {
			ui.print_muted(&format!(
				"Automatically updating from v{} to v{}...",
				version, latest_tag
			));
		} else if !ui
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
		let current_executable =
			std::env::current_exe().context("Failed to get current executable")?;
		let current_executable_name = current_executable
			.file_name()
			.ok_or(anyhow::anyhow!("Failed to get current executable name"))?
			.to_str()
			.ok_or(anyhow::anyhow!("Failed to get current executable name"))?;
		// owned, because the install below runs on a blocking thread
		let executable_dir = current_executable
			.parent()
			.ok_or(anyhow::anyhow!(
				"Failed to get current executable directory"
			))?
			.to_path_buf();
		let content = download_file(&asset.browser_download_url).await?;
		if current_executable_name.contains(&version) {
			let update_path = current_executable
				.with_file_name(current_executable_name.replace(&version, latest_tag));
			// writing, syncing and renaming a multi-MB binary blocks, so keep it off the
			// runtime's workers
			let update_path = tokio::task::spawn_blocking(move || {
				install_renamed_update(&executable_dir, &content, &update_path, || {
					self_replace::self_delete()
				})
				.map(|()| update_path)
			})
			.await??;
			ui.print_muted(&format!("Downloaded update to {}", update_path.display()));
		} else {
			tokio::task::spawn_blocking(move || {
				install_replacing_update(&executable_dir, &content, |staged| {
					self_replace::self_replace(staged)
				})
			})
			.await??;
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

async fn download_file(url: &str) -> Result<Vec<u8>> {
	let response = reqwest::get(url)
		.await
		.with_context(|| format!("Failed to download file from {}", url))?;
	Ok(response.bytes().await?.into())
}

/// Prefix of the temporary file an update is written to before it is moved into place.
const STAGED_UPDATE_PREFIX: &str = ".filen-cli-update-";

/// Writes a downloaded update to a temporary file in `executable_dir`, the directory of the
/// executable it replaces, so that installing it is a rename within one filesystem (a rename
/// from a temp dir fails with EXDEV on tmpfs, or across Windows volumes).
/// On unix the file is made executable (0755) before anything is replaced.
/// The file is deleted when the returned path is dropped without being persisted.
fn stage_update(executable_dir: &Path, content: &[u8]) -> io::Result<TempPath> {
	let mut file = tempfile::Builder::new()
		.prefix(STAGED_UPDATE_PREFIX)
		.tempfile_in(executable_dir)?;
	file.write_all(content)?;
	file.as_file().sync_all()?;
	#[cfg(unix)]
	file.as_file()
		.set_permissions(std::fs::Permissions::from_mode(0o755))?;
	Ok(file.into_temp_path())
}

/// Atomically moves a staged update to `update_path`, replacing any file there.
/// On failure the staged file is deleted and nothing else is touched.
fn move_into_place(staged: TempPath, update_path: &Path) -> io::Result<()> {
	// `error.path` is dropped here, which deletes the staged file
	staged.persist(update_path).map_err(|error| error.error)
}

/// Installs an update under a new file name, for a binary whose name carries its version:
/// stages `content` in `executable_dir`, moves it to `update_path`, and only then calls
/// `delete_old` to remove the running binary. If staging or moving fails, `delete_old` is
/// never called, so the old binary stays usable, and the staged file is removed.
fn install_renamed_update(
	executable_dir: &Path,
	content: &[u8],
	update_path: &Path,
	delete_old: impl FnOnce() -> io::Result<()>,
) -> Result<()> {
	let staged = stage_update(executable_dir, content)
		.with_context(|| format!("Failed to write update to {}", executable_dir.display()))?;
	move_into_place(staged, update_path)
		.with_context(|| format!("Failed to move update to {}", update_path.display()))?;
	delete_old().context("Failed to delete old binary")
}

/// Installs an update over the running binary, for a binary whose name carries no version:
/// stages `content` in `executable_dir` and hands its path to `replace` (`self_replace`, which
/// copies the file over the old binary and keeps the old binary's permissions). The staged
/// file is removed afterwards, whether `replace` succeeded or not.
fn install_replacing_update(
	executable_dir: &Path,
	content: &[u8],
	replace: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<()> {
	let staged = stage_update(executable_dir, content)
		.with_context(|| format!("Failed to write update to {}", executable_dir.display()))?;
	replace(&staged).context("Failed to replace binary with update")
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

	async fn send_releases_request(&self, token: Option<&str>) -> Result<reqwest::Response> {
		let mut request = self
			.client
			.get(
				"https://api.github.com/repos/FilenCloudDienste/filen-cli-releases/releases/latest",
			)
			.header("User-Agent", "filen-cli");
		if let Some(token) = token {
			request = request.bearer_auth(token);
		}
		request
			.send()
			.await
			.context("Failed to send request to GitHub API for releases")
	}

	async fn get_latest_release(&self) -> Result<GitHubRelease> {
		// Unauthenticated api.github.com allows 60 requests/hour per IP, which shared CI
		// runner IPs exhaust; the resulting rate-limit JSON used to surface as an opaque
		// body-decode failure below. A token (CI passes the workflow's GITHUB_TOKEN)
		// lifts the limit. Scoped to this api.github.com request on purpose — the
		// announcements fetch goes to a different host and needs no auth.
		let token = std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty());
		let mut response = self.send_releases_request(token.as_deref()).await?;
		// A token must never fail harder than no token: GITHUB_TOKEN is an installation
		// token scoped to the calling repository, and reads of ANOTHER (public) repo's
		// releases are expected to work but can be rejected as "Resource not accessible
		// by integration". On an auth-shaped rejection, retry unauthenticated — worst
		// case that retry hits the plain IP rate limit and we report that instead.
		if token.is_some() && matches!(response.status().as_u16(), 401 | 403) {
			response = self.send_releases_request(None).await?;
		}
		let status = response.status();
		if !status.is_success() {
			// Read the body for the error: GitHub explains itself there (rate limits
			// most importantly), and decoding a non-2xx body as a release produced
			// undiagnosable "error decoding response body" failures.
			let body = response.text().await.unwrap_or_default();
			let body = body.chars().take(300).collect::<String>();
			anyhow::bail!("GitHub API returned {status} for releases: {body}");
		}
		response
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

#[cfg(test)]
mod tests {
	use std::cell::Cell;

	use super::*;

	/// Lists the names of the files in `dir`, sorted.
	fn file_names(dir: &Path) -> Vec<String> {
		let mut names = std::fs::read_dir(dir)
			.unwrap()
			.map(|entry| entry.unwrap().file_name().into_string().unwrap())
			.collect::<Vec<_>>();
		names.sort();
		names
	}

	#[cfg(unix)]
	fn mode(path: &Path) -> u32 {
		std::fs::metadata(path).unwrap().permissions().mode() & 0o777
	}

	#[test]
	fn staged_update_is_written_next_to_the_executable() {
		let dir = tempfile::tempdir().unwrap();
		let staged = stage_update(dir.path(), b"new binary").unwrap();
		assert_eq!(staged.parent(), Some(dir.path()));
		assert!(
			staged
				.file_name()
				.unwrap()
				.to_str()
				.unwrap()
				.starts_with(STAGED_UPDATE_PREFIX)
		);
		assert_eq!(std::fs::read(&staged).unwrap(), b"new binary");
	}

	#[cfg(unix)]
	#[test]
	fn staged_update_is_executable() {
		let dir = tempfile::tempdir().unwrap();
		let staged = stage_update(dir.path(), b"new binary").unwrap();
		assert_eq!(mode(&staged), 0o755);
	}

	#[test]
	fn renamed_update_is_moved_into_place_before_the_old_binary_is_deleted() {
		let dir = tempfile::tempdir().unwrap();
		let old = dir.path().join("filen-cli-v0.2.8-linux-x64");
		std::fs::write(&old, b"old binary").unwrap();
		let update_path = dir.path().join("filen-cli-v0.2.9-linux-x64");

		install_renamed_update(dir.path(), b"new binary", &update_path, || {
			// the new binary must be complete and in place when the old one goes
			assert_eq!(std::fs::read(&update_path).unwrap(), b"new binary");
			#[cfg(unix)]
			assert_eq!(mode(&update_path), 0o755);
			std::fs::remove_file(&old)
		})
		.unwrap();

		assert_eq!(file_names(dir.path()), ["filen-cli-v0.2.9-linux-x64"]);
	}

	#[test]
	fn failed_renamed_update_keeps_the_old_binary_and_deletes_the_staged_file() {
		let dir = tempfile::tempdir().unwrap();
		let old = dir.path().join("filen-cli-v0.2.8-linux-x64");
		std::fs::write(&old, b"old binary").unwrap();
		// the parent directory does not exist, so the move fails
		let update_path = dir
			.path()
			.join("missing")
			.join("filen-cli-v0.2.9-linux-x64");
		let delete_called = Cell::new(false);

		let error = install_renamed_update(dir.path(), b"new binary", &update_path, || {
			delete_called.set(true);
			Ok(())
		})
		.unwrap_err();

		assert_eq!(
			error.downcast_ref::<io::Error>().unwrap().kind(),
			io::ErrorKind::NotFound
		);
		assert!(!delete_called.get());
		assert_eq!(std::fs::read(&old).unwrap(), b"old binary");
		assert_eq!(file_names(dir.path()), ["filen-cli-v0.2.8-linux-x64"]);
	}

	#[test]
	fn replacing_update_is_staged_next_to_the_executable_and_then_removed() {
		let dir = tempfile::tempdir().unwrap();
		let old = dir.path().join("filen-cli");
		std::fs::write(&old, b"old binary").unwrap();

		install_replacing_update(dir.path(), b"new binary", |staged| {
			assert_eq!(staged.parent(), Some(dir.path()));
			#[cfg(unix)]
			assert_eq!(mode(staged), 0o755);
			std::fs::copy(staged, &old).map(|_| ())
		})
		.unwrap();

		assert_eq!(std::fs::read(&old).unwrap(), b"new binary");
		assert_eq!(file_names(dir.path()), ["filen-cli"]);
	}

	#[test]
	fn failed_replacing_update_keeps_the_old_binary_and_deletes_the_staged_file() {
		let dir = tempfile::tempdir().unwrap();
		let old = dir.path().join("filen-cli");
		std::fs::write(&old, b"old binary").unwrap();

		install_replacing_update(dir.path(), b"new binary", |_| {
			Err(io::Error::other("replace failed"))
		})
		.unwrap_err();

		assert_eq!(std::fs::read(&old).unwrap(), b"old binary");
		assert_eq!(file_names(dir.path()), ["filen-cli"]);
	}
}
