//! Tests of `statfs` and `account`. The live ones only read the account: none of them
//! changes a setting or deletes anything.

use std::process::{Command, Stdio};

use assert_fs::TempDir;
use filen_macros::shared_test_runtime;
use serde_json::Value;
use test_utils::authenticated_cli_with_args;

fn stdout_of(assert: &assert_cmd::assert::Assert) -> String {
	String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn json_of(assert: &assert_cmd::assert::Assert) -> Value {
	let stdout = stdout_of(assert);
	serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

#[shared_test_runtime]
async fn statfs_prints_the_storage_usage() {
	let client = test_utils::RESOURCES.client().await;
	let info = client.get_user_info().await.unwrap();

	let stdout = stdout_of(&authenticated_cli_with_args!("statfs").success());
	for row in [
		"Used",
		"Max",
		"Free",
		"Percent used",
		"Versions",
		"Unfinished",
	] {
		assert!(stdout.contains(row), "{row}: {stdout}");
	}

	let json = json_of(&authenticated_cli_with_args!("statfs", "--json").success());
	assert_eq!(json["max"], info.max_storage);
	assert_eq!(
		json["free"],
		json["max"]
			.as_u64()
			.unwrap()
			.saturating_sub(json["used"].as_u64().unwrap())
	);
	assert!(json["used"].is_u64(), "{json}");
	assert!(json["versionedFiles"].is_u64(), "{json}");
	assert!(json["versionedStorage"].is_u64(), "{json}");
	assert!(json["unfinishedStorage"].is_u64(), "{json}");
	assert!(
		json["percentUsed"].is_f64() || info.max_storage == 0,
		"{json}"
	);
	assert!(json.get("email").is_none(), "{json}");
}

#[shared_test_runtime]
async fn account_shows_the_email_and_settings_but_no_secret() {
	let client = test_utils::RESOURCES.client().await;
	let info = client.get_user_info().await.unwrap();

	let stdout = stdout_of(&authenticated_cli_with_args!("account").success());
	assert!(stdout.contains(client.email()), "{stdout}");
	for row in [
		"Plan",
		"Used",
		"Two-factor auth",
		"Versioning",
		"Login alerts",
	] {
		assert!(stdout.contains(row), "{row}: {stdout}");
	}
	if let Some(two_factor_key) = &info.two_factor_key {
		assert!(!stdout.contains(two_factor_key.as_str()), "{stdout}");
	}

	// `account info` is what `account` runs
	let info_stdout = stdout_of(&authenticated_cli_with_args!("account", "info").success());
	assert!(info_stdout.contains(client.email()), "{info_stdout}");
}

#[shared_test_runtime]
async fn account_as_json_has_the_settings_the_sdk_reports() {
	let client = test_utils::RESOURCES.client().await;
	let info = client.get_user_info().await.unwrap();

	let assert = authenticated_cli_with_args!("account", "--json").success();
	let json = json_of(&assert);
	assert_eq!(json["email"], client.email());
	assert!(json["plan"].is_string(), "{json}");
	assert_eq!(json["max"], info.max_storage);
	assert_eq!(json["twoFactorEnabled"], info.two_factor_enabled);
	assert_eq!(json["versioningEnabled"], info.versioning_enabled);
	assert_eq!(json["loginAlertsEnabled"], info.login_alerts_enabled);
	if let Some(two_factor_key) = &info.two_factor_key {
		assert!(
			!stdout_of(&assert).contains(two_factor_key.as_str()),
			"{json}"
		);
	}
}

/// Needs no account: the confirmation is checked before logging in.
#[test]
fn deleting_versions_without_a_terminal_or_yes_is_a_usage_error() {
	let config_dir = TempDir::new().unwrap();
	let output = Command::new(assert_cmd::cargo::cargo_bin!())
		.arg("--config-dir")
		.arg(config_dir.path())
		.args(["--skip-update", "account", "delete-versions"])
		.stdin(Stdio::null())
		.output()
		.unwrap();

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
	let stderr = String::from_utf8(output.stderr).unwrap();
	assert!(stderr.contains("Pass --yes to answer yes."), "{stderr}");
}
