//! Which of stdout and stderr the CLI writes to, and the exit codes it exits with. These need no
//! account: each process stops before it would log in.

use std::{
	io,
	process::{Command, Output, Stdio},
};

use assert_fs::TempDir;

/// Runs the CLI on a fresh config directory with `args`, skipping the update check.
fn run(args: &[&str]) -> Output {
	let config_dir = TempDir::new().unwrap();
	Command::new(assert_cmd::cargo::cargo_bin!())
		.arg("--config-dir")
		.arg(config_dir.path())
		.arg("--skip-update")
		.args(args)
		.output()
		.unwrap()
}

#[test]
fn log_lines_go_to_stderr_and_help_to_stdout() {
	let output = run(&["-v", "--help"]);

	assert!(output.status.success(), "{}", output.status);
	let stdout = String::from_utf8(output.stdout).unwrap();
	let stderr = String::from_utf8(output.stderr).unwrap();
	assert!(stdout.contains("Welcome to Filen CLI"), "{stdout}");
	assert!(!stdout.contains("Logging level"), "{stdout}");
	assert!(stderr.contains("Logging level: INFO"), "{stderr}");
}

#[test]
fn a_closed_stdout_ends_the_cli_cleanly() {
	let config_dir = TempDir::new().unwrap();
	// Closing the read end first makes the first write fail, as it does once `head` has its lines.
	let (reader, writer) = io::pipe().unwrap();
	drop(reader);
	let output = Command::new(assert_cmd::cargo::cargo_bin!())
		.arg("--config-dir")
		.arg(config_dir.path())
		.args(["--skip-update", "--help"])
		.stdout(writer)
		.stderr(Stdio::piped())
		.output()
		.unwrap();

	let stderr = String::from_utf8(output.stderr).unwrap();
	assert_eq!(output.status.code(), Some(0), "{stderr}");
	assert_eq!(stderr, "");
}

#[test]
fn help_for_an_unknown_topic_fails() {
	let output = run(&["--help", "nonexistent"]);

	assert_eq!(output.status.code(), Some(1));
	assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
	let stderr = String::from_utf8(output.stderr).unwrap();
	assert!(
		stderr.contains("No help found for 'nonexistent'"),
		"{stderr}"
	);
}

#[test]
fn a_config_dir_that_does_not_exist_is_a_usage_error() {
	let parent = TempDir::new().unwrap();
	let output = Command::new(assert_cmd::cargo::cargo_bin!())
		.arg("--config-dir")
		.arg(parent.path().join("missing"))
		.args(["--skip-update", "--help"])
		.output()
		.unwrap();

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
	let stderr = String::from_utf8(output.stderr).unwrap();
	assert!(stderr.contains("Config dir does not exist"), "{stderr}");
}

#[test]
fn an_unknown_command_is_a_usage_error() {
	let output = run(&["unknowncommand"]);

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
}

#[test]
fn exporting_the_markdown_docs_exits_after_writing_them() {
	let config_dir = TempDir::new().unwrap();
	let working_dir = TempDir::new().unwrap();
	let output = Command::new(assert_cmd::cargo::cargo_bin!())
		.current_dir(working_dir.path())
		.arg("--config-dir")
		.arg(config_dir.path())
		.args(["--skip-update", "--export-markdown-docs"])
		.stdin(Stdio::null())
		.output()
		.unwrap();

	let stderr = String::from_utf8(output.stderr).unwrap();
	assert_eq!(output.status.code(), Some(0), "{stderr}");
	// no banner and no login prompt: the REPL did not start
	assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
	assert!(
		working_dir
			.path()
			.join("filen-cli-markdown-docs/01-usage.md")
			.is_file()
	);
}
