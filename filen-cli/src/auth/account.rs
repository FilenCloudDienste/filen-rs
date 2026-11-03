//! [cli-doc] account-credentials
//! `2fa enable` turns on two-factor authentication: it prints a secret (and an `otpauth://` URL with
//! it) to add to your authenticator app, asks for a code from the app (or takes it with `--code`),
//! and prints the recovery key, which logs you in without a code: store it somewhere safe.
//! With `--json`, the secret and the recovery key are printed as `{"secret", "recoveryKey"}`, and
//! the secret is shown on stderr before the code is asked for. `2fa disable` turns it off again,
//! with a code or the recovery key. Both ask to confirm, which `--yes` does. The login the CLI
//! keeps stays valid.
//!
//! `change-password` asks for your current password and the new one (twice), and changes it.
//! The login the CLI keeps is updated with it: the system keychain's, or the auth config file you
//! logged in with. A password given with `--password` or the environment has to be updated where
//! your scripts set it (and a login this config dir keeps as well, if it is of the same account,
//! stops working: the CLI says so), and logins kept elsewhere (another config dir, another device,
//! an exported auth config, the old Filen CLI's saved login) may stop working.
//!
//! `change-email <new email>` asks for your password and requests the change: Filen sends an
//! email to confirm it, and the change happens once you confirm it. Then log in again (with
//! `logout`, or with the new `--email` in your scripts), so that the kept login has the new email.
//!
//! `export-master-keys` prints your master keys, which decrypt all your files, after you confirm
//! it (which `--yes` does). Keep them where only you can read them.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use filen_sdk_rs::ErrorKind;
use filen_types::error::ResponseError;
use serde_json::json;

use filen_sdk_rs::auth::TwoFASecret;

use super::{
	KeptLogin, KeyringLocation, LazyClient, TWO_FACTOR_CODE_PROMPT,
	get_auth_config_default_locations, keyring_disabled_for_testing, write_private_file,
};
use crate::{CliConfig, serialize_auth_config, ui::UI, util::LongKeyringEntry};

const ENABLE_TWO_FACTOR_PROMPT: &str = "Enable two-factor authentication? Logging in will then \
	also need a code from your authenticator app";
const NEW_TWO_FACTOR_CODE_PROMPT: &str = "Code from your authenticator app: ";

/// Enable two-factor authentication, after the user confirmed it: print the secret for an
/// authenticator app, enable it with the app's current code, `code_arg` or asked for, and print the
/// recovery key. The login the CLI keeps stays valid: enabling it leaves the API key as it is.
pub(crate) async fn enable_two_factor(
	ui: &mut UI,
	client: &mut LazyClient,
	code_arg: Option<&str>,
) -> Result<()> {
	// before logging in, so that a script learns at once what it has to pass
	ui.ensure_can_confirm(ENABLE_TWO_FACTOR_PROMPT)?;
	if code_arg.is_none() {
		ui.ensure_can_prompt(NEW_TWO_FACTOR_CODE_PROMPT)?;
	}
	if !ui.prompt_confirm(ENABLE_TWO_FACTOR_PROMPT, true)? {
		ui.print_muted("Two-factor authentication not enabled");
		return Ok(());
	}
	let client = client.get(ui).await?;
	let secret = client
		.generate_2fa_secret()
		.await
		.map_err(|e| match e.kind() {
			ErrorKind::InvalidState => UI::failure("Two-factor authentication is already enabled"),
			_ => account_failure(e, "Failed to get a two-factor authentication secret"),
		})?;
	print_two_factor_secret(ui, &secret);
	let code = match code_arg {
		Some(code) => code.to_owned(),
		None => ui.prompt(NEW_TWO_FACTOR_CODE_PROMPT)?,
	};
	let recovery_key = client
		.enable_2fa(code.trim())
		.await
		.map_err(|e| account_failure(e, "Failed to enable two-factor authentication"))?;
	print_two_factor_enabled(ui, secret.secret(), &recovery_key)
}

/// Print `secret` for the user to add to an authenticator app: on stdout, or on stderr with
/// `--json`, whose output comes once two-factor authentication is enabled.
fn print_two_factor_secret(ui: &mut UI, secret: &TwoFASecret) {
	let lines = [
		"Add this secret to your authenticator app, or open the URL with it:".to_owned(),
		format!("Secret: {}", secret.secret()),
		format!("URL: {}", secret.url()),
	];
	for line in &lines {
		if ui.json {
			ui.print_to_stderr(line);
		} else {
			ui.print(line);
		}
	}
}

