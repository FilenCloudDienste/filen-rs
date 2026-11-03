//! [cli-doc] updates
//! The updater checks for new releases from https://github.com/FilenCloudDienste/filen-cli-releases
//! when the CLI is run (unless skipped in the 5mins since the last check, or through the `--skip-update` flag).
//! The executable will be replaced in place, with the filename updated if it contains the version number.
//! In the Docker image, a newer release is only announced, with the image to pull; the CLI never
//! replaces itself there, not even with `--always-update`.
//! The release notes of the new release are shown before it is offered or installed.
//! Update notices, release notes and announcements are printed to stderr, and only when stdout is a terminal and neither `--json` nor `--quiet` is given.
//! A failed update check (for example when offline) is printed as a warning to stderr and does not stop the command.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
	fmt,
	io::{self, Write},
	path::Path,
};

use anyhow::{Context, Result};
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempPath;
use tokio::fs;

use crate::ui::UI;

const BUILD_TARGET: &str = env!("BUILD_TARGET"); // injected in build.rs

/// The Docker image the CLI is published as, tagged with each release's version.
const DOCKER_IMAGE: &str = "filen/cli";

/// The file the announcements are published in, one per line.
const ANNOUNCEMENTS_URL: &str =
	"https://raw.githubusercontent.com/FilenCloudDienste/filen-cli-releases/main/announcements";

/// How long after a check the next one is skipped, in seconds.
const LAST_CHECK_VALIDITY_SECS: i64 = 5 * 60;

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
/// a message is shown to the user. Declining the update in the REPL
/// starts the REPL with the current version.
/// A failed check (offline, an error from GitHub, a failed install) is printed as a warning
/// and never fails the command. It still counts as a check, so the next one waits as long as
/// after a successful one.
pub(crate) async fn check_for_updates(
	ui: &mut UI,
	force_update_check: bool,
	automatically_update: bool,
	config_dir: &Path,
	is_repl: bool,
) {
	let (is_testing, version) = match std::env::var("FILEN_CLI_TESTING_MOCK_VERSION") {
		Ok(val) if val != "off" => (true, val),
		_ => (false, env!("CARGO_PKG_VERSION").to_string()),
	};

	if cfg!(debug_assertions) && !is_testing {
		log::info!("Skipping update check in debug build");
		return;
	}

	// a forced check is not recorded, so it does not delay the next regular one
	let last_checked_config = if force_update_check {
		log::info!("Update check forced via --force-update-check");
		None
	} else {
		let last_checked_config = LastCheckedConfig::new(config_dir);
		if !last_checked_config.is_check_due().await {
			return;
		}
		Some(last_checked_config)
	};

	let github_api = GitHubApiClient::new();
	run_update_check(
		ui,
		&version,
		UpdateContext {
			automatically_update,
			is_repl,
			in_container: cfg!(feature = "is_docker"),
		},
		last_checked_config.as_ref(),
		async || {
			tokio::join!(
				github_api.get_latest_release(),
				github_api.get_announcements(),
			)
		},
		// installing an update is not what --yes confirms
		|ui, question| ui.prompt_yes_no(question, true),
	)
	.await;
}

/// Shows the announcements and handles the latest release that `fetch` returns, printing a
/// warning for each part that fails, then writes the time of the check to
/// `last_checked_config`, whether the check succeeded or not. A running `version` that is not a
/// semver version fails the whole check, before anything is fetched. `confirm` asks whether to
/// install the update the REPL offers, [`UI::prompt_yes_no`] outside the tests.
async fn run_update_check(
	ui: &mut UI,
	version: &str,
	context: UpdateContext,
	last_checked_config: Option<&LastCheckedConfig>,
	fetch: impl AsyncFnOnce() -> (Result<GitHubRelease>, Result<Vec<Announcement>>),
	confirm: impl FnOnce(&mut UI, &str) -> Result<bool>,
) {
	match semver::Version::parse(version) {
		Ok(current) => {
			let (latest_release, announcements) = fetch().await;

			match announcements {
				Ok(announcements) => show_announcements(ui, &current, announcements),
				Err(e) => ui.print_side_warning(&format!("Failed to fetch announcements: {e:#}")),
			}

			let running = RunningVersion {
				current: &current,
				version,
			};
			let updated = match latest_release {
				Ok(latest_release) => {
					handle_latest_release(ui, running, &latest_release, context, confirm).await
				}
				Err(e) => Err(e),
			};
			if let Err(e) = updated {
				ui.print_side_warning(&format!("Update check failed: {e:#}"));
			}
		}
		Err(e) => ui.print_side_warning(&format!(
			"Update check failed: Failed to parse the running version {version}: {e}"
		)),
	}

	if let Some(last_checked_config) = last_checked_config
		&& let Err(e) = last_checked_config.write().await
	{
		log::warn!("{e:#}");
	}
}

