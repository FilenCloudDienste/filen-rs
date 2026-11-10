//! rclone run in the foreground with the account as its `filen` remote, for filen-cli's `rclone`
//! command.
//!
//! The remote reaches rclone through `RCLONE_CONFIG_FILEN_*` variables, its password options
//! obscured as a config file holds them, and nothing is written to disk: rclone runs with an
//! in-memory config (`--config ""`). The environment is the one place a remote can be given to a
//! command rclone runs once, and the process it is set on lives only as long as the command.
//!
//! A stop asks rclone to terminate and kills it only after a grace period, as rclone unmounts and
//! removes its partial files when SIGINT or SIGTERM ends it. It is not sent a second signal: one
//! that arrives while its exit handlers run ends it before they are done, leaving a mount behind.

use std::{
	path::Path,
	process::{ExitStatus, Stdio},
	time::Duration,
};

use filen_types::auth::FilenSDKConfig;
use tokio::{
	io::AsyncWriteExt as _,
	process::{Child, Command},
};
use tracing::warn;

use crate::{
	Error, ErrorKind,
	auth::Client,
	error::ResultExt as _,
	rclone::{
		RcloneBinary,
		system::{ProcessIdentity, identify, spawn_rclone, terminate},
	},
};

/// The remote's name, which filen-cli documents.
const REMOTE: &str = "FILEN";

/// Why [`RunningRclone::stop`] stops rclone, which decides whether it is sent SIGTERM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopCause {
	/// Ctrl-C or SIGINT. When rclone is in its terminal's foreground process group the terminal
	/// signalled it too, and it is sent nothing: a second signal while it runs its exit handlers
	/// ends it before it unmounts or removes its partial files.
	Interrupt,
	/// SIGTERM, SIGHUP, or any other request that reached this process alone: rclone is sent
	/// SIGTERM.
	Terminate,
}

/// An rclone command running in the foreground ([`run_rclone`]). Dropping it kills rclone.
pub struct RunningRclone {
	child: Child,
	/// `None` when rclone had exited before it could be identified.
	identity: Option<ProcessIdentity>,
}

/// Starts `rclone <args>` with `client`'s account as the `filen` remote, inheriting the terminal.
pub async fn run_rclone(
	client: &Client,
	binary: &RcloneBinary,
	args: &[String],
) -> Result<RunningRclone, Error> {
	let binary = binary.resolve().await?;
	let remote = remote_env(&binary, &client.to_sdk_config()).await?;
	RunningRclone::spawn(&mut passthrough_command(
		&binary,
		args,
		remote,
		own_process_group(),
	))
	.await
}

/// [`run_rclone`] without an account: no `filen` remote, so a test runs rclone on local paths.
#[cfg(feature = "test-seams")]
pub async fn run_rclone_local_for_test(
	binary: &RcloneBinary,
	args: &[String],
) -> Result<RunningRclone, Error> {
	let binary = binary.resolve().await?;
	RunningRclone::spawn(&mut passthrough_command(
		&binary,
		args,
		Vec::new(),
		own_process_group(),
	))
	.await
}

impl RunningRclone {
	async fn spawn(command: &mut Command) -> Result<Self, Error> {
		let child = spawn_rclone(command)
			.await
			.context("failed to start rclone")?;
		let identity = match child.id() {
			Some(pid) => identify(pid).await?,
			None => None,
		};
		Ok(Self { child, identity })
	}

	/// Waits for rclone to exit. Cancel safe.
	pub async fn wait(&mut self) -> Result<ExitStatus, Error> {
		self.child.wait().await.context("failed to wait for rclone")
	}

	/// Stops rclone: sends SIGTERM unless `cause` says it was signalled already (never on
	/// Windows, whose console delivers Ctrl-C, Ctrl-Break and close to rclone itself), and kills it
	/// if it has not exited within `grace`. Returns its exit status. Dropping the future leaves
	/// rclone running: [`RunningRclone::kill`] it then.
	pub async fn stop(&mut self, cause: StopCause, grace: Duration) -> Result<ExitStatus, Error> {
		let in_foreground = self
			.identity
			.as_ref()
			.is_some_and(|identity| in_terminal_foreground(identity.pid));
		self.stop_with(sends_sigterm(cause, in_foreground), grace)
			.await
	}

