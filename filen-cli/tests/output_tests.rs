//! Which of stdout and stderr the CLI writes to, and the exit codes it exits with. These need no
//! account: each process stops before it would log in.

#[cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt as _};
use std::{
	io::Write as _,
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
fn a_config_dir_that_does_not_exist_is_created() {
	let parent = TempDir::new().unwrap();
	let config_dir = parent.path().join("missing/config");
	let output = Command::new(assert_cmd::cargo::cargo_bin!())
		.arg("--config-dir")
		.arg(&config_dir)
		.args(["--skip-update", "--help"])
		.output()
		.unwrap();

	let stderr = String::from_utf8(output.stderr).unwrap();
	assert_eq!(output.status.code(), Some(0), "{stderr}");
	assert!(config_dir.is_dir());
}

#[test]
fn a_config_dir_that_cannot_be_created_is_a_usage_error() {
	let parent = TempDir::new().unwrap();
	let file = parent.path().join("file");
	std::fs::write(&file, "").unwrap();
	let output = Command::new(assert_cmd::cargo::cargo_bin!())
		.arg("--config-dir")
		.arg(&file)
		.args(["--skip-update", "--help"])
		.output()
		.unwrap();

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
	let stderr = String::from_utf8(output.stderr).unwrap();
	assert!(
		stderr.contains("Failed to create the config directory"),
		"{stderr}"
	);
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

#[test]
fn cd_outside_the_repl_is_a_usage_error() {
	let output = run(&["cd", "/some/dir"]);

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
	let stderr = String::from_utf8(output.stderr).unwrap();
	assert!(stderr.contains("`cd` only works in the REPL"), "{stderr}");
	assert!(stderr.contains("--working-path"), "{stderr}");
}

#[test]
fn help_after_a_command_prints_that_commands_help() {
	for help_flag in ["--help", "-h"] {
		let output = run(&["cat", help_flag]);

		let stderr = String::from_utf8(output.stderr).unwrap();
		assert_eq!(output.status.code(), Some(0), "{stderr}");
		let stdout = String::from_utf8(output.stdout).unwrap();
		assert!(stdout.contains("Print the contents of files"), "{stdout}");
	}
}

/// Runs `logout` with `args` on a config directory holding an auth config, without a terminal
/// on stdin and with the keyring left alone. Returns its output and whether the auth config is
/// still there.
fn logout(args: &[&str]) -> (Output, bool) {
	let config_dir = TempDir::new().unwrap();
	let working_dir = TempDir::new().unwrap();
	let auth_config = config_dir.path().join("filen-cli-auth-config.txt");
	std::fs::write(&auth_config, "filen_cli_auth_config_1:").unwrap();
	let output = Command::new(assert_cmd::cargo::cargo_bin!())
		.current_dir(working_dir.path())
		.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
		.arg("--config-dir")
		.arg(config_dir.path())
		.args(["--skip-update", "logout"])
		.args(args)
		.stdin(Stdio::null())
		.output()
		.unwrap();
	(output, auth_config.exists())
}

#[test]
fn logout_without_a_terminal_asks_for_yes_and_keeps_the_credentials() {
	let (output, auth_config_exists) = logout(&[]);

	assert_eq!(output.status.code(), Some(2));
	let stderr = String::from_utf8(output.stderr).unwrap();
	assert!(stderr.contains("stdin is not a terminal"), "{stderr}");
	assert!(stderr.contains("Pass --yes to answer yes."), "{stderr}");
	assert!(auth_config_exists);
}

#[test]
fn logout_with_yes_deletes_the_credentials_without_a_terminal() {
	let (output, auth_config_exists) = logout(&["--yes"]);

	let stderr = String::from_utf8(output.stderr).unwrap();
	assert_eq!(output.status.code(), Some(0), "{stderr}");
	let stdout = String::from_utf8(output.stdout).unwrap();
	assert!(stdout.contains("Deleted auth config file at"), "{stdout}");
	assert!(!auth_config_exists);
}

#[test]
fn logging_in_with_no_input_fails_instead_of_prompting() {
	let config_dir = TempDir::new().unwrap();
	let output = Command::new(assert_cmd::cargo::cargo_bin!())
		.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
		.env("FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG", "1")
		.env_remove("FILEN_CLI_EMAIL")
		.env_remove("FILEN_CLI_PASSWORD")
		.env_remove("FILEN_CLI_2FA_CODE")
		.arg("--config-dir")
		.arg(config_dir.path())
		.args(["--skip-update", "--no-input", "ls"])
		.output()
		.unwrap();

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
	let stderr = String::from_utf8(output.stderr).unwrap();
	assert!(
		stderr.contains(
			"Not logged in: log in with --email and --password, or with --auth-config-path: \
			 Cannot ask \"Email:\": --no-input was given."
		),
		"{stderr}"
	);
}

/// Runs the CLI with `args` on a fresh config directory, logged out and without a terminal on
/// stdin, so that it fails before it would ask for an email.
fn run_logged_out(args: &[&str]) -> Output {
	let config_dir = TempDir::new().unwrap();
	Command::new(assert_cmd::cargo::cargo_bin!())
		.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
		.env("FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG", "1")
		.env_remove("FILEN_CLI_EMAIL")
		.env_remove("FILEN_CLI_PASSWORD")
		.env_remove("FILEN_CLI_2FA_CODE")
		.arg("--config-dir")
		.arg(config_dir.path())
		.arg("--skip-update")
		.args(args)
		.stdin(Stdio::null())
		.output()
		.unwrap()
}

#[test]
fn a_permanent_rm_without_yes_asks_for_it_before_logging_in() {
	let output = run_logged_out(&["rm", "-p", "a.txt"]);

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(
		console::strip_ansi_codes(&String::from_utf8(output.stderr).unwrap()),
		"✘ Cannot ask \"Permanently delete /a.txt?\": stdin is not a terminal. Pass --yes to \
		 answer yes.\n"
	);
}

#[test]
fn trash_delete_without_a_terminal_fails_before_logging_in() {
	let output = run_logged_out(&["trash-delete"]);

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(
		console::strip_ansi_codes(&String::from_utf8(output.stderr).unwrap()),
		"✘ Cannot ask \"Select an item to permanently delete\": stdin is not a terminal.\n"
	);
}

#[test]
fn no_input_without_a_command_runs_the_commands_piped_in() {
	let output = run_script(&["--no-input"], "--help cat\n");

	let stderr = String::from_utf8(output.stderr).unwrap();
	assert_eq!(output.status.code(), Some(0), "{stderr}");
	let stdout = String::from_utf8(output.stdout).unwrap();
	assert!(stdout.contains("Print the contents of files"), "{stdout}");
}

/// Runs the CLI with `args` and without a command on a fresh config directory, with `script`
/// piped into it.
fn run_script(args: &[&str], script: &str) -> Output {
	let config_dir = TempDir::new().unwrap();
	let mut cli = Command::new(assert_cmd::cargo::cargo_bin!())
		.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
		.env("FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG", "1")
		.env_remove("FILEN_CLI_EMAIL")
		.env_remove("FILEN_CLI_PASSWORD")
		.env_remove("FILEN_CLI_2FA_CODE")
		.arg("--config-dir")
		.arg(config_dir.path())
		.arg("--skip-update")
		.args(args)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
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
fn commands_piped_in_run_one_after_another() {
	let output = run_script(&[], "--help ls\n\n--help cat\n");

	let stderr = String::from_utf8(output.stderr).unwrap();
	assert_eq!(output.status.code(), Some(0), "{stderr}");
	let stdout = String::from_utf8(output.stdout).unwrap();
	let ls = stdout.find("List files in a directory").unwrap();
	let cat = stdout.find("Print the contents of files").unwrap();
	assert!(ls < cat, "{stdout}");
	// no banner and no login: the commands did not need one
	assert!(!stdout.contains("Filen CLI v"), "{stdout}");
}

#[test]
fn nothing_piped_in_runs_nothing() {
	let output = run_script(&[], "");

	assert_eq!(output.status.code(), Some(0));
	assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
}

#[test]
fn commands_piped_in_go_on_after_a_failure_and_exit_with_its_code() {
	let output = run_script(&[], "unknowncommand\n--help cat\n");

	assert_eq!(output.status.code(), Some(2));
	let stdout = String::from_utf8(output.stdout).unwrap();
	assert!(stdout.contains("Print the contents of files"), "{stdout}");
	let stderr = String::from_utf8(output.stderr).unwrap();
	assert!(stderr.contains("unknowncommand"), "{stderr}");
}

#[test]
fn exit_piped_in_stops_the_commands_after_it() {
	let output = run_script(&[], "exit\n--help cat\n");

	assert_eq!(output.status.code(), Some(0));
	assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
}

#[test]
fn a_command_piped_in_that_would_prompt_fails_instead() {
	let output = run_script(&[], "ls\n");

	assert_eq!(output.status.code(), Some(2));
	let stderr = String::from_utf8(output.stderr).unwrap();
	assert!(stderr.contains("stdin is not a terminal"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn a_config_dir_where_logs_cannot_be_written_only_warns() {
	let config_dir = TempDir::new().unwrap();
	fs::set_permissions(config_dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
	let output = Command::new(assert_cmd::cargo::cargo_bin!())
		.arg("--config-dir")
		.arg(config_dir.path())
		.args(["--skip-update", "--help"])
		.output()
		.unwrap();
	fs::set_permissions(config_dir.path(), fs::Permissions::from_mode(0o700)).unwrap();

	let stderr = String::from_utf8(output.stderr).unwrap();
	assert_eq!(output.status.code(), Some(0), "{stderr}");
	assert!(
		stderr.contains("Not writing log files: failed to create"),
		"{stderr}"
	);
	let stdout = String::from_utf8(output.stdout).unwrap();
	assert!(stdout.contains("Welcome to Filen CLI"), "{stdout}");
	assert!(!config_dir.path().join("logs").exists());
}
