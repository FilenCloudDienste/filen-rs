#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{panic::AssertUnwindSafe, time::Duration};

use assert_cmd::cargo::cargo_bin_cmd;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use filen_macros::shared_test_runtime;
use filen_sdk_rs::auth::{Client, TwoFASecret};
use predicates::prelude::PredicateBooleanExt as _;
use test_utils::{authenticated_cli_with_args, cli::logged_out_command};

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
	assert_cmd::Command::from_std(logged_out_command(
		assert_cmd::cargo::cargo_bin!(),
		config_dir.path(),
	))
	.env("FILEN_EMAIL", &email)
	.env("FILEN_PASSWORD", &password)
	.args(["--skip-update", "-v", "stat", "/"])
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
	assert_cmd::Command::from_std(logged_out_command(
		assert_cmd::cargo::cargo_bin!(),
		config_dir.path(),
	))
	.args([
		"--skip-update",
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

#[test]
fn logout_with_yes_deletes_the_given_auth_config_and_the_cache() {
	// logout also deletes ./filen-cli-auth-config.txt, so it must not run in the package directory
	let working_dir = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let elsewhere = assert_fs::TempDir::new().unwrap();
	let auth_config = elsewhere.path().join("my-auth-config.txt");
	std::fs::write(&auth_config, "filen_cli_auth_config_1:").unwrap();
	let cache = config_dir.path().join("filen-sdk-rs-cache");
	std::fs::write(&cache, "cache").unwrap();
	cargo_bin_cmd!()
		.current_dir(working_dir.path())
		.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
		.args([
			"--skip-update",
			"--config-dir",
			config_dir.path().to_str().unwrap(),
			"--auth-config-path",
			auth_config.to_str().unwrap(),
			"logout",
			"--yes",
		])
		.assert()
		.success()
		.stdout(predicates::str::contains(format!(
			"Deleted auth config file at {}",
			auth_config.display()
		)))
		.stdout(predicates::str::contains(format!(
			"Deleted the local cache at {}",
			cache.display()
		)));
	assert!(!auth_config.exists());
	assert!(!cache.exists());
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

/// The CLI in an empty working dir with `config_dir`, and with no credentials but those a test
/// gives it: no keyring, no autodetected auth config, none of the credential variables.
fn cli_without_stored_credentials(
	working_dir: &std::path::Path,
	config_dir: &std::path::Path,
) -> assert_cmd::Command {
	let mut cmd = assert_cmd::Command::from_std(logged_out_command(
		assert_cmd::cargo::cargo_bin!(),
		config_dir,
	));
	cmd.current_dir(working_dir).arg("--skip-update");
	cmd
}

/// Enables 2FA on the test account, retrying while the server refuses the code, and returns the
/// recovery key.
async fn enable_two_factor(client: &Client, secret: &TwoFASecret) -> String {
	for _ in 0..10 {
		match client
			.enable_2fa(&secret.make_totp_code(chrono::Utc::now()).unwrap())
			.await
		{
			Ok(recovery_key) => return recovery_key,
			Err(e) => {
				eprintln!("Failed to enable 2FA, retrying: {e}");
				tokio::time::sleep(Duration::from_secs(5)).await;
			}
		}
	}
	panic!("Failed to enable 2FA after 10 attempts");
}

/// Disables 2FA on the test account with the recovery key, or, if a login with that key turned
/// 2FA off or replaced the key, checks that it is off or disables it with a code of `secret`.
async fn disable_two_factor(client: &Client, secret: &TwoFASecret, recovery_key: &str) {
	let Err(e) = client.disable_2fa(recovery_key).await else {
		return;
	};
	eprintln!("Failed to disable 2FA with the recovery key: {e}");
	// only succeeds while 2FA is off
	if client.generate_2fa_secret().await.is_ok() {
		return;
	}
	client
		.disable_2fa(&secret.make_totp_code(chrono::Utc::now()).unwrap())
		.await
		.expect("2FA could not be disabled: disable it on the test account by hand");
}

/// Runs `run` with the 2FA secret and the recovery key while 2FA is enabled on the test account,
/// and disables it again afterwards, also when `run` panicked: while 2FA is on, every other login
/// to the shared account fails.
async fn with_two_factor_enabled(run: impl FnOnce(&TwoFASecret, &str)) {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client.lock_auth().await.unwrap();
	let secret = client.generate_2fa_secret().await.unwrap();
	let recovery_key = enable_two_factor(&client, &secret).await;
	let result = std::panic::catch_unwind(AssertUnwindSafe(|| run(&secret, &recovery_key)));
	disable_two_factor(&client, &secret, &recovery_key).await;
	if let Err(panic) = result {
		std::panic::resume_unwind(panic);
	}
}

#[shared_test_runtime]
#[ignore = "toggles 2FA on the shared account"]
async fn a_two_factor_code_is_made_from_the_secret_in_the_environment() {
	let (email, password) = get_testing_credentials();
	let working_dir = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	with_two_factor_enabled(|secret, _| {
		// twice, as a script would: the server takes each code once, and enabling 2FA used one,
		// so each run may find its first code used and have to wait for the next one
		for _ in 0..2 {
			cli_without_stored_credentials(working_dir.path(), config_dir.path())
				.env("FILEN_CLI_EMAIL", &email)
				.env("FILEN_CLI_PASSWORD", &password)
				.env("FILEN_CLI_2FA_SECRET", secret.secret())
				.args(["--no-input", "-v", "stat", "/"])
				.assert()
				.success()
				.stderr(predicates::str::contains(
					"Logging in with a two-factor code made from FILEN_CLI_2FA_SECRET",
				))
				.stderr(predicates::str::contains(secret.secret()).not());
		}
	})
	.await;
}

#[shared_test_runtime]
#[ignore = "toggles 2FA on the shared account"]
async fn a_wrong_two_factor_code_given_with_the_flag_fails_without_asking_again() {
	let (email, password) = get_testing_credentials();
	let working_dir = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	with_two_factor_enabled(|secret, _| {
		// the code of an hour ago, which the server no longer accepts
		let wrong_code = secret
			.make_totp_code(chrono::Utc::now() - chrono::Duration::hours(1))
			.unwrap();
		cli_without_stored_credentials(working_dir.path(), config_dir.path())
			.args([
				"--email",
				&email,
				"--password",
				&password,
				"--two-factor-code",
				&wrong_code,
				"--no-input",
				"stat",
				"/",
			])
			.assert()
			.code(1)
			.stdout("")
			.stderr(predicates::str::contains(
				"Two-factor authentication code wrong",
			))
			.stderr(predicates::str::contains("Cannot ask").not());
	})
	.await;
}

#[shared_test_runtime]
#[ignore = "toggles 2FA on the shared account"]
async fn the_recovery_key_is_taken_as_a_two_factor_code() {
	let (email, password) = get_testing_credentials();
	let working_dir = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	with_two_factor_enabled(|_, recovery_key| {
		cli_without_stored_credentials(working_dir.path(), config_dir.path())
			.args([
				"--email",
				&email,
				"--password",
				&password,
				"--two-factor-code",
				recovery_key,
				"--no-input",
				"-v",
				"stat",
				"/",
			])
			.assert()
			.success()
			.stderr(predicates::str::contains(
				"Authenticated from CLI arguments",
			));
	})
	.await;
}

/// Runs the CLI with `args`, logged in with the auth config at `auth_config`.
fn cli_with_auth_config(
	working_dir: &std::path::Path,
	config_dir: &std::path::Path,
	auth_config: &std::path::Path,
	args: &[&str],
) -> assert_cmd::assert::Assert {
	cli_without_stored_credentials(working_dir, config_dir)
		.args([
			"--auth-config-path",
			auth_config.to_str().unwrap(),
			"--no-input",
		])
		.args(args)
		.assert()
}

#[shared_test_runtime]
#[ignore = "toggles 2FA on the shared account"]
async fn two_factor_authentication_is_enabled_and_disabled_with_the_cli() {
	let client = test_utils::RESOURCES.client().await;
	let working_dir = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let auth_config = config_dir.path().join("auth-config.txt");
	std::fs::write(
		&auth_config,
		filen_cli::serialize_auth_config(&client).unwrap(),
	)
	.unwrap();
	#[cfg(unix)]
	std::fs::set_permissions(&auth_config, std::fs::Permissions::from_mode(0o600)).unwrap();
	// the secret the server keeps until 2FA is enabled, which the CLI gets too
	let secret = client.generate_2fa_secret().await.unwrap();

	// the CLI takes the auth lock itself, so the test must not hold it
	let mut enabled = None;
	for _ in 0..5 {
		let code = secret.make_totp_code(chrono::Utc::now()).unwrap();
		let assert = cli_with_auth_config(
			working_dir.path(),
			config_dir.path(),
			&auth_config,
			&["--yes", "--json", "2fa", "enable", "--code", &code],
		);
		let output = assert.get_output();
		if output.status.success() {
			enabled = Some(output.clone());
			break;
		}
		eprintln!(
			"Failed to enable 2FA, retrying: {}",
			String::from_utf8_lossy(&output.stderr)
		);
		tokio::time::sleep(Duration::from_secs(10)).await;
	}
	let Some(enabled) = enabled else {
		// in case a run enabled 2FA and failed after it, without printing the recovery key
		if client.generate_2fa_secret().await.is_err() {
			disable_two_factor(&client, &secret, "").await;
		}
		panic!("the CLI could not enable 2FA");
	};
	let printed: serde_json::Value = serde_json::from_slice(&enabled.stdout).unwrap();
	let recovery_key = printed["recoveryKey"].as_str().unwrap().to_owned();

	let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
		assert_eq!(printed["secret"].as_str(), Some(secret.secret()));
		assert!(!recovery_key.is_empty());
		assert!(
			String::from_utf8_lossy(&enabled.stderr)
				.contains(&format!("Secret: {}", secret.secret()))
		);
		// the kept login stays valid
		cli_with_auth_config(
			working_dir.path(),
			config_dir.path(),
			&auth_config,
			&["stat", "/"],
		)
		.success();
		cli_with_auth_config(
			working_dir.path(),
			config_dir.path(),
			&auth_config,
			&["--yes", "2fa", "disable", "--code", &recovery_key],
		)
		.success()
		.stdout(predicates::str::contains(
			"Disabled two-factor authentication",
		));
	}));
	// 2FA stays off for the other tests, also when the CLI did not disable it
	if client.generate_2fa_secret().await.is_err() {
		disable_two_factor(&client, &secret, &recovery_key).await;
	}
	if let Err(panic) = result {
		std::panic::resume_unwind(panic);
	}
}

