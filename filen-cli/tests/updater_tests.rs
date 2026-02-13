#[test]
// do not run in aarch64-apple-darwin, because there is no release asset for that
#[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
fn test_updater() {
	use regex::Regex;
	use std::fs;

	let binary = assert_cmd::cargo::cargo_bin!();
	let dir = tempfile::tempdir().unwrap();
	fs::copy(binary, dir.path().join(binary.file_name().unwrap())).unwrap();

	// run once, should update
	unsafe {
		std::env::set_var("FILEN_CLI_TESTING_MOCK_VERSION", "0.0.0-test");
	}
	let assert = assert_cmd::Command::new(dir.path().join(binary.file_name().unwrap()))
		.args([
			"-vv",
			"--config-dir",
			dir.path().to_str().unwrap(),
			"--always-update",
			"exit",
		])
		.assert()
		.success()
		.stdout(predicates::str::contains("Update installed successfully"));
	let output = assert.get_output();
	let new_version = Regex::new(r#"Automatically updating from v.* to v(.*)\.\.\."#)
		.unwrap()
		.captures(std::str::from_utf8(&output.stdout).unwrap())
		.unwrap()
		.get(1)
		.unwrap()
		.as_str();
	println!("New version installed: {}", new_version);
	unsafe {
		std::env::set_var("FILEN_CLI_TESTING_MOCK_VERSION", "off");
	}

	// run again, so the last checked time is updated
	assert_cmd::Command::new(dir.path().join(binary.file_name().unwrap()))
		.args(["-vv", "--config-dir", dir.path().to_str().unwrap(), "exit"])
		.assert()
		.success()
		.stdout(predicates::str::contains(
			"Wrote last update check timestamp",
		));

	// run again, should be up to date and not check again
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
