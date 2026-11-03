#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use assert_cmd::cargo::cargo_bin_cmd;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use filen_macros::shared_test_runtime;
use filen_sdk_rs::auth::Client;
use predicates::prelude::PredicateBooleanExt as _;
use test_utils::authenticated_cli_with_args;

fn get_testing_credentials() -> (String, String) {
	dotenv::dotenv().ok();
	let email = std::env::var("TEST_EMAIL").expect("TEST_EMAIL not set in .env file");
	let password = std::env::var("TEST_PASSWORD").expect("TEST_PASSWORD not set in .env file");
	(email, password)
}

#[shared_test_runtime]
async fn test_serialize_deserialize_auth_config() {
	let original_client = test_utils::RESOURCES.client().await;
	let serialized = filen_cli::serialize_auth_config(&original_client).unwrap();
	let deserialized_client =
		filen_cli::deserialize_auth_config(&serialized, &filen_cli::ClientConfigArgs::default())
			.unwrap();
	assert_eq!(original_client.email(), deserialized_client.email());
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
		.stderr(predicates::str::contains(
			"Authenticated from CLI arguments",
		));
}

#[test]
fn authenticate_from_the_variables_of_the_old_cli() {
	let (email, password) = get_testing_credentials();
	let config_dir = assert_fs::TempDir::new().unwrap();
	cargo_bin_cmd!()
		.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
		.env("FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG", "1")
		.env_remove("FILEN_CLI_EMAIL")
		.env_remove("FILEN_CLI_PASSWORD")
		.env_remove("FILEN_CLI_2FA_CODE")
		.env("FILEN_EMAIL", &email)
		.env("FILEN_PASSWORD", &password)
		.env_remove("FILEN_2FA_CODE")
		.args([
			"--skip-update",
			"--config-dir",
			config_dir.path().to_str().unwrap(),
			"-v",
			"stat",
			"/",
		])
		.assert()
		.success()
		.stderr(predicates::str::contains(
			"Using the credentials from FILEN_EMAIL and FILEN_PASSWORD",
		))
		.stderr(predicates::str::contains(
			"Authenticated from CLI arguments",
		));
}

#[shared_test_runtime]
async fn export_and_authenticate_from_auth_config() {
	let workdir = assert_fs::TempDir::new().unwrap();
	let path = workdir.path().join("exported-auth-config.txt");
	authenticated_cli_with_args!("export-auth-config", path.to_str().unwrap(), "--yes")
		.success()
		.stdout(predicates::str::contains(format!(
			"Exported auth config to {}",
			path.display()
		)));
	cargo_bin_cmd!()
		.args([
			"--auth-config-path",
			path.to_str().unwrap(),
			"-v",
			"stat",
			"/",
		])
		.assert()
		.success()
		.stderr(predicates::str::contains(
			"Authenticated from auth config file",
		));
}

#[test]
fn exporting_an_auth_config_without_yes_fails_before_logging_in() {
	let config_dir = assert_fs::TempDir::new().unwrap();
	let path = config_dir.path().join("exported-auth-config.txt");
	cargo_bin_cmd!()
		.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
		.env("FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG", "1")
		.env_remove("FILEN_CLI_EMAIL")
		.env_remove("FILEN_CLI_PASSWORD")
		.env_remove("FILEN_CLI_2FA_CODE")
		.env_remove("FILEN_EMAIL")
		.env_remove("FILEN_PASSWORD")
		.env_remove("FILEN_2FA_CODE")
		.args([
			"--skip-update",
			"--config-dir",
			config_dir.path().to_str().unwrap(),
			"export-auth-config",
			path.to_str().unwrap(),
		])
		.assert()
		.code(2)
		.stdout("")
		.stderr(predicates::str::contains("Pass --yes to answer yes."))
		// the login prompt, which comes after the confirmation
		.stderr(predicates::str::contains("Not logged in").not());
	assert!(!path.exists());
}

#[shared_test_runtime]
async fn whoami_prints_the_email_of_the_account() {
	let (email, _) = get_testing_credentials();
	authenticated_cli_with_args!("whoami")
		.success()
		.stdout(format!("{email}\n"));
}