#[test]
fn authenticate_from_the_credential_variables() {
	let (email, password) = get_testing_credentials();
	let working_dir = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	cli_without_stored_credentials(working_dir.path(), config_dir.path())
		.env("FILEN_CLI_EMAIL", &email)
		.env("FILEN_CLI_PASSWORD", &password)
		.args(["-v", "stat", "/"])
		.assert()
		.success()
		.stderr(predicates::str::contains(
			"Authenticated from CLI arguments",
		));
}

#[shared_test_runtime]
async fn the_arguments_win_over_an_auth_config_in_the_config_dir() {
	let client = test_utils::RESOURCES.client().await;
	let (email, password) = get_testing_credentials();
	let working_dir = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let auth_config = config_dir.path().join("filen-cli-auth-config.txt");
	std::fs::write(
		&auth_config,
		filen_cli::serialize_auth_config(&client).unwrap(),
	)
	.unwrap();
	#[cfg(unix)]
	std::fs::set_permissions(&auth_config, std::fs::Permissions::from_mode(0o600)).unwrap();
	cli_without_stored_credentials(working_dir.path(), config_dir.path())
		// so that the auth config in the config dir would be used without the arguments
		.env_remove("FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG")
		.args([
			"--email",
			&email,
			"--password",
			&password,
			"-v",
			"stat",
			"/",
		])
		.assert()
		.success()
		.stderr(predicates::str::contains(
			"Authenticated from CLI arguments",
		))
		.stderr(predicates::str::contains("Authenticated from auth config file").not());
}