/// Shows the announcements targeted at `version`.
fn show_announcements(ui: &mut UI, version: &semver::Version, announcements: Vec<Announcement>) {
	announcements
		.into_iter()
		.filter(|announcement| announcement.version_range.matches(version))
		.for_each(|announcement| ui.print_announcement(&announcement.message));
}

/// The version of the running CLI.
#[derive(Clone, Copy)]
struct RunningVersion<'a> {
	current: &'a semver::Version,
	/// As it was given, which the name of an executable that carries its version holds.
	version: &'a str,
}

/// How the CLI runs, which decides what it does about a newer release.
#[derive(Debug, Clone, Copy)]
struct UpdateContext {
	/// `--always-update`: install a newer release without asking.
	automatically_update: bool,
	/// Whether the REPL is starting, the only place an update is offered.
	is_repl: bool,
	/// Whether the CLI runs in its Docker image, which it must not replace itself in.
	in_container: bool,
}

/// What to do about the latest release.
#[derive(Debug, PartialEq, Eq)]
enum ReleaseAction {
	/// It is not newer than the running version.
	Nothing,
	/// Tell the user to update from the REPL, outside of which the CLI does not update.
	Announce,
	/// Tell the user to pull the release's Docker image, in which the CLI does not update itself.
	AnnounceImage,
	/// Ask the user whether to install it.
	Offer,
	/// Install it without asking, for `--always-update`.
	Install,
}

/// Chooses what to do about the `latest` release when running `current`. Only a release of
/// higher semver precedence is offered or installed, never an older one (a GitHub release
/// marked latest by mistake, or a newer local build). In a container it is only announced.
fn release_action(
	current: &semver::Version,
	latest: &semver::Version,
	UpdateContext {
		automatically_update,
		is_repl,
		in_container,
	}: UpdateContext,
) -> ReleaseAction {
	if latest.cmp_precedence(current).is_le() {
		ReleaseAction::Nothing
	} else if in_container {
		ReleaseAction::AnnounceImage
	} else if automatically_update {
		ReleaseAction::Install
	} else if is_repl {
		ReleaseAction::Offer
	} else {
		ReleaseAction::Announce
	}
}