/// Say that two-factor authentication with `secret` is enabled, and print its `recovery_key` with
/// a warning, or `{"secret": ..., "recoveryKey": ...}` with `--json`.
fn print_two_factor_enabled(ui: &mut UI, secret: &str, recovery_key: &str) -> Result<()> {
	if ui.json {
		return ui.print_json(json!({ "secret": secret, "recoveryKey": recovery_key }));
	}
	ui.print_success("Enabled two-factor authentication");
	ui.print(&format!("Recovery key: {recovery_key}"));
	ui.print_warning(
		"Store the recovery key somewhere safe: it logs you in without a code if you lose your \
		 authenticator app",
	);
	Ok(())
}

const DISABLE_TWO_FACTOR_PROMPT: &str = "Disable two-factor authentication? Logging in will then \
	need only your email and password";

/// Disable two-factor authentication, after the user confirmed it, with a current code or the
/// recovery key, `code_arg` or asked for.
pub(crate) async fn disable_two_factor(
	ui: &mut UI,
	client: &mut LazyClient,
	code_arg: Option<&str>,
) -> Result<()> {
	ui.ensure_can_confirm(DISABLE_TWO_FACTOR_PROMPT)?;
	if code_arg.is_none() {
		ui.ensure_can_prompt(TWO_FACTOR_CODE_PROMPT)?;
	}
	if !ui.prompt_confirm(DISABLE_TWO_FACTOR_PROMPT, false)? {
		ui.print_muted("Two-factor authentication not disabled");
		return Ok(());
	}
	let client = client.get(ui).await?;
	let code = match code_arg {
		Some(code) => code.to_owned(),
		None => ui.prompt_password(TWO_FACTOR_CODE_PROMPT)?,
	};
	client
		.disable_2fa(code.trim())
		.await
		.map_err(|e| account_failure(e, "Failed to disable two-factor authentication"))?;
	ui.print_success("Disabled two-factor authentication");
	Ok(())
}

const CURRENT_PASSWORD_PROMPT: &str = "Current password: ";
const NEW_PASSWORD_PROMPT: &str = "New password: ";
const REPEATED_NEW_PASSWORD_PROMPT: &str = "Repeat the new password: ";

/// Change the password of the account, asking for the current and the new one, then update the
/// kept login, which the change replaced.
pub(crate) async fn change_password(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
) -> Result<()> {
	// before logging in, so that a script learns at once that this needs a terminal
	ui.ensure_can_prompt(CURRENT_PASSWORD_PROMPT)?;
	let (client, kept) = client.get_with_kept_login(ui).await?;
	let current_password = ui.prompt_password(CURRENT_PASSWORD_PROMPT)?;
	let new_password = new_password(
		ui.prompt_password(NEW_PASSWORD_PROMPT)?,
		&ui.prompt_password(REPEATED_NEW_PASSWORD_PROMPT)?,
	)?;
	client
		.change_password(&current_password, &new_password)
		.await
		.map_err(|e| account_failure(e, "Failed to change the password"))?;
	ui.print_success("Changed the password");
	// the client carries the new API key and master keys now
	let sdk_config = serialize_auth_config(client)?;
	update_kept_login(config, ui, kept, &sdk_config)
}

/// The new password typed as `first` and `repeated`, if they match and it is not empty.
fn new_password(first: String, repeated: &str) -> Result<String> {
	if first != repeated {
		return Err(UI::failure("The new passwords do not match"));
	}
	if first.is_empty() {
		return Err(UI::failure("The new password must not be empty"));
	}
	Ok(first)
}

/// Write the login `sdk_config`, whose credentials changed, to where the login was kept, or say
/// where the user has to update it.
fn update_kept_login(
	config: &CliConfig,
	ui: &mut UI,
	kept: &KeptLogin,
	sdk_config: &str,
) -> Result<()> {
	let location = KeyringLocation::of(config);
	let store = location.store();
	let keyring_entry = (!keyring_disabled_for_testing())
		.then(|| LongKeyringEntry::new(&store, &location.entry_name));
	let auth_configs = get_auth_config_default_locations(config)
		.into_iter()
		.map(|(path, _)| path)
		.collect::<Vec<_>>();
	update_kept_login_in(ui, kept, sdk_config, keyring_entry.as_ref(), &auth_configs)
}

