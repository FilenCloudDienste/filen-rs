//! The commands about the CLI. These need no account: none of them logs in.

use std::{
	io::Write as _,
	process::{Command, Output, Stdio},
};

use assert_fs::TempDir;

/// Runs the CLI on a fresh config directory in `working_dir` with `args`, skipping the update
/// check, with `stdin` piped into it.
fn run_in(working_dir: &TempDir, args: &[&str], stdin: &str) -> Output {
	let config_dir = TempDir::new().unwrap();
	let mut child = Command::new(assert_cmd::cargo::cargo_bin!())
		.current_dir(working_dir.path())
		.arg("--config-dir")
		.arg(config_dir.path())
		.arg("--skip-update")
		.args(args)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	child
		.stdin
		.take()
		.unwrap()
		.write_all(stdin.as_bytes())
		.unwrap();
	child.wait_with_output().unwrap()
}

#[test]
fn completions_print_a_script_without_logging_in() {
	let working_dir = TempDir::new().unwrap();

	let output = run_in(&working_dir, &["completions", "bash"], "");

	let stderr = String::from_utf8(output.stderr).unwrap();
	assert_eq!(output.status.code(), Some(0), "{stderr}");
	let stdout = String::from_utf8(output.stdout).unwrap();
	assert!(stdout.contains("complete -F _filen"), "{stdout}");
}
