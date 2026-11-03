//! `serve` and `mount` as a user runs them: the CLI is a child process, logged into the test
//! account, and stopped with Ctrl-C. They download the managed Rclone into a config directory every
//! test shares, so that it is downloaded once per target directory. Unix only: Ctrl-C is sent as
//! SIGINT.
#![cfg(unix)]

use std::{
	net::TcpListener,
	path::PathBuf,
	process::{ExitStatus, Stdio},
	sync::{LazyLock, Mutex, PoisonError},
	thread,
	time::{Duration, Instant},
};

use assert_fs::{TempDir, prelude::*};
use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	fs::HasName as _,
	rclone::{FuseStatus, fuse_status},
};
use regex::Regex;
use reqwest::{Method, StatusCode};
use serde_json::Value;
use test_utils::cli::prepare_cli_auth_config;
use tokio::{
	io::{AsyncBufReadExt, AsyncReadExt, BufReader, Lines},
	process::{Child, ChildStdout, Command},
	sync::{Semaphore, SemaphorePermit},
	task::JoinHandle,
	time::timeout,
};

/// How long a server may take to start, the managed Rclone's download included.
const START: Duration = Duration::from_secs(180);
/// How long the CLI may take to exit once it stopped or failed, 30 s of draining uploads included.
const EXIT: Duration = Duration::from_secs(60);

/// Held while a CLI is spawned: on macOS a pipe is marked close-on-exec only after it is created,
/// so a CLI another test spawned in between would inherit this test's pipes and keep them open.
static SPAWNING: Mutex<()> = Mutex::new(());

/// How many mounts and servers the CLI runs at once on one config directory, each on a cache of its
/// own; another fails until one stops.
const CLI_CACHES: usize = 8;

/// A permit per CLI running at once, so that the tests, which share one config directory, never
/// run more CLIs than it has caches.
static RUNNING: Semaphore = Semaphore::const_new(CLI_CACHES);

/// What a WebDAV server's start line says, with its generated credentials.
static WEBDAV_STARTED: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(
		r#"Started WebDAV server on (http://\S+) with username "([^"]+)" and password "([^"]+)" \(press Ctrl-C to stop\)"#,
	)
	.unwrap()
});

