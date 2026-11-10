//! The updater and `filen install` against releases served from a local server, whose release
//! asset is the CLI's own binary: no GitHub and no account, so they run anywhere.
//!
//! `FILEN_CLI_TESTING_MOCK_VERSION` sets the version the CLI runs as, which also lets a debug
//! build update and makes it read the release URLs from `FILEN_CLI_TESTING_RELEASES_URL` and
//! `FILEN_CLI_TESTING_ANNOUNCEMENTS_URL`.

use std::{
	env::consts::EXE_SUFFIX,
	fs,
	net::TcpListener,
	path::{Path, PathBuf},
	sync::{Arc, Mutex},
	thread::{self, JoinHandle},
};

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tiny_http::{Header, Response, Server};

/// The target the CLI was built for, which names its release asset.
const TARGET: &str = env!("BUILD_TARGET");

/// What the mock serves.
#[derive(Clone, Copy)]
struct Releases {
	/// The tag of `releases/latest`.
	latest: &'static str,
	/// The tags `releases/tags/<tag>` finds; any other is not found.
	tags: &'static [&'static str],
	/// The status every `releases/...` request is answered with; one that is not 200 comes with
	/// an error body instead of a release.
	api_status: u16,
	/// The status the asset download is answered with.
	asset_status: u16,
	/// The digest the release lists for its asset.
	digest: ListedDigest,
}

/// The digest a release lists for its asset.
#[derive(Clone, Copy)]
enum ListedDigest {
	/// None, as on older releases.
	Missing,
	/// The SHA-256 digest of the asset served.
	OfAsset,
	/// This one.
	Given(&'static str),
}

impl Default for Releases {
	fn default() -> Self {
		Self {
			latest: "v1.0.0",
			tags: &["v0.1.0", "v1.0.0"],
			api_status: 200,
			asset_status: 200,
			digest: ListedDigest::Missing,
		}
	}
}

/// The release asset of `tag`: the CLI's binary, with a trailer that tells the releases apart.
fn asset(tag: &str) -> Vec<u8> {
	let mut asset = fs::read(assert_cmd::cargo::cargo_bin!()).unwrap();
	asset.extend_from_slice(format!("\nfilen-cli-test-asset-{tag}").as_bytes());
	asset
}

/// The SHA-256 digest of `bytes` in lowercase hex, as the updater prints and GitHub lists it.
fn sha256_hex(bytes: &[u8]) -> String {
	Sha256::digest(bytes)
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect()
}

/// A local server answering for GitHub, until it is dropped.
struct MockServer {
	server: Arc<Server>,
	url: String,
	/// The paths requested, in order.
	requests: Arc<Mutex<Vec<String>>>,
	thread: Option<JoinHandle<()>>,
}

impl MockServer {
	fn start(releases: Releases) -> Self {
		let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
		let url = format!("http://{}", server.server_addr().to_ip().unwrap());
		let requests = Arc::new(Mutex::new(Vec::new()));
		let thread = thread::spawn({
			let server = Arc::clone(&server);
			let url = url.clone();
			let requests = Arc::clone(&requests);
			move || {
				for request in server.incoming_requests() {
					let path = request.url().to_owned();
					let (status, body) = respond(releases, &url, &path);
					requests.lock().unwrap().push(path);
					let response = Response::from_data(body)
						.with_status_code(status)
						.with_header(
							Header::from_bytes("Content-Type", "application/json").unwrap(),
						);
					let _ = request.respond(response);
				}
			}
		});
		Self {
			server,
			url,
			requests,
			thread: Some(thread),
		}
	}