#[shared_test_runtime]
async fn whoami_with_json_prints_the_email_of_the_account() {
	let (email, _) = get_testing_credentials();
	authenticated_cli_with_args!("whoami", "--json")
		.success()
		.stdout(format!("{{\n  \"email\": \"{email}\"\n}}\n"));
}

#[test]
fn a_missing_auth_config_path_fails_without_prompting() {
	let config_dir = assert_fs::TempDir::new().unwrap();
	let missing = config_dir.path().join("missing-auth-config.txt");
	cargo_bin_cmd!()
		.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
		.env_remove("FILEN_CLI_EMAIL")
		.env_remove("FILEN_CLI_PASSWORD")
		.env_remove("FILEN_CLI_2FA_CODE")
		.env_remove("FILEN_EMAIL")
		.env_remove("FILEN_PASSWORD")
		.env_remove("FILEN_2FA_CODE")
		.args([
			"--skip-update",
			"--config-dir",
			config_dir.path().to_str().unwrap(),
			"--auth-config-path",
			missing.to_str().unwrap(),
			"stat",
			"/",
		])
		.assert()
		.code(1)
		.stdout("")
		.stderr(predicates::str::contains(format!(
			"Auth config file not found: {}",
			missing.display()
		)))
		// the prompt it fell back to before, which fails without a terminal
		.stderr(predicates::str::contains("Not logged in").not());
}