/// The config directory every test's CLI uses: one managed Rclone, and the caches the CLI picks
/// one of per mount or server. Created if missing, as the CLI takes only an existing one.
fn config_dir() -> PathBuf {
	let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("filen-cli-servers-tests");
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

/// A port nothing listens on, from a listener bound and dropped again.
fn free_port() -> u16 {
	TcpListener::bind("127.0.0.1:0")
		.unwrap()
		.local_addr()
		.unwrap()
		.port()
}

/// A running CLI, interrupted and then killed if a test leaves it running, so that no server
/// outlives a failed test.
struct Cli {
	child: Child,
	stdout: Lines<BufReader<ChildStdout>>,
	stderr: JoinHandle<String>,
	/// The account's credentials, which the CLI reads when it starts.
	_auth_config: TempDir,
	/// Released once the CLI has exited, when the guard is dropped.
	_running: SemaphorePermit<'static>,
}

impl Cli {
	/// Starts `filen <args>` logged into the test account.
	async fn spawn(args: &[&str]) -> Self {
		let running = RUNNING.acquire().await.unwrap();
		let (auth_config, auth_config_file) = prepare_cli_auth_config().await;
		let mut command = Command::new(assert_cmd::cargo::cargo_bin!("filen-cli"));
		command
			.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
			.env("FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG", "1")
			.env_remove("FILEN_CLI_EMAIL")
			.env_remove("FILEN_CLI_PASSWORD")
			.env_remove("FILEN_CLI_2FA_CODE")
			.env_remove("FILEN_CLI_SERVE_PASSWORD")
			.arg("--config-dir")
			.arg(config_dir())
			.arg("--auth-config-path")
			.arg(auth_config_file.path())
			.arg("--skip-update")
			.arg("--no-log-file")
			.args(args)
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped());
		let mut child = {
			let _spawning = SPAWNING.lock().unwrap_or_else(PoisonError::into_inner);
			command.spawn().unwrap()
		};
		let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
		let mut stderr = child.stderr.take().unwrap();
		let stderr = tokio::spawn(async move {
			let mut read = String::new();
			stderr.read_to_string(&mut read).await.unwrap();
			read
		});
		Self {
			child,
			stdout,
			stderr,
			_auth_config: auth_config,
			_running: running,
		}
	}

	/// The first line on stdout that contains `needed`; fails with the CLI's exit and stderr when
	/// the CLI exits before printing it.
	async fn line_containing(&mut self, needed: &str) -> String {
		let found = timeout(START, async {
			while let Some(line) = self.stdout.next_line().await.unwrap() {
				if line.contains(needed) {
					return Some(line);
				}
			}
			None
		})
		.await
		.unwrap_or_else(|_| panic!("the CLI printed no line with {needed:?} within {START:?}"));
		match found {
			Some(line) => line,
			None => {
				let (status, stderr) = self.exit().await;
				panic!("the CLI exited ({status}) without printing {needed:?}; stderr: {stderr}");
			}
		}
	}

	/// The JSON object printed first on stdout, pretty-printed over several lines.
	async fn json_object(&mut self) -> Value {
		let opening = self.line_containing("{").await;
		assert_eq!(opening, "{", "the JSON does not start on a line of its own");
		let mut json = opening;
		loop {
			let line = timeout(START, self.stdout.next_line())
				.await
				.unwrap_or_else(|_| panic!("the CLI did not finish its JSON within {START:?}"))
				.unwrap()
				.unwrap_or_else(|| panic!("stdout ended inside the JSON: {json}"));
			let end = line == "}";
			json.push_str(&line);
			if end {
				return serde_json::from_str(&json).unwrap();
			}
		}
	}

	/// The JSON array printed first on stdout, pretty-printed over several lines.
	async fn json_array(&mut self) -> Value {
		let opening = self.line_containing("[").await;
		assert_eq!(opening, "[", "the JSON does not start on a line of its own");
		let mut json = opening;
		loop {
			let line = timeout(START, self.stdout.next_line())
				.await
				.unwrap_or_else(|_| panic!("the CLI did not finish its JSON within {START:?}"))
				.unwrap()
				.unwrap_or_else(|| panic!("stdout ended inside the JSON: {json}"));
			let end = line == "]";
			json.push_str(&line);
			if end {
				return serde_json::from_str(&json).unwrap();
			}
		}
	}

	/// What is left on stdout until the CLI closes it.
	async fn rest_of_stdout(&mut self) -> Vec<String> {
		let mut lines = Vec::new();
		while let Some(line) = timeout(EXIT, self.stdout.next_line())
			.await
			.expect("stdout closes when the CLI exits")
			.unwrap()
		{
			lines.push(line);
		}
		lines
	}

	/// Presses Ctrl-C.
	fn interrupt(&self) {
		let pid = self.child.id().expect("the CLI is still running");
		let status = std::process::Command::new("kill")
			.args(["-INT", &pid.to_string()])
			.status()
			.unwrap();
		assert!(status.success(), "kill -INT {pid}: {status}");
	}

	/// Waits for the CLI to exit, and gives its exit status and stderr.
	async fn exit(&mut self) -> (ExitStatus, String) {
		let status = timeout(EXIT, self.child.wait())
			.await
			.unwrap_or_else(|_| panic!("the CLI did not exit within {EXIT:?}"))
			.unwrap();
		let stderr = timeout(EXIT, &mut self.stderr)
			.await
			.expect("stderr closes when the CLI exits")
			.unwrap();
		(status, stderr)
	}
}

impl Drop for Cli {
	fn drop(&mut self) {
		if !matches!(self.child.try_wait(), Ok(None)) {
			return;
		}
		// Ctrl-C first, so that the CLI stops its Rclone; killed when it does not exit in time, after
		// which Rclone's watchdog stops it.
		if let Some(pid) = self.child.id() {
			let _ = std::process::Command::new("kill")
				.args(["-INT", &pid.to_string()])
				.status();
		}
		let deadline = Instant::now() + EXIT;
		while Instant::now() < deadline {
			if !matches!(self.child.try_wait(), Ok(None)) {
				return;
			}
			thread::sleep(Duration::from_millis(100));
		}
		let _ = self.child.start_kill();
	}
}