	async fn stop_with(
		&mut self,
		sends_sigterm: bool,
		grace: Duration,
	) -> Result<ExitStatus, Error> {
		if sends_sigterm && let Some(identity) = &self.identity {
			terminate(identity).await?;
		}
		// The child is not reaped until it is waited on, so its pid is not reused while it is
		// signalled; `terminate` checks its start time all the same.
		match tokio::time::timeout(grace, self.child.wait()).await {
			Ok(status) => status.context("failed to wait for rclone"),
			Err(_) => {
				warn!("rclone did not exit within {grace:?}, killing it");
				self.kill()?;
				self.wait().await
			}
		}
	}

	/// Kills rclone at once; [`RunningRclone::wait`] collects its status.
	pub fn kill(&mut self) -> Result<(), Error> {
		self.child.start_kill().context("failed to kill rclone")
	}
}

/// Whether a stop sends rclone SIGTERM: not when Ctrl-C reached rclone itself, as it does in the
/// terminal's foreground process group, and never on Windows.
fn sends_sigterm(cause: StopCause, rclone_in_terminal_foreground: bool) -> bool {
	cfg!(unix)
		&& match cause {
			StopCause::Terminate => true,
			StopCause::Interrupt => !rclone_in_terminal_foreground,
		}
}

/// Whether rclone gets a process group of its own. It shares this process's only while that is
/// the terminal's foreground group, so it keeps the terminal to read from. Otherwise a signal to
/// the whole group (`timeout`, `kill %job` for a job started in the background) must not reach it
/// as well as the stop that this process passes on. The daemon runs in a group of its own for the
/// same reason. Decided once, when rclone starts: a job started in the background and brought to
/// the foreground leaves rclone in the background, where reading the terminal stops it (which
/// [`terminate`]'s SIGCONT undoes), and one started in the foreground and sent to the background
/// keeps rclone in its group, which `kill %job` then signals along with the stop.
#[cfg(unix)]
fn own_process_group() -> bool {
	terminal_foreground().is_none_or(|foreground| foreground != nix::unistd::getpgrp())
}

#[cfg(not(unix))]
fn own_process_group() -> bool {
	false
}

/// Whether the process `pid` is in its controlling terminal's foreground process group, where the
/// terminal's Ctrl-C reaches it.
#[cfg(unix)]
fn in_terminal_foreground(pid: u32) -> bool {
	let Ok(pid) = i32::try_from(pid) else {
		return false;
	};
	match (
		terminal_foreground(),
		nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid))),
	) {
		(Some(foreground), Ok(group)) => foreground == group,
		_ => false,
	}
}

#[cfg(not(unix))]
fn in_terminal_foreground(_pid: u32) -> bool {
	false
}

/// The foreground process group of this process's controlling terminal, or `None` without one.
#[cfg(unix)]
fn terminal_foreground() -> Option<nix::unistd::Pid> {
	// Opening it fails when there is no controlling terminal.
	let tty = std::fs::File::open("/dev/tty").ok()?;
	nix::unistd::tcgetpgrp(&tty).ok()
}

/// The command running `rclone <args>` with the remote's variables `remote`, in a process group of
/// its own if `own_group` (Unix; on Windows a console's Ctrl-C reaches every process attached to
/// it whatever its group).
fn passthrough_command(
	binary: &Path,
	args: &[String],
	remote: Vec<(String, String)>,
	own_group: bool,
) -> Command {
	let mut command = Command::new(binary);
	command
		.args(["--config", ""])
		.args(args)
		.envs(remote)
		// The backstop for a panic or a dropped future; a stop gives it a grace period first.
		.kill_on_drop(true);
	#[cfg(unix)]
	if own_group {
		command.process_group(0);
	}
	#[cfg(not(unix))]
	let _ = own_group;
	command
}