	fn requests(&self) -> Vec<String> {
		self.requests.lock().unwrap().clone()
	}
}

impl Drop for MockServer {
	fn drop(&mut self) {
		self.server.unblock();
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

/// The status and the body of the answer to `path`.
fn respond(releases: Releases, url: &str, path: &str) -> (u16, Vec<u8>) {
	let release = |tag: &str| {
		if releases.api_status != 200 {
			return (
				releases.api_status,
				br#"{"message": "Unavailable"}"#.to_vec(),
			);
		}
		let asset = asset(tag);
		let digest = match releases.digest {
			ListedDigest::Missing => None,
			ListedDigest::OfAsset => Some(format!("sha256:{}", sha256_hex(&asset))),
			ListedDigest::Given(digest) => Some(digest.to_owned()),
		};
		let release = json!({
			"prerelease": false,
			"tag_name": tag,
			"body": format!("Notes for {tag}"),
			"assets": [
				{
					// an asset for another target comes first, and its download fails
					"name": format!("filen-cli-{tag}-another-target"),
					"browser_download_url": format!("{url}/download/another-target"),
					"size": 1,
				},
				{
					"name": format!("filen-cli-{tag}-{TARGET}"),
					"browser_download_url": format!("{url}/download/{tag}"),
					"digest": digest,
					"size": asset.len(),
				},
			],
		});
		(200, release.to_string().into_bytes())
	};
	let not_found = (404, br#"{"message": "Not Found"}"#.to_vec());
	match path {
		"/announcements" => (200, b"=0.0.0-test: A test announcement\n".to_vec()),
		"/releases/latest" => release(releases.latest),
		path if let Some(tag) = path.strip_prefix("/releases/tags/") => {
			if releases.tags.contains(&tag) {
				release(tag)
			} else {
				not_found
			}
		}
		"/download/another-target" => (500, b"wrong asset".to_vec()),
		path if let Some(tag) = path.strip_prefix("/download/") => match releases.asset_status {
			200 => (200, asset(tag)),
			status => (status, b"Not Found".to_vec()),
		},
		_ => not_found,
	}
}

/// A copy of the CLI's binary named `name`, in a directory of its own, and a config directory.
struct Installation {
	bin_dir: TempDir,
	executable: PathBuf,
	config_dir: TempDir,
	/// The binary as it was copied.
	original: Vec<u8>,
}

impl Installation {
	fn new(name: &str) -> Self {
		let bin_dir = tempfile::tempdir().unwrap();
		let executable = bin_dir.path().join(format!("{name}{EXE_SUFFIX}"));
		fs::copy(assert_cmd::cargo::cargo_bin!(), &executable).unwrap();
		Self {
			original: fs::read(&executable).unwrap(),
			bin_dir,
			executable,
			config_dir: tempfile::tempdir().unwrap(),
		}
	}

	/// Runs the binary as `mock_version` against the releases `url` serves.
	fn cli(&self, url: &str, mock_version: &str) -> Command {
		let mut command = Command::new(&self.executable);
		command
			.env("FILEN_CLI_TESTING_MOCK_VERSION", mock_version)
			.env("FILEN_CLI_TESTING_RELEASES_URL", format!("{url}/releases"))
			.env(
				"FILEN_CLI_TESTING_ANNOUNCEMENTS_URL",
				format!("{url}/announcements"),
			)
			.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
			.env("FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG", "1")
			.arg("--config-dir")
			.arg(self.config_dir.path());
		command
	}

	/// The names of the files next to the binary, sorted.
	fn bin_dir_files(&self) -> Vec<String> {
		file_names(self.bin_dir.path())
	}
}

fn file_names(dir: &Path) -> Vec<String> {
	let mut names = fs::read_dir(dir)
		.unwrap()
		.map(|entry| entry.unwrap().file_name().into_string().unwrap())
		.collect::<Vec<_>>();
	names.sort();
	names
}

#[test]
fn always_update_installs_the_latest_release_under_its_versioned_name() {
	let server = MockServer::start(Releases::default());
	let installation = Installation::new(&format!("filen-cli-v0.0.0-test-{TARGET}"));

	installation
		.cli(&server.url, "0.0.0-test")
		.args(["--always-update", "exit"])
		.assert()
		.success()
		.stderr(contains(
			"Automatically updating from v0.0.0-test to v1.0.0...",
		))
		.stderr(contains("Downloaded update to"));

	let updated = installation
		.bin_dir
		.path()
		.join(format!("filen-cli-v1.0.0-{TARGET}{EXE_SUFFIX}"));
	assert_eq!(fs::read(updated).unwrap(), asset("v1.0.0"));
	// Windows deletes the running binary only once it exits, in the background
	#[cfg(unix)]
	assert_eq!(
		installation.bin_dir_files(),
		[format!("filen-cli-v1.0.0-{TARGET}")]
	);
}

#[test]
fn always_update_replaces_a_binary_without_a_version_in_its_name() {
	let server = MockServer::start(Releases::default());
	let installation = Installation::new("filen-cli");

	installation
		.cli(&server.url, "0.0.0-test")
		.args(["--always-update", "exit"])
		.assert()
		.success()
		.stderr(contains("Update installed successfully"));

	assert_eq!(fs::read(&installation.executable).unwrap(), asset("v1.0.0"));
	assert_eq!(
		installation.bin_dir_files(),
		[format!("filen-cli{EXE_SUFFIX}")]
	);
}

#[test]
fn the_update_check_of_a_failing_command_is_reported_after_its_failure() {
	let server = MockServer::start(Releases::default());
	let installation = Installation::new("filen-cli");
	let missing = installation.config_dir.path().join("missing");

	let assert = installation
		.cli(&server.url, "0.0.0-test")
		.arg("--always-update")
		.arg("lls")
		.arg(&missing)
		.assert()
		.failure();

	let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
	let failure = stderr
		.find(&format!("Failed to list {}", missing.display()))
		.unwrap_or_else(|| panic!("no failure in {stderr}"));
	let installed = stderr
		.find("Update installed successfully")
		.unwrap_or_else(|| panic!("no update in {stderr}"));
	assert!(failure < installed, "{stderr}");
	assert_eq!(fs::read(&installation.executable).unwrap(), asset("v1.0.0"));
}

#[test]
fn the_announcements_for_the_running_version_are_shown() {
	let server = MockServer::start(Releases::default());
	let installation = Installation::new("filen-cli");

	// only a terminal shows them, while the log has them at any verbosity
	installation
		.cli(&server.url, "0.0.0-test")
		.args(["-v", "exit"])
		.assert()
		.success()
		.stderr(contains("[ANNOUNCEMENT] A test announcement"));
}

#[test]
fn a_check_is_skipped_for_a_while_after_one() {
	let server = MockServer::start(Releases::default());
	let installation = Installation::new("filen-cli");

	installation
		.cli(&server.url, "1.0.0")
		.args(["-vv", "exit"])
		.assert()
		.success()
		.stderr(contains("Up to date: v1.0.0"))
		.stderr(contains("Wrote last update check timestamp"));
	installation
		.cli(&server.url, "1.0.0")
		.args(["-vv", "exit"])
		.assert()
		.success()
		.stderr(contains("Skipping update check; last checked"));

	assert_eq!(
		server
			.requests()
			.iter()
			.filter(|path| *path == "/releases/latest")
			.count(),
		1
	);
	assert_eq!(
		fs::read(&installation.executable).unwrap(),
		installation.original
	);
}

/// A URL nothing listens on.
fn refusing_url() -> String {
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let url = format!("http://{}", listener.local_addr().unwrap());
	drop(listener);
	url
}

#[test]
fn an_offline_update_check_is_a_warning() {
	let installation = Installation::new("filen-cli");

	installation
		.cli(&refusing_url(), "0.0.0-test")
		.args(["--always-update", "exit"])
		.assert()
		.success()
		.stderr(contains(
			"Update check failed: Failed to send request to GitHub API for releases",
		))
		.stderr(contains("Failed to fetch announcements"));

	assert_eq!(
		fs::read(&installation.executable).unwrap(),
		installation.original
	);
}

#[test]
fn an_offline_update_check_prints_nothing_when_quiet_or_with_json_output() {
	let installation = Installation::new("filen-cli");

	for flag in ["--quiet", "--json"] {
		installation
			.cli(&refusing_url(), "0.0.0-test")
			.args(["--always-update", flag, "exit"])
			.assert()
			.success()
			.stderr("");
	}
}

#[test]
fn an_error_status_from_the_releases_api_is_a_warning() {
	let server = MockServer::start(Releases {
		api_status: 503,
		..Releases::default()
	});
	let installation = Installation::new("filen-cli");

	installation
		.cli(&server.url, "0.0.0-test")
		.args(["--always-update", "exit"])
		.assert()
		.success()
		.stderr(contains(
			"Update check failed: GitHub API returned 503 Service Unavailable for releases: \
			 {\"message\": \"Unavailable\"}",
		));

	assert_eq!(
		fs::read(&installation.executable).unwrap(),
		installation.original
	);
}

#[test]
fn a_failed_asset_download_leaves_the_binary_alone() {
	let server = MockServer::start(Releases {
		asset_status: 404,
		..Releases::default()
	});
	let installation = Installation::new("filen-cli");

	installation
		.cli(&server.url, "0.0.0-test")
		.args(["--always-update", "exit"])
		.assert()
		.success()
		.stderr(contains(format!(
			"Update check failed: Downloading {}/download/v1.0.0 returned 404 Not Found: Not Found",
			server.url
		)));

	assert_eq!(
		fs::read(&installation.executable).unwrap(),
		installation.original
	);
	assert_eq!(
		installation.bin_dir_files(),
		[format!("filen-cli{EXE_SUFFIX}")]
	);
}

#[test]
fn an_asset_matching_its_digest_is_installed() {
	let server = MockServer::start(Releases {
		digest: ListedDigest::OfAsset,
		..Releases::default()
	});
	let installation = Installation::new("filen-cli");

	installation
		.cli(&server.url, "0.0.0-test")
		.args(["--always-update", "exit"])
		.assert()
		.success()
		.stderr(contains("Update installed successfully."));

	assert_eq!(fs::read(&installation.executable).unwrap(), asset("v1.0.0"));
}

#[test]
fn an_asset_not_matching_its_digest_is_not_installed() {
	let server = MockServer::start(Releases {
		digest: ListedDigest::Given(
			"sha256:0000000000000000000000000000000000000000000000000000000000000000",
		),
		..Releases::default()
	});
	let installation = Installation::new("filen-cli");

	installation
		.cli(&server.url, "0.0.0-test")
		.args(["--always-update", "exit"])
		.assert()
		.success()
		.stderr(contains(format!(
			"Update check failed: The download of filen-cli-v1.0.0-{TARGET} has the SHA-256 digest \
			 {}, but its release lists \
			 0000000000000000000000000000000000000000000000000000000000000000",
			sha256_hex(&asset("v1.0.0"))
		)));

	assert_eq!(
		fs::read(&installation.executable).unwrap(),
		installation.original
	);
	assert_eq!(
		installation.bin_dir_files(),
		[format!("filen-cli{EXE_SUFFIX}")]
	);
}

#[test]
fn an_asset_with_a_digest_in_another_algorithm_is_not_installed() {
	let server = MockServer::start(Releases {
		digest: ListedDigest::Given("sha512:abcd"),
		..Releases::default()
	});
	let installation = Installation::new("filen-cli");

	installation
		.cli(&server.url, "0.0.0-test")
		.args(["--always-update", "exit"])
		.assert()
		.success()
		.stderr(contains(format!(
			"Update check failed: The release lists a sha512 digest for \
			 filen-cli-v1.0.0-{TARGET}, which cannot be checked"
		)));

	assert_eq!(
		fs::read(&installation.executable).unwrap(),
		installation.original
	);
}

#[test]
fn an_asset_with_a_malformed_digest_is_not_installed() {
	let server = MockServer::start(Releases {
		digest: ListedDigest::Given("sha256:00"),
		..Releases::default()
	});
	let installation = Installation::new("filen-cli");

	installation
		.cli(&server.url, "0.0.0-test")
		.args(["--always-update", "exit"])
		.assert()
		.success()
		.stderr(contains(
			"Update check failed: Malformed release asset SHA-256 digest \"sha256:00\"",
		));

	assert_eq!(
		fs::read(&installation.executable).unwrap(),
		installation.original
	);
}

#[test]
fn install_downgrades_to_an_older_release() {
	let server = MockServer::start(Releases::default());
	let installation = Installation::new("filen-cli");

	installation
		.cli(&server.url, "2.0.0")
		.args(["install", "v0.1.0", "--yes"])
		.assert()
		.success()
		.stdout(contains("Installed v0.1.0."));

	assert_eq!(fs::read(&installation.executable).unwrap(), asset("v0.1.0"));
	// no update check runs before it
	assert_eq!(
		server.requests(),
		["/releases/tags/v0.1.0", "/download/v0.1.0"]
	);
}

#[test]
fn install_renames_a_binary_with_a_version_in_its_name() {
	let server = MockServer::start(Releases::default());
	let installation = Installation::new(&format!("filen-cli-v1.0.0-{TARGET}"));

	installation
		.cli(&server.url, "1.0.0")
		.args(["install", "0.1.0", "--yes"])
		.assert()
		.success()
		.stdout(contains("Installed v0.1.0 to"));

	let installed = installation
		.bin_dir
		.path()
		.join(format!("filen-cli-v0.1.0-{TARGET}{EXE_SUFFIX}"));
	assert_eq!(fs::read(installed).unwrap(), asset("v0.1.0"));
	// Windows deletes the running binary only once it exits, in the background
	#[cfg(unix)]
	assert_eq!(
		installation.bin_dir_files(),
		[format!("filen-cli-v0.1.0-{TARGET}")]
	);
}

#[test]
fn install_reinstalls_the_running_release_over_a_binary_with_its_version_in_its_name() {
	let server = MockServer::start(Releases::default());
	let name = format!("filen-cli-v1.0.0-{TARGET}{EXE_SUFFIX}");
	let installation = Installation::new(&format!("filen-cli-v1.0.0-{TARGET}"));

	installation
		.cli(&server.url, "1.0.0")
		.args(["install", "latest", "--yes"])
		.assert()
		.success()
		.stdout(contains("Installed v1.0.0."));

	assert_eq!(fs::read(&installation.executable).unwrap(), asset("v1.0.0"));
	assert_eq!(installation.bin_dir_files(), [name]);
}

#[test]
fn install_of_a_release_that_does_not_exist_fails() {
	let server = MockServer::start(Releases::default());
	let installation = Installation::new("filen-cli");

	installation
		.cli(&server.url, "1.0.0")
		.args(["install", "v7.7.7", "--yes"])
		.assert()
		.code(1)
		.stderr(contains("There is no release v7.7.7"));

	assert_eq!(
		fs::read(&installation.executable).unwrap(),
		installation.original
	);
}

#[test]
fn install_without_a_terminal_needs_yes() {
	let server = MockServer::start(Releases::default());
	let installation = Installation::new("filen-cli");

	installation
		.cli(&server.url, "1.0.0")
		.args(["install", "v0.1.0"])
		.assert()
		.code(2)
		.stderr(contains(
			"Cannot ask \"Install v0.1.0\": stdin is not a terminal. Pass --yes to answer yes.",
		));

	assert_eq!(server.requests(), Vec::<String>::new());
	assert_eq!(
		fs::read(&installation.executable).unwrap(),
		installation.original
	);
}

#[test]
fn install_of_something_that_is_not_a_version_is_a_usage_error() {
	let server = MockServer::start(Releases::default());
	let installation = Installation::new("filen-cli");

	installation
		.cli(&server.url, "1.0.0")
		.args(["install", "banana", "--yes"])
		.assert()
		.code(2)
		.stderr(contains("invalid value 'banana'"));

	assert_eq!(server.requests(), Vec::<String>::new());
}

#[test]
fn version_runs_without_an_account_or_an_update_check() {
	let server = MockServer::start(Releases::default());
	let installation = Installation::new("filen-cli");

	let output = installation
		.cli(&server.url, "0.0.0-test")
		.args(["version", "--json"])
		.assert()
		.success()
		.get_output()
		.stdout
		.clone();

	let printed: serde_json::Value = serde_json::from_slice(&output).unwrap();
	assert_eq!(printed["version"], env!("CARGO_PKG_VERSION"));
	assert_eq!(printed["target"], TARGET);
	assert_eq!(
		printed["configDir"],
		installation.config_dir.path().to_str().unwrap()
	);
	assert_eq!(server.requests(), Vec::<String>::new());
}

#[test]
fn the_log_names_the_environment() {
	let server = MockServer::start(Releases::default());
	let installation = Installation::new("filen-cli");

	installation
		.cli(&server.url, "0.0.0-test")
		.args(["-v", "version"])
		.assert()
		.success()
		.stderr(contains(format!(
			"Target: {TARGET}, config directory: {}, Docker image: no",
			installation.config_dir.path().display()
		)));
}
