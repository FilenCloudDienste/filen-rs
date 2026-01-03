use std::fs;

use regex::Regex;

#[test]
fn test_updater() {
	let binary = assert_cmd::cargo::cargo_bin!();
	let dir = tempfile::tempdir().unwrap();
	fs::copy(binary, dir.path().join(binary.file_name().unwrap())).unwrap();

	// run once, should update
	unsafe {
		std::env::set_var("FILEN_CLI_TESTING_MOCK_VERSION", "0.0.0-test");
	}
	let assert = assert_cmd::Command::new(dir.path().join(binary.file_name().unwrap()))
		.args(["-vv", "--config-dir", dir.path().to_str().unwrap(), "exit"])
		.assert()
		.success()
		.stdout(predicates::str::contains("Update installed successfully"));
	let output = assert.get_output();
	let new_version = Regex::new(r#"Updating from v.* to v(.*)\.\.\."#)
		.unwrap()
		.captures(std::str::from_utf8(&output.stdout).unwrap())
		.unwrap()
		.get(1)
		.unwrap()
		.as_str();
	println!("New version installed: {}", new_version);

	// run again, should be up to date and not check again
	unsafe {
		std::env::set_var("FILEN_CLI_TESTING_MOCK_VERSION", "off");
	}
	assert_cmd::Command::new(dir.path().join(binary.file_name().unwrap()))
		.args(["-vv", "--config-dir", dir.path().to_str().unwrap(), "exit"])
		.assert()
		.success()
		.stdout(predicates::str::contains(
			"Skipping update check; last check",
		));

	// should be new version
	assert_cmd::Command::new(dir.path().join(binary.file_name().unwrap()))
		.args(["--config-dir", dir.path().to_str().unwrap(), "--version"])
		.assert()
		.success()
		.stdout(predicates::str::contains(new_version));
}