/// The `RCLONE_CONFIG_FILEN_*` variables of the account `config` describes, its password options
/// obscured by `binary`.
async fn remote_env(
	binary: &Path,
	config: &FilenSDKConfig,
) -> Result<Vec<(String, String)>, Error> {
	// The backend logs in with the keys, never with a password, but requires the option to be set.
	// This is the value it documents for that.
	// https://github.com/FilenCloudDienste/filen-rclone/blob/784979078ecd573af0d9809d2c5a07bafaad2c41/backend/filen/filen.go#L136
	let password = obscure(binary, "INTERNAL").await?;
	let api_key = obscure(binary, &config.api_key).await?;
	// Owned copies: the variables are built once per command.
	Ok([
		("TYPE", "filen".to_owned()),
		("EMAIL", config.email.clone()),
		("PASSWORD", password),
		("API_KEY", api_key),
		("MASTER_KEYS", config.master_keys.join("|")),
		("PUBLIC_KEY", config.public_key.clone()),
		("PRIVATE_KEY", config.private_key.clone()),
		("AUTH_VERSION", (config.auth_version as u8).to_string()),
		("BASE_FOLDER_UUID", config.base_folder_uuid.clone()),
	]
	.into_iter()
	.map(|(option, value)| (format!("RCLONE_CONFIG_{REMOTE}_{option}"), value))
	.collect())
}

