//! What the CLI does when its stdout is closed before it finished, as `head` closes it once it has
//! read its lines. These need no account.
//!
//! Each test holds [`SPAWNING`] while it runs: on macOS a pipe is marked close-on-exec only after
//! it is created, so a child another test spawned in between would inherit the pipe's read end and
//! keep the stdout of this test's CLI open.

use std::{
	io::{self, Write as _},
	process::{Command, Output, Stdio},
	sync::{Mutex, PoisonError},
};

use assert_fs::TempDir;

static SPAWNING: Mutex<()> = Mutex::new(());

/// Runs the CLI with `args` on a fresh config directory, with `script` piped into it and a stdout
/// whose reader is gone.
fn run_with_closed_stdout(args: &[&str], script: &str) -> Output {
	let _spawning = SPAWNING.lock().unwrap_or_else(PoisonError::into_inner);
	let config_dir = TempDir::new().unwrap();
	// Closing the read end first makes the first write fail, as it does once `head` has its lines.
	let (reader, writer) = io::pipe().unwrap();
	drop(reader);
	let mut cli = Command::new(assert_cmd::cargo::cargo_bin!())
		.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
		.env("FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG", "1")
		.arg("--config-dir")
		.arg(config_dir.path())
		.arg("--skip-update")
		.args(args)
		.stdin(Stdio::piped())
		.stdout(writer)
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	cli.stdin
		.take()
		.unwrap()
		.write_all(script.as_bytes())
		.unwrap();
	cli.wait_with_output().unwrap()
}

#[test]
fn a_closed_stdout_ends_the_cli_successfully() {
	let output = run_with_closed_stdout(&["--help"], "");

	let stderr = String::from_utf8(output.stderr).unwrap();
	assert_eq!(output.status.code(), Some(0), "{stderr}");
	assert_eq!(stderr, "");
}

#[test]
fn a_command_whose_stdout_closes_exits_successfully() {
	let output = run_with_closed_stdout(&["help"], "");

	let stderr = String::from_utf8(output.stderr).unwrap();
	assert_eq!(output.status.code(), Some(0), "{stderr}");
	assert_eq!(stderr, "");
}

#[test]
fn a_closed_stdout_stops_the_commands_piped_in() {
	// the unknown command would fail with 2 if the commands went on after the first
	let output = run_with_closed_stdout(&[], "help\nno-such-command\n");

	let stderr = String::from_utf8(output.stderr).unwrap();
	assert_eq!(output.status.code(), Some(0), "{stderr}");
	assert_eq!(stderr, "");
}