/// A server's address and the credentials it printed.
struct Started {
	url: String,
	user: String,
	password: String,
}

impl Started {
	/// What a WebDAV server's start line `line` says.
	fn webdav(line: &str) -> Self {
		let captures = WEBDAV_STARTED
			.captures(line)
			.unwrap_or_else(|| panic!("not a WebDAV server's start line: {line}"));
		Self {
			url: captures[1].to_owned(),
			user: captures[2].to_owned(),
			password: captures[3].to_owned(),
		}
	}

	/// The status of a PROPFIND of the server's root, with the printed credentials or none.
	async fn propfind_root(&self, with_credentials: bool) -> StatusCode {
		let request = reqwest::Client::new()
			.request(
				Method::from_bytes(b"PROPFIND").unwrap(),
				format!("{}/", self.url),
			)
			.header("Depth", "0");
		let request = if with_credentials {
			request.basic_auth(&self.user, Some(&self.password))
		} else {
			request
		};
		request.send().await.unwrap().status()
	}

	/// The status and body of a GET of `path` with the printed credentials.
	async fn get(&self, path: &str) -> (StatusCode, String) {
		let response = reqwest::Client::new()
			.get(format!("{}/{path}", self.url))
			.basic_auth(&self.user, Some(&self.password))
			.send()
			.await
			.unwrap();
		(response.status(), response.text().await.unwrap())
	}
}

/// Starts `filen serve webdav` on a free port, with `extra` arguments, and waits for it to start.
async fn serve_webdav(extra: &[&str]) -> (Cli, Started, u16) {
	let port = free_port();
	let address = format!("127.0.0.1:{port}");
	let mut args = vec!["serve", "webdav", "--addr", &address];
	args.extend_from_slice(extra);
	let mut cli = Cli::spawn(&args).await;
	let started = Started::webdav(&cli.line_containing("Started WebDAV server").await);
	(cli, started, port)
}

#[shared_test_runtime]
async fn a_webdav_server_refuses_requests_without_its_generated_credentials() {
	let (mut cli, started, port) = serve_webdav(&[]).await;

	assert_eq!(started.url, format!("http://127.0.0.1:{port}"));
	assert_eq!(started.propfind_root(false).await, StatusCode::UNAUTHORIZED);
	assert_eq!(started.propfind_root(true).await, StatusCode::MULTI_STATUS);

	cli.interrupt();
	let (status, stderr) = cli.exit().await;
	assert_eq!(status.code(), Some(0), "{stderr}");
}

#[shared_test_runtime]
async fn a_quiet_server_still_prints_its_generated_credentials() {
	let (mut cli, started, port) = serve_webdav(&["--quiet"]).await;

	assert_eq!(started.url, format!("http://127.0.0.1:{port}"));
	assert_eq!(started.propfind_root(true).await, StatusCode::MULTI_STATUS);

	cli.interrupt();
	let (status, stderr) = cli.exit().await;
	assert_eq!(status.code(), Some(0), "{stderr}");
}

#[shared_test_runtime]
async fn a_server_on_a_port_in_use_fails() {
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let port = listener.local_addr().unwrap().port();

	let mut cli = Cli::spawn(&["serve", "webdav", "--addr", &format!("127.0.0.1:{port}")]).await;
	let (status, stderr) = cli.exit().await;

	assert_eq!(status.code(), Some(1), "{stderr}");
	assert!(
		stderr.contains(&format!(
			"Failed to start WebDAV server: port {port} is in use"
		)),
		"{stderr}"
	);
	drop(listener);
}

