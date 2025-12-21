use assert_cmd::cargo::cargo_bin_cmd;
use assert_fs::TempDir;
use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::HasName;
use predicates::prelude::PredicateBooleanExt;

#[test]
fn print_help_text() {
	cargo_bin_cmd!().arg("--help").assert().success().stdout(
		predicates::str::contains("filen-cli").and(predicates::str::contains(
			"Invoke the Filen CLI with no command specified to enter interactive mode",
		)),
	);
	cargo_bin_cmd!().arg("help").assert().success().stdout(
		predicates::str::contains("filen-cli").and(predicates::str::contains(
			"Invoke the Filen CLI with no command specified to enter interactive mode",
		)),
	);
}

#[test]
fn print_subcommand_help_text() {
	cargo_bin_cmd!()
		.args(["help", "cd"])
		.assert()
		.success()
		.stdout(predicates::str::contains("Change the working"));
}

#[test]
fn print_version() {
	cargo_bin_cmd!()
		.arg("--version")
		.assert()
		.success()
		.stdout(predicates::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn config_dir_flag() {
	let config_dir = TempDir::new().unwrap();
	cargo_bin_cmd!()
		.args([
			"--config-dir",
			config_dir.path().to_str().unwrap(),
			"-v",
			"exit",
		])
		.assert()
		.success()
		.stdout(predicates::str::contains(format!(
			"Full log file: {}",
			config_dir.path().join("logs").join("latest.log").display()
		)));
}

#[test]
fn verbose_flag() {
	// -q flag currently, doesn't do much, so just ignore it
	cargo_bin_cmd!()
		.args(["-v", "exit"])
		.assert()
		.success()
		.stdout(predicates::str::contains("Logging level: INFO"));
	cargo_bin_cmd!()
		.args(["-vv", "exit"])
		.assert()
		.success()
		.stdout(predicates::str::contains("Logging level: DEBUG"));
}

#[test]
fn logs_file() {
	let config_dir = TempDir::new().unwrap();
	let log_file = config_dir.path().join("logs").join("latest.log");
	cargo_bin_cmd!()
		.args(["--config-dir", config_dir.path().to_str().unwrap(), "exit"])
		.assert()
		.success();
	assert!(log_file.exists());
	assert!(log_file.is_file());
	let log_content = std::fs::read_to_string(log_file).unwrap();
	assert!(log_content.contains("Logging level: OFF"));
}

#[test]
fn unknown_command() {
	cargo_bin_cmd!()
		.arg("unknowncommand")
		.assert()
		.failure()
		.stderr(predicates::str::contains(
			"error: unrecognized subcommand 'unknowncommand'",
		));
}

#[cfg(target_os = "linux")] // rexpect only works on linux
#[shared_test_runtime]
async fn interactive_repl() {
	let test_resources = test_utils::RESOURCES.get_resources().await;
	let (email, password, _) = test_utils::RESOURCES.get_credentials();
	let mut p = rexpect::spawn(env!("CARGO_BIN_EXE_filen-cli"), Some(5000)).unwrap();
	p.exp_string("Email").unwrap();
	p.send_line(&email).unwrap();
	p.exp_string("Password").unwrap();
	p.send_line(&password).unwrap();
	p.exp_string("Keep me logged in?").unwrap();
	p.send_line("n").unwrap();
	p.exp_string(&format!("({})", email)).unwrap();
	p.send_line("ls").unwrap();
	p.exp_string(test_resources.dir.name().unwrap()).unwrap();
	p.send_line("exit").unwrap();
	p.exp_eof().unwrap();
}

// todo: add more tests:

// main.rs:
// updater (?)
// print user-facing failure
// invalid quotes

// ui.rs:
// snapshot-test something with a lot of formatted stuff (maybe actually unit-test)

// updater.rs:
// what *can* we even test here? do we need to mock something?
