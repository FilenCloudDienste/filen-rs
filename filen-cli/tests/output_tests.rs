//! Which of stdout and stderr the CLI writes to, and the exit codes it exits with. These need no
//! account: each process stops before it would log in.

use std::process::{Command, Output};

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