#[shared_test_runtime]
async fn two_servers_run_at_once() {
	let (mut first, first_started, _) = serve_webdav(&[]).await;
	let (mut second, second_started, _) = serve_webdav(&[]).await;

	assert_eq!(
		first_started.propfind_root(true).await,
		StatusCode::MULTI_STATUS
	);
	assert_eq!(
		second_started.propfind_root(true).await,
		StatusCode::MULTI_STATUS
	);

	first.interrupt();
	second.interrupt();
	let (first_status, first_stderr) = first.exit().await;
	let (second_status, second_stderr) = second.exit().await;
	assert_eq!(first_status.code(), Some(0), "{first_stderr}");
	assert_eq!(second_status.code(), Some(0), "{second_stderr}");
}

#[shared_test_runtime]
async fn a_mount_on_a_directory_that_is_not_empty_fails() {
	let mount_point = TempDir::new().unwrap();
	mount_point
		.child("file.txt")
		.write_str("in the way")
		.unwrap();

	let mut cli = Cli::spawn(&["mount", mount_point.path().to_str().unwrap()]).await;
	let (status, stderr) = cli.exit().await;

	// FUSE is looked for before the mount point: without it, the mount fails for that.
	let expected = match fuse_status().await {
		FuseStatus::Ready { .. } => {
			"Failed to mount network drive (use --verbose for more info): the mount point is not empty"
		}
		FuseStatus::Missing { .. } => {
			"Failed to mount network drive (use --verbose for more info): no FUSE is installed"
		}
	};
	assert_eq!(status.code(), Some(1), "{stderr}");
	assert!(stderr.contains(expected), "{stderr}");
}

#[shared_test_runtime]
async fn a_server_prints_its_address_and_credentials_as_json_under_json() {
	let port = free_port();
	let address = format!("127.0.0.1:{port}");
	let mut cli = Cli::spawn(&["serve", "webdav", "--addr", &address, "--json"]).await;

	let json = cli.json_object().await;
	assert_eq!(json["protocol"], "webdav");
	assert_eq!(json["address"], address.as_str());
	assert_eq!(json["url"], format!("http://{address}").as_str());
	let started = Started {
		url: json["url"].as_str().unwrap().to_owned(),
		user: json["user"].as_str().unwrap().to_owned(),
		password: json["password"].as_str().unwrap().to_owned(),
	};
	assert_eq!(started.propfind_root(true).await, StatusCode::MULTI_STATUS);

	cli.interrupt();
	// The notices about stopping go to stderr: stdout holds the JSON alone.
	assert_eq!(cli.rest_of_stdout().await, Vec::<String>::new());
	let (status, stderr) = cli.exit().await;
	assert_eq!(status.code(), Some(0), "{stderr}");
	assert!(stderr.contains("Stopping WebDAV server"), "{stderr}");
}

#[shared_test_runtime]
async fn a_server_takes_its_password_from_a_file() {
	let dir = TempDir::new().unwrap();
	let password_file = dir.child("password.txt");
	password_file.write_str("from-the-file\n").unwrap();

	let (mut cli, started, _) = serve_webdav(&[
		"--user",
		"me",
		"--password-file",
		password_file.path().to_str().unwrap(),
	])
	.await;

	assert_eq!(started.user, "me");
	assert_eq!(started.password, "from-the-file");
	assert_eq!(started.propfind_root(true).await, StatusCode::MULTI_STATUS);

	cli.interrupt();
	let (status, stderr) = cli.exit().await;
	assert_eq!(status.code(), Some(0), "{stderr}");
}

#[shared_test_runtime]
async fn a_server_with_a_root_serves_only_that_directory() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let file = client
		.make_file_builder("inside.txt", test_dir.uuid)
		.unwrap();
	client.upload_file(file, b"inside").await.unwrap();
	let root = format!("/{}", test_dir.name().unwrap());

	let (mut cli, started, _) = serve_webdav(&["--root", &root]).await;

	assert_eq!(
		started.get("inside.txt").await,
		(StatusCode::OK, "inside".to_owned())
	);
	assert_eq!(
		started
			.get(&format!("{}/inside.txt", test_dir.name().unwrap()))
			.await
			.0,
		StatusCode::NOT_FOUND
	);
	cli.interrupt();
	let (status, stderr) = cli.exit().await;
	assert_eq!(status.code(), Some(0), "{stderr}");
}