/// [`update_kept_login`], with the keyring entry of the config dir, unless the tests turned the
/// keyring off, and the auth config files in the default locations `auth_configs`.
fn update_kept_login_in(
	ui: &mut UI,
	kept: &KeptLogin,
	sdk_config: &str,
	keyring_entry: Option<&LongKeyringEntry<'_>>,
	auth_configs: &[PathBuf],
) -> Result<()> {
	match kept {
		KeptLogin::Arguments => {
			ui.print_warning(
				"Update the password where your scripts set it: --password, FILEN_CLI_PASSWORD or \
				 FILEN_PASSWORD",
			);
			// left as it is: it may be another account's login
			if let Some(places) = other_kept_logins(keyring_entry, auth_configs) {
				ui.print_warning(&format!(
					"A login is also kept {places}: if it is of this account, it no longer works, \
					 so log in again with `filen logout`, then any command"
				));
			}
		}
		KeptLogin::OldCli(path) => ui.print_warning(&format!(
			"The credentials saved by the old Filen CLI at {}, which this login came from, were \
			 not updated and no longer work: log in again on the next run, and keep the login",
			path.display()
		)),
		KeptLogin::AuthConfig(path) => update_auth_config(ui, path, sdk_config)?,
		KeptLogin::Keyring => {
			let Some(entry) = keyring_entry else {
				log::info!("The keyring is turned off, so the kept login is not updated");
				return Ok(());
			};
			entry.write(sdk_config).map_err(|e| {
				log::warn!("Failed to update the login in the keyring: {:#}", e);
				UI::failure(
					"Failed to update the login kept in the system keychain: log in again with \
					 `filen logout`, then any command",
				)
			})?;
			ui.print_success("Updated the login kept in the system keychain");
		}
		KeptLogin::Nowhere => {}
	}
	Ok(())
}

/// Where logins are kept for the config dir, in `keyring_entry` and the auth config files
/// `auth_configs`, as in "kept {places}", or `None` if nowhere.
fn other_kept_logins(
	keyring_entry: Option<&LongKeyringEntry<'_>>,
	auth_configs: &[PathBuf],
) -> Option<String> {
	let in_keyring = keyring_entry.is_some_and(|entry| match entry.read() {
		Ok(sdk_config) => sdk_config.is_some(),
		Err(e) => {
			log::warn!("Failed to look for a login kept in the keyring: {:#}", e);
			false
		}
	});
	let places = in_keyring
		.then(|| "in the system keychain".to_owned())
		.into_iter()
		.chain(
			auth_configs
				.iter()
				.map(|path| format!("in the auth config file {}", path.display())),
		)
		.collect::<Vec<_>>();
	(!places.is_empty()).then(|| places.join(" and "))
}

/// Replace the auth config file at `path` with the login `sdk_config`.
fn update_auth_config(ui: &mut UI, path: &Path, sdk_config: &str) -> Result<()> {
	write_private_file(path, sdk_config).map_err(|e| {
		log::warn!(
			"Failed to update the auth config file {}: {:?}",
			path.display(),
			e
		);
		UI::failure(&format!(
			"Failed to update the auth config file {} ({e}): export a new one with \
			 `filen export-auth-config`",
			path.display()
		))
	})?;
	ui.print_success(&format!("Updated the auth config file {}", path.display()));
	Ok(())
}

const CHANGE_EMAIL_PASSWORD_PROMPT: &str = "Password: ";

/// Request the change of the account's email to `new_email`, asking for the password.
pub(crate) async fn change_email(
	ui: &mut UI,
	client: &mut LazyClient,
	new_email: &str,
) -> Result<()> {
	ui.ensure_can_prompt(CHANGE_EMAIL_PASSWORD_PROMPT)?;
	let (client, kept) = client.get_with_kept_login(ui).await?;
	let password = ui.prompt_password(CHANGE_EMAIL_PASSWORD_PROMPT)?;
	client
		.change_email(&password, new_email)
		.await
		.map_err(|e| account_failure(e, "Failed to change the email"))?;
	print_email_change_requested(ui, client.email(), new_email, kept);
	Ok(())
}

