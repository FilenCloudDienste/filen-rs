use assert_cmd::cargo::cargo_bin_cmd;
use filen_macros::shared_test_runtime;
use test_utils::authenticated_cli_with_args;

fn get_testing_credentials() -> (String, String) {
	dotenv::dotenv().ok();
	let email = std::env::var("TEST_EMAIL").expect("TEST_EMAIL not set in .env file");
	let password = std::env::var("TEST_PASSWORD").expect("TEST_PASSWORD not set in .env file");
	(email, password)
}

#[test]
fn authenticate_from_cli_args() {
	let (email, password) = get_testing_credentials();
	cargo_bin_cmd!()
		.args([
			"--email",
			&email,
			"--password",
			&password,
			"-v",
			"stat",
			"/",
		])
		// (use stat command to verify successful authentication)
		.assert()
		.success()
		.stdout(predicates::str::contains(
			"Authenticated from CLI arguments",
		));
}

#[shared_test_runtime]
async fn export_and_authenticate_from_auth_config() {
	let workdir = assert_fs::TempDir::new().unwrap();
	authenticated_cli_with_args!("export-auth-config")
		.success()
		.stdout(predicates::str::contains("Exported auth config"));
	tokio::fs::copy(
		"filen-cli-auth-config.txt",
		workdir.path().join("filen-cli-auth-config.txt"),
	)
	.await
	.unwrap();
	tokio::fs::remove_file("filen-cli-auth-config.txt")
		.await
		.unwrap();
	cargo_bin_cmd!()
		.args([
			"--auth-config-path",
			workdir
				.path()
				.join("filen-cli-auth-config.txt")
				.to_str()
				.unwrap(),
			"-v",
			"stat",
			"/",
		])
		.assert()
		.success()
		.stdout(predicates::str::contains(
			"Authenticated from auth config file",
		));
}

#[test]
fn authenticate_from_env_vars() {
	let config_dir = assert_fs::TempDir::new().unwrap();
	let (email, password) = get_testing_credentials();
	cargo_bin_cmd!()
		.env("FILEN_CLI_EMAIL", &email)
		.env("FILEN_CLI_PASSWORD", &password)
		.args([
			"--config-dir",
			config_dir.path().to_str().unwrap(),
			"-v",
			"stat",
			"/",
		])
		.assert()
		.success()
		.stdout(predicates::str::contains(
			"Authenticated from environment variables",
		));
}

// notes:
// keyring authentication is hard to test here, so ignore it for now

// todo: test prompting for 2fa code (need another test account with 2fa enabled for that?)
// -> I think there isn't currently one in CI, so do that later
