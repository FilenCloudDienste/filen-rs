//! The commands about the CLI and the local side of the REPL. These need no account: none of
//! them logs in.

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

#[test]
fn lcd_outside_the_repl_is_a_usage_error() {
	let working_dir = TempDir::new().unwrap();

	let output = run_in(&working_dir, &["lcd", "/"], "");

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
	let stderr = String::from_utf8(output.stderr).unwrap();
	assert!(stderr.contains("`lcd` only works in the REPL"), "{stderr}");
}

#[test]
fn piped_commands_list_and_follow_the_local_working_directory() {
	let working_dir = TempDir::new().unwrap();
	std::fs::create_dir(working_dir.path().join("sub")).unwrap();
	std::fs::write(working_dir.path().join("sub/file.txt"), "x").unwrap();
	let sub = working_dir.path().join("sub").canonicalize().unwrap();

	let output = run_in(&working_dir, &["--json"], "lls\nlcd sub\nlpwd\nlls\n");

	let stderr = String::from_utf8(output.stderr).unwrap();
	assert_eq!(output.status.code(), Some(0), "{stderr}");
	let stdout = String::from_utf8(output.stdout).unwrap();
	let printed = serde_json::Deserializer::from_str(&stdout)
		.into_iter::<serde_json::Value>()
		.collect::<Result<Vec<_>, _>>()
		.unwrap();
	assert_eq!(
		printed,
		[
			serde_json::json!({ "directories": ["sub"], "files": [] }),
			serde_json::json!({ "path": sub.to_string_lossy() }),
			serde_json::json!({ "directories": [], "files": ["file.txt"] }),
		]
	);
}

#[test]
fn a_relative_config_dir_stays_put_after_lcd() {
	let working_dir = TempDir::new().unwrap();
	std::fs::create_dir(working_dir.path().join("sub")).unwrap();
	let config_dir = working_dir.path().canonicalize().unwrap().join("cfg");

	let mut child = Command::new(assert_cmd::cargo::cargo_bin!())
		.current_dir(working_dir.path())
		.args(["--config-dir", "cfg", "--skip-update", "--json"])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	child
		.stdin
		.take()
		.unwrap()
		.write_all(b"lcd sub\nversion\n")
		.unwrap();
	let output = child.wait_with_output().unwrap();

	let stderr = String::from_utf8(output.stderr).unwrap();
	assert_eq!(output.status.code(), Some(0), "{stderr}");
	let version: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
	assert_eq!(version["configDir"], config_dir.to_string_lossy().as_ref());
	assert!(!working_dir.path().join("sub/cfg").exists());
}

#[test]
fn a_failure_with_json_is_one_error_line_on_stderr() {
	let working_dir = TempDir::new().unwrap();

	let output = run_in(&working_dir, &["--json", "lcd", "/"], "");

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
	let stderr = String::from_utf8(output.stderr).unwrap();
	let failure: serde_json::Value = serde_json::from_str(&stderr).unwrap();
	assert_eq!(failure["error"]["kind"], "usage", "{stderr}");
	assert_eq!(failure["error"]["exitCode"], 2, "{stderr}");
}
