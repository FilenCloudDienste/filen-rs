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
use filen_sdk_rs::rclone::{FuseStatus, fuse_status};
use regex::Regex;
use reqwest::{Method, StatusCode};
use test_utils::cli::prepare_cli_auth_config;
use tokio::{
	io::{AsyncBufReadExt, AsyncReadExt, BufReader, Lines},
	process::{Child, ChildStdout, Command},
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
}

impl Cli {
	/// Starts `filen <args>` logged into the test account.
	async fn spawn(args: &[&str]) -> Self {
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