/// Say that the change of the email from `old_email` to `new_email` waits for its confirmation,
/// and what to update after it, given where the login is `kept`.
fn print_email_change_requested(ui: &mut UI, old_email: &str, new_email: &str, kept: &KeptLogin) {
	ui.print_success(&format!(
		"Requested to change the email from {old_email} to {new_email}: confirm it with the \
		 link Filen sends by email"
	));
	match kept {
		KeptLogin::Arguments => ui.print_warning(
			"Once it is confirmed, update the email where your scripts set it: --email, \
			 FILEN_CLI_EMAIL or FILEN_EMAIL",
		),
		KeptLogin::AuthConfig(_) | KeptLogin::Keyring => ui.print_muted_to_stderr(
			"Once it is confirmed, log in again (`filen logout`, then any command), so that the \
			 kept login has the new email",
		),
		// the old CLI's saved login keeps working: an email change leaves the API key as it is
		KeptLogin::OldCli(_) | KeptLogin::Nowhere => {}
	}
}

const EXPORT_MASTER_KEYS_PROMPT: &str = "This prints your master keys, which decrypt all your \
	files. Anyone who has them and access to your account can read everything in it. Proceed?";

/// Print the account's master keys, after the user confirmed it.
pub(crate) async fn export_master_keys(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	// before logging in, so that a script learns it needs --yes before anything else happens
	ui.ensure_can_confirm(EXPORT_MASTER_KEYS_PROMPT)?;
	if !ui.prompt_confirm(EXPORT_MASTER_KEYS_PROMPT, false)? {
		ui.print_muted("Master keys not printed");
		return Ok(());
	}
	let client = client.get(ui).await?;
	let master_keys = client
		.export_master_keys()
		.await
		.map_err(|e| account_failure(e, "Failed to export the master keys"))?;
	print_master_keys(ui, &master_keys)
}

/// Print `master_keys`, or `{"masterKeys": ...}` with `--json`.
fn print_master_keys(ui: &mut UI, master_keys: &str) -> Result<()> {
	if ui.json {
		return ui.print_json(json!({ "masterKeys": master_keys }));
	}
	ui.print(master_keys);
	ui.print_warning("Keep the master keys where only you can read them");
	Ok(())
}

/// The error for an account request that failed with `e`: the server's reason after `context`, for
/// a request the server refused, or `e` with `context` if it is unexpected.
fn account_failure(e: filen_sdk_rs::Error, context: &str) -> anyhow::Error {
	match e.kind() {
		ErrorKind::InvalidState => {
			// the SDK refuses what the account does not support, such as a V3 account's password
			log::warn!("{context}: {e}");
			UI::failure(&format!("{context}: this account does not support it"))
		}
		ErrorKind::Server
		| ErrorKind::WrongPassword
		| ErrorKind::EmailOrPasswordWrong
		| ErrorKind::Enter2fa
		| ErrorKind::Wrong2fa => match server_message(&e) {
			Some(message) => UI::failure(&format!("{context}: {message}")),
			None => anyhow!(e).context(context.to_owned()),
		},
		_ => anyhow!(e).context(context.to_owned()),
	}
}

/// The message the server refused a request with, if it gave one.
fn server_message(e: &filen_sdk_rs::Error) -> Option<&str> {
	let ResponseError::ApiError { message, .. } = e.downcast_ref::<ResponseError>()?;
	message.as_deref().filter(|message| !message.is_empty())
}

#[cfg(test)]
mod tests {
	#[cfg(unix)]
	use std::os::unix::fs::PermissionsExt;

	use super::*;
	use crate::{
		error::exit_code,
		ui::test_support::Printed,
		util::{ChunkStore as _, test_support::MemoryChunkStore},
	};

	#[test]
	fn the_two_factor_secret_is_printed_with_its_url() {
		let printed = Printed::default();
		let secret = TwoFASecret::new("JBSWY3DPEHPK3PXP".to_owned(), "user@example.com");

		print_two_factor_secret(&mut printed.ui(), &secret);

		assert_eq!(
			printed.lines(),
			"Add this secret to your authenticator app, or open the URL with it:\n\
			 Secret: JBSWY3DPEHPK3PXP\n\
			 URL: otpauth://totp/Filen:user%40example.com?secret=JBSWY3DPEHPK3PXP&issuer=Filen&\
			 digits=6&period=30"
		);
	}