#[shared_test_runtime]
async fn a_server_with_a_root_that_does_not_exist_fails() {
	let root = format!("/filen-cli-servers-tests-missing-{}", free_port());

	let mut cli = Cli::spawn(&[
		"serve",
		"webdav",
		"--addr",
		&format!("127.0.0.1:{}", free_port()),
		"--root",
		&root,
	])
	.await;
	let (status, stderr) = cli.exit().await;

	assert_eq!(status.code(), Some(1), "{stderr}");
	assert!(
		stderr.contains(&format!("No such directory: {root}")),
		"{stderr}"
	);
}

/// The credentials and URL of a server in `serve --json`'s output.
fn started_from_json(server: &Value) -> Started {
	let field = |key: &str| {
		server[key]
			.as_str()
			.unwrap_or_else(|| panic!("no {key} in {server}"))
			.to_owned()
	};
	Started {
		url: field("url"),
		user: field("user"),
		password: field("password"),
	}
}

// On the default ports of WebDAV and HTTP, which no other test here uses: several servers take no
// --addr.
#[shared_test_runtime]
async fn several_servers_run_on_one_command() {
	let mut cli = Cli::spawn(&["serve", "webdav", "http", "--json"]).await;

	let json = cli.json_array().await;
	let servers = json.as_array().unwrap();
	assert_eq!(servers.len(), 2, "{json}");
	assert_eq!(servers[0]["protocol"], "webdav");
	assert_eq!(servers[0]["address"], "127.0.0.1:1900");
	assert_eq!(servers[1]["protocol"], "http");
	assert_eq!(servers[1]["address"], "127.0.0.1:1700");
	let webdav = started_from_json(&servers[0]);
	let http = started_from_json(&servers[1]);
	assert_ne!(webdav.password, http.password);
	assert_eq!(webdav.propfind_root(true).await, StatusCode::MULTI_STATUS);
	assert_eq!(http.get("").await.0, StatusCode::OK);

	cli.interrupt();
	let (status, stderr) = cli.exit().await;
	assert_eq!(status.code(), Some(0), "{stderr}");
	assert!(
		stderr.contains("Stopping WebDAV and HTTP servers"),
		"{stderr}"
	);
}

// On the default ports of FTP and restic, which no other test here uses.
#[shared_test_runtime]
async fn servers_that_started_are_stopped_when_a_later_one_fails() {
	let taken = TcpListener::bind("127.0.0.1:1600").unwrap();

	let mut cli = Cli::spawn(&["serve", "ftp", "restic"]).await;
	let (status, stderr) = cli.exit().await;

	assert_eq!(status.code(), Some(1), "{stderr}");
	assert!(
		stderr.contains("Failed to start restic server: port 1600 is in use"),
		"{stderr}"
	);
	// The FTP server that started first is gone with the CLI's Rclone.
	assert!(TcpListener::bind("127.0.0.1:2121").is_ok());
	drop(taken);
}

// On the default port of SFTP, which no other test here uses.
#[shared_test_runtime]
async fn a_mount_that_fails_after_its_servers_started_stops_them() {
	let mount_point = TempDir::new().unwrap();
	mount_point
		.child("file.txt")
		.write_str("in the way")
		.unwrap();

	let mut cli = Cli::spawn(&[
		"serve",
		"sftp",
		"--mount",
		"--mount-point",
		mount_point.path().to_str().unwrap(),
	])
	.await;
	let (status, stderr) = cli.exit().await;

	// FUSE is looked for before the mount point: without it, the mount fails for that.
	let expected = match fuse_status().await {
		FuseStatus::Ready { .. } => {
			"Failed to mount network drive (use --verbose for more info): the mount point is not empty"
		}
		FuseStatus::Missing { .. } => {
			"Failed to mount network drive (use --verbose for more info): no FUSE is installed"
		}
	};
	assert_eq!(status.code(), Some(1), "{stderr}");
	assert!(stderr.contains(expected), "{stderr}");
	assert!(TcpListener::bind("127.0.0.1:2022").is_ok());
}