/// Offers, announces or installs `latest_release` if it is newer than the running version. An
/// offer that cannot be asked is announced instead, as outside the REPL; declining it is not a
/// failure, the CLI carries on with the running version.
async fn handle_latest_release(
	ui: &mut UI,
	RunningVersion { current, version }: RunningVersion<'_>,
	latest_release: &GitHubRelease,
	context: UpdateContext,
	confirm: impl FnOnce(&mut UI, &str) -> Result<bool>,
) -> Result<()> {
	let latest_tag = latest_release.tag_name.trim_start_matches('v');
	let latest = semver::Version::parse(latest_tag)
		.with_context(|| format!("Failed to parse the latest release's version {latest_tag}"))?;
	let show_release_notes = |ui: &mut UI| {
		if let Some(notes) = latest_release
			.body
			.as_deref()
			.map(str::trim)
			.filter(|notes| !notes.is_empty())
		{
			ui.print_muted_notice(&format!("Release notes for v{latest_tag}:\n{notes}"));
		}
	};
	let announce = |ui: &mut UI| {
		ui.print_announcement(&format!(
			"Please update from v{} to v{} by invoking the CLI with no command specified (REPL).",
			version, latest_tag
		));
	};
	match release_action(current, &latest, context) {
		ReleaseAction::Nothing => {
			log::info!("Up to date: v{version} (latest release: v{latest_tag})");
			return Ok(());
		}
		ReleaseAction::Announce => {
			announce(ui);
			return Ok(());
		}
		ReleaseAction::AnnounceImage => {
			ui.print_announcement(&format!(
				"Update available: v{version} -> v{latest_tag}. Pull the new image: docker pull \
				 {DOCKER_IMAGE}:{latest_tag}"
			));
			return Ok(());
		}
		ReleaseAction::Install => {
			show_release_notes(ui);
			ui.print_muted_to_stderr(&format!(
				"Automatically updating from v{} to v{}...",
				version, latest_tag
			));
		}
		ReleaseAction::Offer => {
			let question = format!("Update now from v{} to v{}", version, latest_tag);
			if ui.ensure_can_prompt(&question).is_err() {
				announce(ui);
				return Ok(());
			}
			show_release_notes(ui);
			if !confirm(ui, &question).context("Failed to read confirmation")? {
				// the CLI carries on with this version, and asks again after the next check
				log::info!("Update to v{latest_tag} declined");
				return Ok(());
			}
		}
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
	let current_executable = std::env::current_exe().context("Failed to get current executable")?;
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
	verify_download(asset, &content)?;
	if current_executable_name.contains(version) {
		let update_path =
			current_executable.with_file_name(current_executable_name.replace(version, latest_tag));
		// writing, syncing and renaming a multi-MB binary blocks, so keep it off the
		// runtime's workers
		let update_path = tokio::task::spawn_blocking(move || {
			install_renamed_update(&executable_dir, &content, &update_path, || {
				self_replace::self_delete()
			})
			.map(|()| update_path)
		})
		.await??;
		ui.print_muted_to_stderr(&format!("Downloaded update to {}", update_path.display()));
	} else {
		tokio::task::spawn_blocking(move || {
			install_replacing_update(&executable_dir, &content, |staged| {
				self_replace::self_replace(staged)
			})
		})
		.await??;
		ui.print_muted_to_stderr(
			"Update installed successfully. It will take effect the next time you run the application.",
		);
	}
	Ok(())
}

// util

/// Downloads `url`, failing on a response that is not a success: its body is an error page, not
/// the file.
async fn download_file(url: &str) -> Result<Vec<u8>> {
	let response = reqwest::get(url)
		.await
		.with_context(|| format!("Failed to download file from {}", url))?;
	let status = response.status();
	if !status.is_success() {
		let body = response.text().await.unwrap_or_default();
		let body = body.chars().take(300).collect::<String>();
		anyhow::bail!("Downloading {url} returned {status}: {body}");
	}
	Ok(response
		.bytes()
		.await
		.with_context(|| format!("Failed to download file from {}", url))?
		.into())
}

/// Checks a downloaded release asset against the size and the digest its release lists, either of
/// which may be missing (older releases carry no digest). A digest in an algorithm other than
/// SHA-256 cannot be checked, so the download is refused rather than installed unchecked.
fn verify_download(asset: &GitHubReleaseAsset, content: &[u8]) -> Result<()> {
	if let Some(expected) = asset.size {
		let downloaded = u64::try_from(content.len()).context("Download size out of range")?;
		if downloaded != expected {
			anyhow::bail!(
				"The download of {} is {downloaded} bytes, but its release lists {expected} bytes",
				asset.name
			);
		}
	}
	match asset
		.digest
		.as_deref()
		.map(AssetDigest::parse)
		.transpose()?
	{
		Some(AssetDigest::Sha256(expected)) => {
			let actual = Sha256Digest::of(content);
			if actual != expected {
				anyhow::bail!(
					"The download of {} has the SHA-256 digest {actual}, but its release lists \
					 {expected}",
					asset.name
				);
			}
			Ok(())
		}
		Some(AssetDigest::Unsupported(algorithm)) => anyhow::bail!(
			"The release lists a {algorithm} digest for {}, which cannot be checked",
			asset.name
		),
		None => {
			log::info!("The release lists no digest for {}", asset.name);
			Ok(())
		}
	}
}

/// A SHA-256 digest.
#[derive(Debug, PartialEq, Eq)]
struct Sha256Digest([u8; 32]);

impl Sha256Digest {
	/// The digest of `content`.
	fn of(content: &[u8]) -> Self {
		Self(Sha256::digest(content).into())
	}

	/// Parses 64 hex digits, in either case.
	fn parse_hex(hex: &str) -> Option<Self> {
		let (pairs, rest) = hex.as_bytes().as_chunks::<2>();
		if !rest.is_empty() {
			return None;
		}
		let nibble = |digit: u8| {
			char::from(digit)
				.to_digit(16)
				.and_then(|value| u8::try_from(value).ok())
		};
		let mut digest = [0; 32];
		if pairs.len() != digest.len() {
			return None;
		}
		for (byte, [high, low]) in digest.iter_mut().zip(pairs) {
			*byte = (nibble(*high)? << 4) | nibble(*low)?;
		}
		Some(Self(digest))
	}
}

impl fmt::Display for Sha256Digest {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
	}
}

/// The `digest` GitHub lists for a release asset, `<algorithm>:<hex digest>`.
#[derive(Debug, PartialEq, Eq)]
enum AssetDigest<'a> {
	Sha256(Sha256Digest),
	/// A digest in another algorithm, by its name.
	Unsupported(&'a str),
}

impl<'a> AssetDigest<'a> {
	fn parse(digest: &'a str) -> Result<Self> {
		let (algorithm, hex) = digest
			.split_once(':')
			.with_context(|| format!("Malformed release asset digest {digest:?}"))?;
		if algorithm != "sha256" {
			return Ok(Self::Unsupported(algorithm));
		}
		Sha256Digest::parse_hex(hex)
			.map(Self::Sha256)
			.with_context(|| format!("Malformed release asset SHA-256 digest {digest:?}"))
	}
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