	#[test]
	fn with_json_the_two_factor_secret_is_shown_on_stderr() {
		let out = Printed::default();
		let err = Printed::default();
		let mut ui = Printed::split_ui(&out, &err);
		ui.set_quiet_and_json(false, true);
		let secret = TwoFASecret::new("JBSWY3DPEHPK3PXP".to_owned(), "user@example.com");

		print_two_factor_secret(&mut ui, &secret);

		assert_eq!(out.lines_or_empty(), "");
		assert!(err.lines().contains("Secret: JBSWY3DPEHPK3PXP"));
	}

	#[test]
	fn the_recovery_key_is_printed_with_a_warning() {
		let printed = Printed::default();

		print_two_factor_enabled(&mut printed.ui(), "JBSWY3DPEHPK3PXP", "the recovery key")
			.unwrap();

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"✔ Enabled two-factor authentication\n\
			 Recovery key: the recovery key\n\
			 ⚠ Store the recovery key somewhere safe: it logs you in without a code if you lose \
			 your authenticator app"
		);
	}

	#[test]
	fn with_json_the_secret_and_the_recovery_key_are_printed_as_an_object() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.set_quiet_and_json(false, true);

		print_two_factor_enabled(&mut ui, "JBSWY3DPEHPK3PXP", "the recovery key").unwrap();

		assert_eq!(
			printed.lines(),
			"{\n  \"recoveryKey\": \"the recovery key\",\n  \"secret\": \"JBSWY3DPEHPK3PXP\"\n}"
		);
	}

	#[tokio::test]
	async fn enabling_two_factor_without_a_terminal_or_yes_fails_before_logging_in() {
		let printed = Printed::default();
		let mut client = client_never_logged_in();

		let err = enable_two_factor(&mut printed.ui(), &mut client, Some("123456"))
			.await
			.unwrap_err();

		assert_eq!(exit_code(&err), 2);
		assert!(
			format!("{err:#}").ends_with("stdin is not a terminal. Pass --yes to answer yes."),
			"{err:#}"
		);
		assert!(client.get_arc().is_none());
	}

	#[tokio::test]
	async fn enabling_two_factor_with_yes_but_without_a_code_or_a_terminal_fails_before_logging_in()
	{
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.set_assume_yes(true);
		let mut client = client_never_logged_in();

		let err = enable_two_factor(&mut ui, &mut client, None)
			.await
			.unwrap_err();

		assert_eq!(
			format!("{err:#}"),
			"Cannot ask \"Code from your authenticator app:\": stdin is not a terminal."
		);
		assert_eq!(exit_code(&err), 2);
		assert!(client.get_arc().is_none());
	}

	#[tokio::test]
	async fn disabling_two_factor_with_yes_but_without_a_code_or_a_terminal_fails_before_logging_in()
	 {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.set_assume_yes(true);
		let mut client = client_never_logged_in();

		let err = disable_two_factor(&mut ui, &mut client, None)
			.await
			.unwrap_err();

		assert_eq!(
			format!("{err:#}"),
			"Cannot ask \"Two-factor authentication code or recovery key:\": stdin is not a \
			 terminal."
		);
		assert!(client.get_arc().is_none());
	}

	#[tokio::test]
	async fn disabling_two_factor_without_a_terminal_or_yes_fails_before_logging_in() {
		let printed = Printed::default();
		let mut client = client_never_logged_in();

		let err = disable_two_factor(&mut printed.ui(), &mut client, Some("123456"))
			.await
			.unwrap_err();

		assert_eq!(exit_code(&err), 2);
		assert!(client.get_arc().is_none());
	}

	#[test]
	fn a_new_password_typed_the_same_twice_is_taken() {
		assert_eq!(
			new_password("correct horse".to_owned(), "correct horse").unwrap(),
			"correct horse"
		);
	}

	#[test]
	fn new_passwords_that_differ_fail() {
		let err = new_password("correct horse".to_owned(), "correct hose").unwrap_err();
		assert_eq!(format!("{err:#}"), "The new passwords do not match");
		assert_eq!(exit_code(&err), 1);
	}

	#[test]
	fn an_empty_new_password_fails() {
		let err = new_password(String::new(), "").unwrap_err();
		assert_eq!(format!("{err:#}"), "The new password must not be empty");
	}

	#[test]
	fn a_login_kept_in_the_keyring_is_replaced() {
		let store = MemoryChunkStore::default();
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		entry.write("the old login").unwrap();
		let printed = Printed::default();

		update_kept_login_in(
			&mut printed.ui(),
			&KeptLogin::Keyring,
			"the new login",
			Some(&entry),
			&[],
		)
		.unwrap();

		assert_eq!(entry.read().unwrap().as_deref(), Some("the new login"));
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"✔ Updated the login kept in the system keychain"
		);
	}

	#[cfg(unix)]
	#[test]
	fn a_login_kept_in_an_auth_config_file_is_replaced_owner_only() {
		let dir = assert_fs::TempDir::new().unwrap();
		let path = dir.path().join("auth-config.txt");
		std::fs::write(&path, "the old login").unwrap();
		std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
		let store = MemoryChunkStore::default();
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		let printed = Printed::default();

		update_kept_login_in(
			&mut printed.ui(),
			&KeptLogin::AuthConfig(path.clone()),
			"the new login",
			Some(&entry),
			&[],
		)
		.unwrap();

		assert_eq!(std::fs::read_to_string(&path).unwrap(), "the new login");
		assert_eq!(
			std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
			0o600
		);
		assert_eq!(store.keys(), Vec::<String>::new());
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			format!("✔ Updated the auth config file {}", path.display())
		);
	}

	#[test]
	fn an_auth_config_file_that_cannot_be_written_fails_saying_to_export_a_new_one() {
		let dir = assert_fs::TempDir::new().unwrap();
		// a directory, which cannot be opened as a file
		let path = dir.path().to_path_buf();
		let printed = Printed::default();

		let err = update_kept_login_in(
			&mut printed.ui(),
			&KeptLogin::AuthConfig(path.clone()),
			"the new login",
			None,
			&[],
		)
		.unwrap_err();

		let message = format!("{err:#}");
		assert!(
			message.starts_with(&format!(
				"Failed to update the auth config file {} (",
				path.display()
			)),
			"{message}"
		);
		assert!(
			message.ends_with("): export a new one with `filen export-auth-config`"),
			"{message}"
		);
	}

	#[test]
	fn a_password_given_in_the_arguments_is_left_to_the_scripts() {
		let store = MemoryChunkStore::default();
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		let printed = Printed::default();

		update_kept_login_in(
			&mut printed.ui(),
			&KeptLogin::Arguments,
			"the new login",
			Some(&entry),
			&[],
		)
		.unwrap();

		assert_eq!(store.keys(), Vec::<String>::new());
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"⚠ Update the password where your scripts set it: --password, FILEN_CLI_PASSWORD or \
			 FILEN_PASSWORD"
		);
	}

	#[test]
	fn logins_kept_besides_a_password_given_in_the_arguments_are_left_with_a_warning() {
		let store = MemoryChunkStore::default();
		store.set("sdk-config_0", "another login").unwrap();
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		let dir = assert_fs::TempDir::new().unwrap();
		let auth_config = dir.path().join("filen-cli-auth-config.txt");
		std::fs::write(&auth_config, "another login").unwrap();
		let printed = Printed::default();

		update_kept_login_in(
			&mut printed.ui(),
			&KeptLogin::Arguments,
			"the new login",
			Some(&entry),
			std::slice::from_ref(&auth_config),
		)
		.unwrap();

		assert_eq!(entry.read().unwrap().as_deref(), Some("another login"));
		assert_eq!(
			std::fs::read_to_string(&auth_config).unwrap(),
			"another login"
		);
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			format!(
				"⚠ Update the password where your scripts set it: --password, FILEN_CLI_PASSWORD \
				 or FILEN_PASSWORD\n\
				 ⚠ A login is also kept in the system keychain and in the auth config file {}: if \
				 it is of this account, it no longer works, so log in again with `filen logout`, \
				 then any command",
				auth_config.display()
			)
		);
	}

	#[test]
	fn a_login_imported_from_the_old_cli_and_not_kept_is_said_to_be_out_of_date() {
		let store = MemoryChunkStore::default();
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		let path = PathBuf::from("/home/user/.filen-cli-auth-config");
		let printed = Printed::default();

		update_kept_login_in(
			&mut printed.ui(),
			&KeptLogin::OldCli(path),
			"the new login",
			Some(&entry),
			&[],
		)
		.unwrap();

		assert_eq!(store.keys(), Vec::<String>::new());
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"⚠ The credentials saved by the old Filen CLI at /home/user/.filen-cli-auth-config, \
			 which this login came from, were not updated and no longer work: log in again on \
			 the next run, and keep the login"
		);
	}

	#[test]
	fn a_login_kept_nowhere_has_nothing_to_update() {
		let store = MemoryChunkStore::default();
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		let printed = Printed::default();

		update_kept_login_in(
			&mut printed.ui(),
			&KeptLogin::Nowhere,
			"the new login",
			Some(&entry),
			&[],
		)
		.unwrap();

		assert_eq!(store.keys(), Vec::<String>::new());
		assert_eq!(printed.lines_or_empty(), "");
	}

	#[test]
	fn a_requested_email_change_says_to_confirm_it_and_to_update_the_scripts() {
		let printed = Printed::default();

		print_email_change_requested(
			&mut printed.ui(),
			"old@example.com",
			"new@example.com",
			&KeptLogin::Arguments,
		);

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"✔ Requested to change the email from old@example.com to new@example.com: confirm it \
			 with the link Filen sends by email\n\
			 ⚠ Once it is confirmed, update the email where your scripts set it: --email, \
			 FILEN_CLI_EMAIL or FILEN_EMAIL"
		);
	}

	#[test]
	fn a_requested_email_change_of_a_kept_login_says_to_log_in_again() {
		let printed = Printed::default();

		print_email_change_requested(
			&mut printed.ui(),
			"old@example.com",
			"new@example.com",
			&KeptLogin::Keyring,
		);

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"✔ Requested to change the email from old@example.com to new@example.com: confirm it \
			 with the link Filen sends by email\n\
			 Once it is confirmed, log in again (`filen logout`, then any command), so that the \
			 kept login has the new email"
		);
	}

	#[test]
	fn master_keys_are_printed_with_a_warning() {
		let printed = Printed::default();
		print_master_keys(&mut printed.ui(), "the master keys").unwrap();
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"the master keys\n⚠ Keep the master keys where only you can read them"
		);
	}

	#[test]
	fn master_keys_with_json_are_printed_as_an_object() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.set_quiet_and_json(false, true);
		print_master_keys(&mut ui, "the master keys").unwrap();
		assert_eq!(
			printed.lines(),
			"{\n  \"masterKeys\": \"the master keys\"\n}"
		);
	}

	/// A client that is never logged in: the tests below fail before it would be.
	fn client_never_logged_in() -> LazyClient {
		LazyClient::new(
			CliConfig {
				config_dir: std::env::temp_dir(),
				profiles: crate::auth::Profiles::new(&std::env::temp_dir(), None),
			},
			None,
			None,
			None,
			None,
			crate::ClientConfigArgs::default(),
		)
	}

	#[tokio::test]
	async fn exporting_the_master_keys_without_a_terminal_or_yes_fails_before_logging_in() {
		let printed = Printed::default();
		let mut client = client_never_logged_in();

		let err = export_master_keys(&mut printed.ui(), &mut client)
			.await
			.unwrap_err();

		assert_eq!(exit_code(&err), 2);
		assert!(
			format!("{err:#}").ends_with("stdin is not a terminal. Pass --yes to answer yes."),
			"{err:#}"
		);
		assert!(client.get_arc().is_none());
	}

	#[tokio::test]
	async fn changing_the_password_without_a_terminal_fails_before_logging_in() {
		let printed = Printed::default();
		let config = CliConfig {
			config_dir: std::env::temp_dir(),
			profiles: crate::auth::Profiles::new(&std::env::temp_dir(), None),
		};
		let mut client = client_never_logged_in();

		let err = change_password(&config, &mut printed.ui(), &mut client)
			.await
			.unwrap_err();

		assert_eq!(
			format!("{err:#}"),
			"Cannot ask \"Current password:\": stdin is not a terminal."
		);
		assert_eq!(exit_code(&err), 2);
		assert!(client.get_arc().is_none());
	}

	#[tokio::test]
	async fn changing_the_email_without_a_terminal_fails_before_logging_in() {
		let printed = Printed::default();
		let mut client = client_never_logged_in();

		let err = change_email(&mut printed.ui(), &mut client, "new@example.com")
			.await
			.unwrap_err();

		assert_eq!(
			format!("{err:#}"),
			"Cannot ask \"Password:\": stdin is not a terminal."
		);
		assert!(client.get_arc().is_none());
	}
}