/// An auth config file in `config_dir`, with contents that are never read.
fn auth_config_in(config_dir: &std::path::Path) -> std::path::PathBuf {
	let auth_config = config_dir.join("filen-cli-auth-config.txt");
	std::fs::write(&auth_config, "filen_cli_auth_config_1:").unwrap();
	auth_config
}

#[test]
fn logout_without_a_terminal_fails_asking_for_yes() {
	let working_dir = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let auth_config = auth_config_in(config_dir.path());
	cli_without_stored_credentials(working_dir.path(), config_dir.path())
		.arg("logout")
		.assert()
		.code(2)
		.stderr(predicates::str::contains(format!(
			"Cannot ask \"Delete auth config file at {}?\": stdin is not a terminal. \
			 Pass --yes to answer yes.",
			auth_config.display()
		)));
	assert!(auth_config.exists());
}

#[test]
fn logout_with_yes_deletes_the_auth_config_in_the_config_dir() {
	let working_dir = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let auth_config = auth_config_in(config_dir.path());
	cli_without_stored_credentials(working_dir.path(), config_dir.path())
		.args(["logout", "--yes"])
		.assert()
		.success()
		.stdout(predicates::str::contains(format!(
			"Deleted auth config file at {}",
			auth_config.display()
		)));
	assert!(!auth_config.exists());
}