	/// Whether the last check is too long ago (or unknown) to skip this one.
	async fn is_check_due(&self) -> bool {
		let last_checked = match self.read().await {
			Ok(timestamp) => Some(timestamp),
			Err(e) => {
				log::info!("No previous update check found: {e:#}");
				None
			}
		};
		let due = is_check_due(last_checked, chrono::Utc::now().timestamp());
		if let Some(last_checked) = last_checked {
			let last_checked = chrono::DateTime::from_timestamp(last_checked, 0)
				.map(|date| date.format("%Y-%m-%d %H:%M:%S UTC").to_string())
				.unwrap_or_else(|| last_checked.to_string());
			if due {
				log::info!("Last update check at {last_checked}, proceeding with update check");
			} else {
				log::info!("Skipping update check; last checked at {last_checked}");
			}
		}
		due
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

/// Whether an update check is due at `now`, given when the last one was (both in seconds since
/// the epoch). A check is due when there was none, or when the last one is
/// [`LAST_CHECK_VALIDITY_SECS`] or more ago.
fn is_check_due(last_checked: Option<i64>, now: i64) -> bool {
	last_checked
		.and_then(|last_checked| last_checked.checked_add(LAST_CHECK_VALIDITY_SECS))
		.is_none_or(|next_check| next_check <= now)
}

// github api

#[derive(Debug, Serialize, Deserialize)]
struct GitHubRelease {
	prerelease: bool,
	tag_name: String,
	/// The release notes, in Markdown.
	body: Option<String>,
	assets: Vec<GitHubReleaseAsset>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GitHubReleaseAsset {
	name: String,
	browser_download_url: String,
	/// `<algorithm>:<hex digest>`, missing on assets GitHub uploaded before it listed digests.
	digest: Option<String>,
	/// In bytes.
	size: Option<u64>,
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
		self.get_announcements_from(ANNOUNCEMENTS_URL).await
	}

	async fn get_announcements_from(&self, url: &str) -> Result<Vec<Announcement>> {
		let response = self
			.client
			.get(url)
			.header("User-Agent", "filen-cli")
			.send()
			.await
			.context("Failed to send request to GitHub API to fetch announcements")?;
		let status = response.status();
		if !status.is_success() {
			// the body is an error page, not announcements
			let body = response.text().await.unwrap_or_default();
			let body = body.chars().take(300).collect::<String>();
			anyhow::bail!("Fetching announcements returned {status}: {body}");
		}
		let text = response
			.text()
			.await
			.context("Failed to read announcements response text")?;
		Ok(parse_announcements(&text))
	}
}

/// Parses the announcements file: one `<version range>: <message>` per line. A line that is not
/// one is skipped.
fn parse_announcements(text: &str) -> Vec<Announcement> {
	text.lines()
		.filter_map(|line| {
			let (version_req, message) = line.split_once(": ")?;
			Some(Announcement {
				version_range: VersionReq::parse(version_req).ok()?,
				message: message.to_string(),
			})
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use std::{cell::Cell, io::Read, net::TcpListener};

	use super::*;
	use crate::ui::test_support::Printed;

	/// A release tagged `tag` with no assets.
	fn release(tag: &str) -> GitHubRelease {
		GitHubRelease {
			prerelease: false,
			tag_name: tag.to_owned(),
			body: None,
			assets: Vec::new(),
		}
	}

	/// A release tagged `tag` with `notes` and no assets.
	fn release_with_notes(tag: &str, notes: &str) -> GitHubRelease {
		GitHubRelease {
			body: Some(notes.to_owned()),
			..release(tag)
		}
	}

	fn announcement(version_range: &str, message: &str) -> Announcement {
		Announcement {
			version_range: VersionReq::parse(version_range).unwrap(),
			message: message.to_owned(),
		}
	}

	/// The time of the last check written to `config_dir`, if any.
	fn written_stamp(config_dir: &Path) -> Option<i64> {
		std::fs::read_to_string(config_dir.join("last_update_check"))
			.ok()
			.map(|stamp| stamp.parse().unwrap())
	}

	#[test]
	fn a_check_is_due_without_a_previous_one() {
		assert!(is_check_due(None, 1_000_000));
	}

	#[test]
	fn a_check_is_skipped_until_the_last_one_expires() {
		let now = 1_000_000;
		assert!(!is_check_due(Some(now), now));
		assert!(!is_check_due(Some(now - LAST_CHECK_VALIDITY_SECS + 1), now));
		assert!(is_check_due(Some(now - LAST_CHECK_VALIDITY_SECS), now));
	}

	#[test]
	fn a_check_is_due_when_the_last_ones_time_is_out_of_range() {
		assert!(is_check_due(Some(i64::MAX), 1_000_000));
	}

	#[tokio::test]
	async fn a_failed_release_fetch_is_a_warning_and_still_writes_the_stamp() {
		let config_dir = tempfile::tempdir().unwrap();
		let printed = Printed::default();
		let mut ui = printed.terminal_ui();

		run_update_check(
			&mut ui,
			"0.2.9",
			started(false, false),
			Some(&LastCheckedConfig::new(config_dir.path())),
			async || {
				(
					Err(anyhow::anyhow!("GitHub API returned 503")),
					Ok(vec![announcement(">=0.2.0", "Hello")]),
				)
			},
			not_asked,
		)
		.await;

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"[i] Hello\n⚠ Update check failed: GitHub API returned 503"
		);
		assert!(written_stamp(config_dir.path()).is_some());
	}

	#[tokio::test]
	async fn a_failed_check_prints_no_warning_when_quiet_or_with_json_output() {
		for json in [false, true] {
			let printed = Printed::default();
			let mut ui = printed.terminal_ui();
			ui.set_quiet_and_json(!json, json);

			run_update_check(
				&mut ui,
				"0.2.9",
				started(false, false),
				None,
				async || {
					(
						Err(anyhow::anyhow!("GitHub API returned 503")),
						Err(anyhow::anyhow!("connection refused")),
					)
				},
				not_asked,
			)
			.await;

			assert_eq!(printed.lines_or_empty(), "", "json: {json}");
		}
	}

	#[tokio::test]
	async fn a_failed_announcements_fetch_is_a_warning_and_still_handles_the_release() {
		let config_dir = tempfile::tempdir().unwrap();
		let printed = Printed::default();
		let mut ui = printed.terminal_ui();

		run_update_check(
			&mut ui,
			"0.2.9",
			started(false, false),
			Some(&LastCheckedConfig::new(config_dir.path())),
			async || {
				(
					Ok(release("v0.3.0")),
					Err(anyhow::anyhow!("connection refused")),
				)
			},
			not_asked,
		)
		.await;

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"⚠ Failed to fetch announcements: connection refused\n[i] Please update from \
			 v0.2.9 to v0.3.0 by invoking the CLI with no command specified (REPL)."
		);
		assert!(written_stamp(config_dir.path()).is_some());
	}

	#[tokio::test]
	async fn a_release_without_an_asset_for_this_target_is_a_warning() {
		let config_dir = tempfile::tempdir().unwrap();
		let printed = Printed::default();
		let mut ui = printed.ui();

		run_update_check(
			&mut ui,
			"0.2.9",
			started(true, false),
			Some(&LastCheckedConfig::new(config_dir.path())),
			async || (Ok(release("v0.3.0")), Ok(Vec::new())),
			not_asked,
		)
		.await;

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			format!(
				"Automatically updating from v0.2.9 to v0.3.0...\n⚠ Update check failed: No \
				 suitable release asset found for target: {BUILD_TARGET}"
			)
		);
		assert!(written_stamp(config_dir.path()).is_some());
	}

	/// The answer to an update offer the test does not expect.
	fn not_asked(_: &mut UI, question: &str) -> Result<bool> {
		panic!("asked {question:?}")
	}

	#[tokio::test]
	async fn a_declined_update_starts_quietly_and_writes_the_stamp() {
		let config_dir = tempfile::tempdir().unwrap();
		let printed = Printed::default();
		let mut ui = printed.prompting_ui();
		let asked = Cell::new(None);

		run_update_check(
			&mut ui,
			"0.2.9",
			started(false, true),
			Some(&LastCheckedConfig::new(config_dir.path())),
			async || (Ok(release("v0.3.0")), Ok(Vec::new())),
			|_, question| {
				asked.set(Some(question.to_owned()));
				Ok(false)
			},
		)
		.await;

		assert_eq!(
			asked.into_inner().as_deref(),
			Some("Update now from v0.2.9 to v0.3.0")
		);
		assert_eq!(printed.lines_or_empty(), "");
		assert!(written_stamp(config_dir.path()).is_some());
	}

	#[tokio::test]
	async fn a_canceled_update_offer_is_a_warning_and_still_writes_the_stamp() {
		let config_dir = tempfile::tempdir().unwrap();
		let printed = Printed::default();
		let mut ui = printed.prompting_ui();

		run_update_check(
			&mut ui,
			"0.2.9",
			started(false, true),
			Some(&LastCheckedConfig::new(config_dir.path())),
			async || (Ok(release("v0.3.0")), Ok(Vec::new())),
			|_, _| Err(UI::failure("Canceled")),
		)
		.await;

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"⚠ Update check failed: Failed to read confirmation: Canceled"
		);
		assert!(written_stamp(config_dir.path()).is_some());
	}