#[shared_test_runtime]
async fn a_lone_two_factor_code_does_not_keep_the_auth_config_from_being_used() {
	let (_auth_config_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
	cargo_bin_cmd!()
		.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
		.env_remove("FILEN_CLI_EMAIL")
		.env_remove("FILEN_CLI_PASSWORD")
		.env("FILEN_CLI_2FA_CODE", "123456")
		.args([
			"--auth-config-path",
			auth_config.to_str().unwrap(),
			"-v",
			"stat",
			"/",
		])
		.assert()
		.success()
		.stderr(predicates::str::contains(
			"Authenticated from auth config file",
		));
}

/// Runs `stat /` in `working_dir`, with no credentials but the auth config files it finds.
#[cfg(unix)]
fn stat_with_autodetected_auth_config(working_dir: &std::path::Path) -> assert_cmd::assert::Assert {
	let config_dir = assert_fs::TempDir::new().unwrap();
	cargo_bin_cmd!()
		.current_dir(working_dir)
		.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
		.env_remove("FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG")
		.env_remove("FILEN_CLI_EMAIL")
		.env_remove("FILEN_CLI_PASSWORD")
		.env_remove("FILEN_CLI_2FA_CODE")
		.env_remove("FILEN_EMAIL")
		.env_remove("FILEN_PASSWORD")
		.env_remove("FILEN_2FA_CODE")
		.args([
			"--skip-update",
			"--config-dir",
			config_dir.path().to_str().unwrap(),
			"stat",
			"/",
		])
		.assert()
}

#[cfg(unix)]
#[test]
fn an_auth_config_in_the_working_dir_open_to_other_users_is_refused() {
	let working_dir = assert_fs::TempDir::new().unwrap();
	// as the CLI names it, from the working directory the OS resolved (/private/var on macOS)
	let path = working_dir
		.path()
		.canonicalize()
		.unwrap()
		.join("filen-cli-auth-config.txt");
	std::fs::write(&path, "not a config").unwrap();
	std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
	stat_with_autodetected_auth_config(working_dir.path())
		.code(1)
		.stdout("")
		.stderr(predicates::str::contains(format!(
			"restrict it with `chmod 600 {}`",
			path.display()
		)));
}

#[cfg(unix)]
#[shared_test_runtime]
async fn logging_in_from_an_auth_config_in_the_working_dir_names_the_account_and_the_file() {
	let client = test_utils::RESOURCES.client().await;
	let working_dir = assert_fs::TempDir::new().unwrap();
	// as the CLI names it, from the working directory the OS resolved (/private/var on macOS)
	let path = working_dir
		.path()
		.canonicalize()
		.unwrap()
		.join("filen-cli-auth-config.txt");
	std::fs::write(&path, filen_cli::serialize_auth_config(&client).unwrap()).unwrap();
	std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
	stat_with_autodetected_auth_config(working_dir.path())
		.success()
		.stderr(predicates::str::contains(format!(
			"Logged in as {} from the auth config file {}",
			client.email(),
			path.display()
		)));
}

/// An auth config of `client`'s login with an API key the server does not know, as one revoked
/// since it was exported.
fn auth_config_with_revoked_api_key(client: &Client) -> String {
	let mut stringified = client.to_stringified();
	stringified.api_key = "0".repeat(64);
	format!(
		"filen_cli_auth_config_1:{}",
		BASE64.encode(serde_json::to_string(&stringified).unwrap())
	)
}

#[shared_test_runtime]
async fn a_given_auth_config_with_a_revoked_api_key_fails_saying_to_export_a_new_one() {
	let client = test_utils::RESOURCES.client().await;
	let config_dir = assert_fs::TempDir::new().unwrap();
	let path = config_dir.path().join("revoked-auth-config.txt");
	std::fs::write(&path, auth_config_with_revoked_api_key(&client)).unwrap();
	#[cfg(unix)]
	std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
	cargo_bin_cmd!()
		.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
		.env_remove("FILEN_CLI_EMAIL")
		.env_remove("FILEN_CLI_PASSWORD")
		.env_remove("FILEN_CLI_2FA_CODE")
		.env_remove("FILEN_EMAIL")
		.env_remove("FILEN_PASSWORD")
		.env_remove("FILEN_2FA_CODE")
		.args([
			"--skip-update",
			"--config-dir",
			config_dir.path().to_str().unwrap(),
			"--auth-config-path",
			path.to_str().unwrap(),
			"stat",
			"/",
		])
		.assert()
		.code(1)
		.stdout("")
		.stderr(predicates::str::contains(format!(
			"The auth config file {} cannot be used (the server no longer accepts this login): \
			 export a new one with `filen export-auth-config`",
			path.display()
		)));
}

#[cfg(unix)]
#[shared_test_runtime]
async fn an_autodetected_auth_config_with_a_revoked_api_key_falls_back_to_logging_in() {
	let client = test_utils::RESOURCES.client().await;
	let working_dir = assert_fs::TempDir::new().unwrap();
	// as the CLI names it, from the working directory the OS resolved (/private/var on macOS)
	let path = working_dir
		.path()
		.canonicalize()
		.unwrap()
		.join("filen-cli-auth-config.txt");
	std::fs::write(&path, auth_config_with_revoked_api_key(&client)).unwrap();
	std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
	stat_with_autodetected_auth_config(working_dir.path())
		.code(2)
		.stdout("")
		.stderr(predicates::str::contains(format!(
			"The auth config file {} cannot be used (the server no longer accepts this login)",
			path.display()
		)))
		// the prompt it fell back to, which cannot be asked without a terminal
		.stderr(predicates::str::contains("Not logged in"));
}

#[cfg(unix)]
#[shared_test_runtime]
async fn an_auth_config_of_the_old_cli_in_the_working_dir_is_imported_and_kept() {
	let client = test_utils::RESOURCES.client().await;
	let working_dir = assert_fs::TempDir::new().unwrap();
	// as the CLI names it, from the working directory the OS resolved (/private/var on macOS)
	let path = working_dir
		.path()
		.canonicalize()
		.unwrap()
		.join(".filen-cli-auth-config");
	// as the old CLI exported it: its SDK config as base64 JSON
	let contents = BASE64.encode(serde_json::to_string(&client.to_sdk_config()).unwrap());
	std::fs::write(&path, &contents).unwrap();
	std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
	stat_with_autodetected_auth_config(working_dir.path())
		.success()
		.stderr(predicates::str::contains(format!(
			"Logged in as {} with the credentials saved by the old Filen CLI at {}",
			client.email(),
			path.display()
		)));
	assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
}

// notes:
// keyring authentication is hard to test here, so ignore it for now

// todo: test prompting for 2fa code (need another test account with 2fa enabled for that?)
// -> I think there isn't currently one in CI, so do that later