#[test]
fn a_corrupt_auth_config_given_with_the_flag_fails_saying_to_export_a_new_one() {
	let working_dir = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let auth_config = config_dir.path().join("corrupt-auth-config.txt");
	std::fs::write(&auth_config, "not an auth config").unwrap();
	#[cfg(unix)]
	std::fs::set_permissions(&auth_config, std::fs::Permissions::from_mode(0o600)).unwrap();
	cli_without_stored_credentials(working_dir.path(), config_dir.path())
		.args([
			"--auth-config-path",
			auth_config.to_str().unwrap(),
			"stat",
			"/",
		])
		.assert()
		.code(1)
		.stdout("")
		.stderr(predicates::str::contains(format!(
			"The auth config file {} cannot be used (",
			auth_config.display()
		)))
		.stderr(predicates::str::contains(
			"export a new one with `filen export-auth-config`",
		))
		// the prompt, which a given auth config never falls back to
		.stderr(predicates::str::contains("Not logged in").not());
}

// Saving a login in the keyring, splitting it into chunks and deleting the stale chunks of a
// longer login are unit-tested against an in-memory store in `util.rs`, since these tests must
// not touch the keyring of whoever runs them.

#[test]
fn a_profile_gets_its_config_dir_on_first_use_and_is_listed() {
	let working_dir = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	cli_without_stored_credentials(working_dir.path(), config_dir.path())
		.args(["--profile", "work", "profiles"])
		.assert()
		.success()
		.stdout("* work\n");
	// the profile's logs are in its own config dir
	assert!(
		config_dir
			.path()
			.join("profiles")
			.join("work")
			.join("logs")
			.is_dir()
	);
	cli_without_stored_credentials(working_dir.path(), config_dir.path())
		.env("FILEN_CLI_PROFILE", "home")
		.args(["profiles"])
		.assert()
		.success()
		.stdout("* home\n  work\n");
}

#[test]
fn a_profile_name_that_is_a_path_is_refused() {
	let working_dir = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	cli_without_stored_credentials(working_dir.path(), config_dir.path())
		.args(["--profile", "../work", "profiles"])
		.assert()
		.code(2)
		.stderr(predicates::str::contains(
			"a profile name may only contain letters, digits, `-` and `_`",
		));
	assert!(!config_dir.path().join("profiles").exists());
}