	#[tokio::test]
	async fn an_update_offer_nobody_can_answer_is_announced_instead() {
		let printed = Printed::default();
		let mut ui = printed.terminal_ui();
		ui.disable_prompts();

		run_update_check(
			&mut ui,
			"0.2.9",
			started(false, true),
			None,
			async || (Ok(release("v0.3.0")), Ok(Vec::new())),
			not_asked,
		)
		.await;

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"[i] Please update from v0.2.9 to v0.3.0 by invoking the CLI with no command \
			 specified (REPL)."
		);
	}

	#[tokio::test]
	async fn a_running_version_that_is_not_semver_is_one_warning() {
		let config_dir = tempfile::tempdir().unwrap();
		let printed = Printed::default();
		let mut ui = printed.terminal_ui();

		run_update_check(
			&mut ui,
			"dev",
			started(false, false),
			Some(&LastCheckedConfig::new(config_dir.path())),
			async || -> (Result<GitHubRelease>, Result<Vec<Announcement>>) {
				panic!("fetched with a running version that is not semver")
			},
			not_asked,
		)
		.await;

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"⚠ Update check failed: Failed to parse the running version dev: unexpected \
			 character 'd' while parsing major version number"
		);
		assert!(written_stamp(config_dir.path()).is_some());
	}

	/// How the CLI runs outside a container.
	fn started(automatically_update: bool, is_repl: bool) -> UpdateContext {
		UpdateContext {
			automatically_update,
			is_repl,
			in_container: false,
		}
	}

	#[test]
	fn a_newer_release_is_only_announced_in_a_container() {
		for (automatically_update, is_repl) in [(false, false), (false, true), (true, false)] {
			assert_eq!(
				release_action(
					&version("0.2.9"),
					&version("0.3.0"),
					UpdateContext {
						automatically_update,
						is_repl,
						in_container: true,
					}
				),
				ReleaseAction::AnnounceImage
			);
		}
	}

	#[test]
	fn the_running_release_is_not_announced_in_a_container() {
		assert_eq!(
			release_action(
				&version("0.2.9"),
				&version("0.2.9"),
				UpdateContext {
					automatically_update: true,
					is_repl: true,
					in_container: true,
				}
			),
			ReleaseAction::Nothing
		);
	}

	#[tokio::test]
	async fn a_container_is_told_which_image_to_pull() {
		let printed = Printed::default();
		let mut ui = printed.terminal_ui();

		run_update_check(
			&mut ui,
			"0.2.9",
			UpdateContext {
				automatically_update: true,
				is_repl: true,
				in_container: true,
			},
			None,
			async || (Ok(release("v0.3.0")), Ok(Vec::new())),
			not_asked,
		)
		.await;

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"[i] Update available: v0.2.9 -> v0.3.0. Pull the new image: docker pull \
			 filen/cli:0.3.0"
		);
	}

	#[tokio::test]
	async fn the_release_notes_are_shown_before_the_update_is_offered() {
		let printed = Printed::default();
		let mut ui = printed.prompting_terminal_ui();
		let printed_before_asking = Cell::new(None);

		run_update_check(
			&mut ui,
			"0.2.9",
			started(false, true),
			None,
			async || {
				(
					Ok(release_with_notes("v0.3.0", "## Fixes\n- fewer bugs\n")),
					Ok(Vec::new()),
				)
			},
			|_, _| {
				printed_before_asking.set(Some(
					console::strip_ansi_codes(&printed.lines()).into_owned(),
				));
				Ok(false)
			},
		)
		.await;

		assert_eq!(
			printed_before_asking.into_inner().as_deref(),
			Some("Release notes for v0.3.0:\n## Fixes\n- fewer bugs")
		);
	}

	#[tokio::test]
	async fn the_release_notes_are_shown_before_an_automatic_update() {
		let printed = Printed::default();
		let mut ui = printed.terminal_ui();

		run_update_check(
			&mut ui,
			"0.2.9",
			started(true, false),
			None,
			async || (Ok(release_with_notes("v0.3.0", "Fixes")), Ok(Vec::new())),
			not_asked,
		)
		.await;

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			format!(
				"Release notes for v0.3.0:\nFixes\nAutomatically updating from v0.2.9 to \
				 v0.3.0...\n⚠ Update check failed: No suitable release asset found for target: \
				 {BUILD_TARGET}"
			)
		);
	}

	#[tokio::test]
	async fn the_release_notes_are_not_shown_when_no_person_reads_the_output() {
		let printed = Printed::default();
		let mut ui = printed.ui();

		run_update_check(
			&mut ui,
			"0.2.9",
			started(true, false),
			None,
			async || (Ok(release_with_notes("v0.3.0", "Fixes")), Ok(Vec::new())),
			not_asked,
		)
		.await;

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			format!(
				"Automatically updating from v0.2.9 to v0.3.0...\n⚠ Update check failed: No \
				 suitable release asset found for target: {BUILD_TARGET}"
			)
		);
	}

	#[tokio::test]
	async fn empty_release_notes_are_not_shown() {
		let printed = Printed::default();
		let mut ui = printed.prompting_terminal_ui();

		run_update_check(
			&mut ui,
			"0.2.9",
			started(false, true),
			None,
			async || (Ok(release_with_notes("v0.3.0", " \n")), Ok(Vec::new())),
			|_, _| Ok(false),
		)
		.await;

		assert_eq!(printed.lines_or_empty(), "");
	}

	#[test]
	fn a_release_without_notes_deserializes() {
		let release: GitHubRelease = serde_json::from_str(
			r#"{"prerelease": false, "tag_name": "v0.3.0", "body": null, "assets": []}"#,
		)
		.unwrap();
		assert_eq!(release.body, None);
	}

	fn version(version: &str) -> semver::Version {
		semver::Version::parse(version).unwrap()
	}

	#[test]
	fn a_newer_release_is_announced_outside_the_repl() {
		assert_eq!(
			release_action(&version("0.2.9"), &version("0.2.10"), started(false, false)),
			ReleaseAction::Announce
		);
	}

	#[test]
	fn a_newer_release_is_offered_in_the_repl() {
		assert_eq!(
			release_action(&version("0.2.9"), &version("0.3.0"), started(false, true)),
			ReleaseAction::Offer
		);
	}

	#[test]
	fn a_newer_release_is_installed_with_always_update() {
		assert_eq!(
			release_action(&version("0.2.9"), &version("1.0.0"), started(true, false)),
			ReleaseAction::Install
		);
	}

	#[test]
	fn the_running_release_is_left_alone() {
		assert_eq!(
			release_action(&version("0.2.9"), &version("0.2.9"), started(true, true)),
			ReleaseAction::Nothing
		);
	}

	#[test]
	fn an_older_release_is_never_offered_or_installed() {
		for (automatically_update, is_repl) in [(false, false), (false, true), (true, false)] {
			assert_eq!(
				release_action(
					&version("0.2.10"),
					&version("0.2.9"),
					started(automatically_update, is_repl)
				),
				ReleaseAction::Nothing
			);
		}
	}

	#[test]
	fn a_release_is_newer_than_a_prerelease_of_it() {
		assert_eq!(
			release_action(
				&version("0.3.0-beta.1"),
				&version("0.3.0"),
				started(true, false)
			),
			ReleaseAction::Install
		);
	}

	#[test]
	fn build_metadata_does_not_make_a_release_newer() {
		assert_eq!(
			release_action(
				&version("0.2.9"),
				&version("0.2.9+build.2"),
				started(true, false)
			),
			ReleaseAction::Nothing
		);
	}

	#[tokio::test]
	async fn an_older_latest_release_is_not_announced() {
		let printed = Printed::default();
		let mut ui = printed.terminal_ui();

		run_update_check(
			&mut ui,
			"0.2.10",
			started(false, false),
			None,
			async || (Ok(release("v0.2.9")), Ok(Vec::new())),
			not_asked,
		)
		.await;

		assert_eq!(printed.lines_or_empty(), "");
	}

	#[tokio::test]
	async fn an_unparsable_release_tag_is_a_warning() {
		let printed = Printed::default();
		let mut ui = printed.ui();

		run_update_check(
			&mut ui,
			"0.2.9",
			started(true, false),
			None,
			async || (Ok(release("nightly")), Ok(Vec::new())),
			not_asked,
		)
		.await;

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"⚠ Update check failed: Failed to parse the latest release's version nightly: \
			 unexpected character 'n' while parsing major version number"
		);
	}

	#[tokio::test]
	async fn a_forced_check_writes_no_stamp() {
		let config_dir = tempfile::tempdir().unwrap();
		let printed = Printed::default();
		let mut ui = printed.ui();

		run_update_check(
			&mut ui,
			"0.2.9",
			started(false, false),
			None,
			async || (Ok(release("v0.2.9")), Ok(Vec::new())),
			not_asked,
		)
		.await;

		assert_eq!(printed.lines_or_empty(), "");
		assert_eq!(written_stamp(config_dir.path()), None);
	}

	/// Serves one HTTP request on a local port with `response`, and returns its URL.
	fn serve_once(response: &'static str) -> String {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		std::thread::spawn(move || {
			let (mut stream, _) = listener.accept().unwrap();
			let mut request = [0; 1024];
			let _ = stream.read(&mut request).unwrap();
			stream.write_all(response.as_bytes()).unwrap();
		});
		format!("http://{address}/asset")
	}

	#[tokio::test]
	async fn a_download_answered_with_an_error_status_fails() {
		let url = serve_once(
			"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found",
		);

		let error = download_file(&url).await.unwrap_err();

		assert_eq!(
			error.to_string(),
			format!("Downloading {url} returned 404 Not Found: Not Found")
		);
	}

	#[tokio::test]
	async fn a_successful_download_returns_the_body() {
		let url =
			serve_once("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nbinary");

		assert_eq!(download_file(&url).await.unwrap(), b"binary");
	}

	#[tokio::test]
	async fn an_announcements_fetch_answered_with_an_error_status_fails() {
		let url = serve_once(
			"HTTP/1.1 404 Not Found\r\nContent-Length: 14\r\nConnection: close\r\n\r\n404: Not Found",
		);

		let error = GitHubApiClient::new()
			.get_announcements_from(&url)
			.await
			.unwrap_err();

		assert_eq!(
			error.to_string(),
			"Fetching announcements returned 404 Not Found: 404: Not Found"
		);
	}

	#[test]
	fn announcements_are_parsed_one_per_line() {
		let announcements =
			parse_announcements(">=0.2.0, <0.3.0: Please update: a bug was fixed\n*: Hello\n");

		let parsed = announcements
			.iter()
			.map(|a| (a.version_range.to_string(), a.message.as_str()))
			.collect::<Vec<_>>();
		assert_eq!(
			parsed,
			[
				(
					">=0.2.0, <0.3.0".to_owned(),
					"Please update: a bug was fixed"
				),
				("*".to_owned(), "Hello"),
			]
		);
	}

	#[test]
	fn announcement_lines_without_a_version_range_are_skipped() {
		let announcements =
			parse_announcements("no separator\nnot a range: message\n\n=0.2.9: For 0.2.9\n");

		assert_eq!(announcements.len(), 1);
		assert_eq!(announcements[0].message, "For 0.2.9");
		assert!(announcements[0].version_range.matches(&version("0.2.9")));
		assert!(!announcements[0].version_range.matches(&version("0.2.10")));
	}

	/// An asset of `size` bytes with `digest`.
	fn asset(size: Option<u64>, digest: Option<&str>) -> GitHubReleaseAsset {
		GitHubReleaseAsset {
			name: "filen-cli-v0.3.0-linux-x64".to_owned(),
			browser_download_url: "https://example.com/asset".to_owned(),
			digest: digest.map(str::to_owned),
			size,
		}
	}

	const SHA256_OF_BINARY: &str =
		"sha256:9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9cd";

	#[test]
	fn an_asset_without_a_digest_deserializes() {
		let asset: GitHubReleaseAsset = serde_json::from_str(
			r#"{"name": "filen-cli-v0.2.0-linux-x64", "browser_download_url": "https://example.com/a", "size": 6}"#,
		)
		.unwrap();
		assert_eq!(asset.digest, None);
		assert_eq!(asset.size, Some(6));
	}

	#[test]
	fn a_sha256_digest_parses() {
		let AssetDigest::Sha256(digest) = AssetDigest::parse(SHA256_OF_BINARY).unwrap() else {
			panic!("not a SHA-256 digest");
		};
		assert_eq!(
			format!("sha256:{digest}"),
			SHA256_OF_BINARY,
			"the digest prints as it was given"
		);
	}

	#[test]
	fn a_digest_in_another_algorithm_is_unsupported() {
		assert_eq!(
			AssetDigest::parse("sha512:abcd").unwrap(),
			AssetDigest::Unsupported("sha512")
		);
	}

	#[test]
	fn a_malformed_sha256_digest_is_an_error() {
		for digest in [
			"sha256",
			"sha256:abc",
			"sha256:9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9c",
			"sha256:9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9cdff",
			"sha256:9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9cg",
		] {
			assert!(AssetDigest::parse(digest).is_err(), "{digest}");
		}
	}

	#[test]
	fn a_download_of_the_listed_size_and_digest_passes() {
		verify_download(&asset(Some(6), Some(SHA256_OF_BINARY)), b"binary").unwrap();
	}

	#[test]
	fn a_download_of_another_size_fails() {
		let error = verify_download(&asset(Some(7), None), b"binary").unwrap_err();
		assert_eq!(
			error.to_string(),
			"The download of filen-cli-v0.3.0-linux-x64 is 6 bytes, but its release lists 7 bytes"
		);
	}

	#[test]
	fn a_download_with_another_digest_fails() {
		let error =
			verify_download(&asset(Some(6), Some(SHA256_OF_BINARY)), b"bin4ry").unwrap_err();
		assert_eq!(
			error.to_string(),
			"The download of filen-cli-v0.3.0-linux-x64 has the SHA-256 digest \
			 2dfce159e56ebc75f14698bc654e14aa3dd21f3db365866a219e8ff481299242, but its release lists \
			 9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9cd"
		);
	}

	#[test]
	fn a_download_with_an_unsupported_digest_fails() {
		let error = verify_download(&asset(None, Some("sha512:abcd")), b"binary").unwrap_err();
		assert_eq!(
			error.to_string(),
			"The release lists a sha512 digest for filen-cli-v0.3.0-linux-x64, which cannot be \
			 checked"
		);
	}

	#[test]
	fn a_download_with_a_malformed_digest_fails() {
		assert!(verify_download(&asset(None, Some("sha256:00")), b"binary").is_err());
	}

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
