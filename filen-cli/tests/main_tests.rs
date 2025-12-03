use assert_cmd::cargo::cargo_bin_cmd;
use assert_fs::TempDir;
use predicates::prelude::PredicateBooleanExt;

#[test]
fn print_help_text() {
	cargo_bin_cmd!().arg("--help").assert().success().stdout(
		predicates::str::contains("filen-cli")
			.and(predicates::str::contains("[OPTIONS] [COMMAND]")),
	);
	cargo_bin_cmd!().arg("help").assert().success().stdout(
		predicates::str::contains("filen-cli")
			.and(predicates::str::contains("[OPTIONS] [COMMAND]")),
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

// todo: testing interactive clis is hard
// so we want to use [rexpect](https://docs.rs/rexpect/latest/rexpect/)
// since it does not work on windows, I'll write this test later
// we should test not only basic interactiev input prompts, but also y/n prompts etc.
// maybe a solution would be to mock input? but that seems really complicated and not worth it

// todo: add more tests:

// main.rs:
// updater (?)
// print user-facing failure
// invalid quotes

// ui.rs:
// snapshot-test something with a lot of formatted stuff (maybe actually unit-test)

// updater.rs:
// what *can* we even test here? do we need to mock something?