/// `secret` obscured as rclone's config holds a password option, by `rclone obscure`, which reads
/// it from stdin: its argv is readable by every user.
async fn obscure(binary: &Path, secret: &str) -> Result<String, Error> {
	let mut child = spawn_rclone(
		Command::new(binary)
			.args(["obscure", "-"])
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::null())
			.kill_on_drop(true),
	)
	.await
	.context("failed to start rclone obscure")?;
	let mut stdin = child.stdin.take().ok_or_else(|| {
		Error::custom(ErrorKind::Internal, "rclone obscure's stdin was not piped")
	})?;
	stdin
		.write_all(secret.as_bytes())
		.await
		.context("failed to write to rclone obscure")?;
	drop(stdin);
	let output = child
		.wait_with_output()
		.await
		.context("failed to wait for rclone obscure")?;
	if !output.status.success() {
		return Err(Error::custom(
			ErrorKind::IO,
			format!("rclone obscure exited with {}", output.status),
		));
	}
	let obscured = String::from_utf8(output.stdout).map_err(|e| {
		Error::custom_with_source(
			ErrorKind::Conversion,
			e,
			Some("rclone obscure's output is not UTF-8"),
		)
	})?;
	Ok(obscured.trim().to_owned())
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;
	#[cfg(unix)]
	use std::{
		os::unix::{fs::PermissionsExt as _, process::ExitStatusExt as _},
		time::Instant,
	};

	#[cfg(unix)]
	use tokio::io::{AsyncBufReadExt as _, BufReader};

	use super::*;

	/// SIGKILL's number, which a killed child's status carries.
	#[cfg(unix)]
	const SIGKILL: i32 = 9;

	#[test]
	fn rclone_passthrough_writes_no_config() {
		let remote = vec![("RCLONE_CONFIG_FILEN_TYPE".to_owned(), "filen".to_owned())];
		let args = ["lsf".to_owned(), "filen:".to_owned()];

		let command = passthrough_command(Path::new("/opt/rclone"), &args, remote, false);

		let std = command.as_std();
		assert_eq!(
			std.get_args().collect::<Vec<_>>(),
			["--config", "", "lsf", "filen:"].map(OsStr::new)
		);
		assert_eq!(
			std.get_envs().collect::<Vec<_>>(),
			[(
				OsStr::new("RCLONE_CONFIG_FILEN_TYPE"),
				Some(OsStr::new("filen"))
			)]
		);
	}

	/// A child running the shell `script`, once it printed that it is ready: a signal sent before
	/// its traps are set would end it by the signal's default action instead.
	#[cfg(unix)]
	async fn ready_shell(script: &str) -> RunningRclone {
		let mut command = Command::new("/bin/sh");
		command
			.args(["-c", script])
			.stdout(Stdio::piped())
			.kill_on_drop(true);
		let mut rclone = RunningRclone::spawn(&mut command).await.unwrap();
		let stdout = rclone.child.stdout.take().unwrap();
		let mut line = String::new();
		BufReader::new(stdout).read_line(&mut line).await.unwrap();
		assert_eq!(line, "ready\n");
		rclone
	}

	/// Exits with 7 on SIGTERM.
	#[cfg(unix)]
	const EXITS_ON_SIGTERM: &str = r#"trap "exit 7" TERM; echo ready; while :; do sleep 0.1; done"#;

	#[cfg(unix)]
	#[tokio::test]
	async fn a_terminate_stop_sends_sigterm_and_returns_the_exit_status() {
		let mut rclone = ready_shell(EXITS_ON_SIGTERM).await;

		let status = rclone
			.stop(StopCause::Terminate, Duration::from_secs(60))
			.await
			.unwrap();

		assert_eq!(status.code(), Some(7));
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn a_stopped_child_acts_on_the_sigterm_within_the_grace() {
		let mut rclone = ready_shell(EXITS_ON_SIGTERM).await;
		let pid = rclone.identity.as_ref().unwrap().pid.to_string();
		// As job control stops a background process that reads its terminal.
		let stopped = std::process::Command::new("kill")
			.args(["-STOP", &pid])
			.status()
			.unwrap();
		assert!(stopped.success());

		let status = rclone
			.stop(StopCause::Terminate, Duration::from_secs(10))
			.await
			.unwrap();

		assert_eq!(status.code(), Some(7));
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn a_child_ignoring_sigterm_is_killed_after_the_grace() {
		let grace = Duration::from_millis(500);
		let mut rclone =
			ready_shell(r#"trap "" TERM; echo ready; while :; do sleep 0.1; done"#).await;
		let started = Instant::now();

		let status = rclone.stop(StopCause::Terminate, grace).await.unwrap();

		assert_eq!(status.signal(), Some(SIGKILL));
		assert!(started.elapsed() >= grace, "{:?}", started.elapsed());
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn an_interrupt_from_the_terminal_sends_nothing() {
		let mut rclone = ready_shell(EXITS_ON_SIGTERM).await;

		// Long enough for the shell to act on a SIGTERM, had one been sent.
		let status = rclone
			.stop_with(false, Duration::from_secs(1))
			.await
			.unwrap();

		assert_eq!(status.signal(), Some(SIGKILL));
	}

	#[test]
	fn sigterm_is_sent_unless_the_terminal_signalled_rclone() {
		let cases = [
			(StopCause::Terminate, false, true),
			(StopCause::Terminate, true, true),
			(StopCause::Interrupt, false, true),
			(StopCause::Interrupt, true, false),
		];
		for (cause, in_foreground, on_unix) in cases {
			assert_eq!(
				sends_sigterm(cause, in_foreground),
				cfg!(unix) && on_unix,
				"{cause:?}, in the terminal's foreground: {in_foreground}"
			);
		}
	}

	/// An `rclone` in `dir` that takes the arguments rclone would, ignores them and sleeps.
	#[cfg(unix)]
	fn sleeping_rclone(dir: &Path) -> std::path::PathBuf {
		let binary = dir.join("rclone");
		std::fs::write(&binary, "#!/bin/sh\nexec sleep 60\n").unwrap();
		std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
		binary
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn a_child_out_of_the_terminal_gets_a_process_group_of_its_own() {
		let dir = tempfile::tempdir().unwrap();
		let binary = sleeping_rclone(dir.path());

		let mut rclone =
			RunningRclone::spawn(&mut passthrough_command(&binary, &[], Vec::new(), true))
				.await
				.unwrap();

		let pid = nix::unistd::Pid::from_raw(i32::try_from(rclone.child.id().unwrap()).unwrap());
		assert_eq!(nix::unistd::getpgid(Some(pid)), Ok(pid));
		rclone.kill().unwrap();
		rclone.wait().await.unwrap();
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn kill_ends_rclone_at_once() {
		let mut rclone = ready_shell(EXITS_ON_SIGTERM).await;
		tokio::select! {
			_ = rclone.stop_with(false, Duration::from_secs(60)) => panic!("rclone stopped by itself"),
			() = tokio::time::sleep(Duration::from_millis(100)) => {}
		}

		rclone.kill().unwrap();

		assert_eq!(rclone.wait().await.unwrap().signal(), Some(SIGKILL));
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn dropping_the_handle_kills_rclone() {
		let dir = tempfile::tempdir().unwrap();
		let binary = sleeping_rclone(dir.path());
		let rclone =
			RunningRclone::spawn(&mut passthrough_command(&binary, &[], Vec::new(), false))
				.await
				.unwrap();
		let pid = rclone.identity.as_ref().unwrap().pid;

		drop(rclone);

		let deadline = Instant::now() + Duration::from_secs(10);
		while identify(pid).await.unwrap().is_some() {
			assert!(Instant::now() < deadline, "rclone {pid} still runs");
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}
}
