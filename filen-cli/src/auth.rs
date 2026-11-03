//! [cli-doc] auth-methods
//! There are multiple ways to authenticate:
//! - set the CLI arguments `--email` (or environment variable `FILEN_CLI_EMAIL`)
//!   and `--password` (`FILEN_CLI_PASSWORD`), optionally `--two-factor-code` (`FILEN_CLI_2FA_CODE`)
//!   (when the two-factor code is omitted and required, you will be prompted for it)
//! - specify an auth config via the `--auth-config-path` flag
//!   (exported via `filen export-auth-config`),
//!   or put an auth config in one of the default locations:
//!   (unless overwritten by the `--config-dir` flag)
//!   - `./filen-cli-auth-config.txt` (current working directory)
//!   - Linux/macOS: `$HOME/.filen-cli/filen-cli-auth-config.txt`
//!   - Windows: `%appdata%\filen-cli\filen-cli-auth-config.txt`
//!
//!   (on Linux/macOS, an auth config in a default location must not be accessible
//!   to other users: restrict it with `chmod 600`)
//! - if none of these is set, you will be prompted for credentials,
//!   with the option to save them securely in the system keychain

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
	path::{Path, PathBuf},
	sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use filen_sdk_rs::{
	ErrorKind,
	auth::{Client, unauth::UnauthClient},
};

use crate::{
	CliConfig, ClientConfigArgs, build_client_config, deserialize_auth_config,
	serialize_auth_config, ui::UI, util::LongKeyringEntry,
};

/// A lazily authenticated client.
/// Since some commands (e. g. logout) don't need the user to be authenticated, we only authenticate when necessary.
pub(crate) enum LazyClient {
	Unauthenticated {
		config: CliConfig,
		email_arg: Option<String>,
		password_arg: Option<String>,
		auth_config_path_arg: Option<String>,
		two_factor_code_arg: Option<String>,
		client_config_args: ClientConfigArgs,
	},
	Authenticated {
		client: Arc<Client>,
		cache_callback_rx: tokio::sync::mpsc::UnboundedReceiver<filen_sdk_rs::cache::CacheMessage>,
	},
}

impl LazyClient {
	pub(crate) fn new(
		config: CliConfig,
		email_arg: Option<String>,
		password_arg: Option<String>,
		two_factor_code_arg: Option<String>,
		auth_config_path_arg: Option<String>,
		client_config_args: ClientConfigArgs,
	) -> Self {
		Self::Unauthenticated {
			config,
			email_arg,
			password_arg,
			two_factor_code_arg,
			auth_config_path_arg,
			client_config_args,
		}
	}

	pub(crate) async fn get(&mut self, ui: &mut UI) -> Result<&Arc<Client>> {
		match self {
			Self::Authenticated { client, .. } => Ok(client),
			Self::Unauthenticated {
				config,
				email_arg,
				password_arg,
				auth_config_path_arg,
				two_factor_code_arg,
				client_config_args,
			} => {
				let client = authenticate_and_get_password(
					config,
					ui,
					email_arg.as_deref(),
					password_arg.as_deref(),
					two_factor_code_arg.as_deref(),
					auth_config_path_arg.as_deref(),
					client_config_args,
				)
				.await?;
				let (cache_callback_tx, cache_callback_rx) = tokio::sync::mpsc::unbounded_channel();
				client
					.configure_cache(config.config_dir.join("filen-sdk-rs-cache"), move |msgs| {
						for msg in msgs {
							cache_callback_tx.send(msg).unwrap_or_else(|e| {
								log::warn!("Failed to send cache callback message: {:?}", e);
							});
						}
					})
					.await
					.context("Failed to configure cache")?;
				*self = Self::Authenticated {
					client: Arc::new(client),
					cache_callback_rx,
				};
				let Self::Authenticated { client, .. } = self else {
					unreachable!();
				};
				Ok(client)
			}
		}
	}

	pub(crate) fn get_arc(&self) -> Option<Arc<Client>> {
		match self {
			Self::Authenticated { client, .. } => Some(client.clone()),
			Self::Unauthenticated { .. } => None,
		}
	}

	pub(crate) fn get_cache_callback_rx(
		&mut self,
	) -> Option<&mut tokio::sync::mpsc::UnboundedReceiver<filen_sdk_rs::cache::CacheMessage>> {
		match self {
			Self::Authenticated {
				cache_callback_rx, ..
			} => Some(cache_callback_rx),
			Self::Unauthenticated { .. } => None,
		}
	}
}

/// Authenticate by one of the available authentication methods.
/// Also returns the password (it's needed for Rclone config).
pub(crate) async fn authenticate_and_get_password(
	config: &CliConfig,
	ui: &mut UI,
	email_arg: Option<&str>,
	password_arg: Option<&str>,
	two_factor_code_arg: Option<&str>,
	auth_config_path_arg: Option<&str>,
	client_config_args: &ClientConfigArgs,
) -> Result<Client> {
	if let Some(credentials) = cli_credentials(email_arg, password_arg, two_factor_code_arg)? {
		let client = login_and_optionally_prompt_two_factor_code(
			ui,
			credentials.email,
			credentials.password,
			credentials.two_factor_code,
			client_config_args,
		)
		.await?;
		log::info!("Authenticated from CLI arguments");
		Ok(client)
	} else if let Some((client, export_path)) =
		authenticate_from_auth_config(config, ui, auth_config_path_arg, client_config_args)?
	{
		log::info!(
			"Authenticated from auth config file {}",
			export_path.display()
		);
		Ok(client)
	} else {
		match authenticate_from_keyring(client_config_args).await {
			Ok(Some(client)) => {
				log::info!("Authenticated from keyring");
				Ok(client)
			}
			Ok(None) => authenticate_from_prompt(config, ui, client_config_args).await,
			Err(e) => {
				log::warn!("Failed to authenticate from keyring: {:?}", e);
				authenticate_from_prompt(config, ui, client_config_args).await
			}
		}
	}
}

async fn login_and_optionally_prompt_two_factor_code(
	ui: &mut UI,
	email: &str,
	password: &str,
	two_factor_code: Option<&str>,
	client_config_args: &ClientConfigArgs,
) -> Result<Client> {
	let unauth_client = UnauthClient::from_config(build_client_config(client_config_args))?;

	match unauth_client
		.login(
			email.to_owned(),
			password,
			two_factor_code.unwrap_or("XXXXXX"),
		)
		.await
	{
		Ok(client) => Ok(client),
		Err(e) if e.kind() == ErrorKind::Enter2fa => {
			let two_factor_code = ui.prompt_password("Two-factor authentication code: ")?;
			match unauth_client
				.login(email.to_owned(), password, two_factor_code.trim())
				.await
			{
				Ok(client) => Ok(client),
				Err(e) if e.kind() == ErrorKind::Wrong2fa => {
					Err(UI::failure("Two-factor authentication code wrong"))
				}
				Err(e) => Err(anyhow!(e)).context("Failed to log in (with 2fa)"),
			}
		}
		Err(e) if e.kind() == ErrorKind::EmailOrPasswordWrong => {
			Err(UI::failure("Email or password wrong"))
		}
		Err(e) => Err(anyhow!(e)).context("Failed to log in"),
	}
}

/// Credentials given in the CLI arguments.
struct CliCredentials<'a> {
	email: &'a str,
	password: &'a str,
	two_factor_code: Option<&'a str>,
}

/// The credentials to log in with from the CLI arguments, if an email or a password was given.
/// A two-factor code alone, usually a `FILEN_CLI_2FA_CODE` left in the environment, is ignored,
/// so that it does not keep the login from going on through an auth config or the keyring.
fn cli_credentials<'a>(
	email_arg: Option<&'a str>,
	password_arg: Option<&'a str>,
	two_factor_code_arg: Option<&'a str>,
) -> Result<Option<CliCredentials<'a>>> {
	match (email_arg, password_arg) {
		(Some(email), Some(password)) => Ok(Some(CliCredentials {
			email,
			password,
			two_factor_code: two_factor_code_arg,
		})),
		(Some(_), None) => Err(UI::failure("Password is required")),
		(None, Some(_)) => Err(UI::failure("Email is required")),
		(None, None) => {
			if two_factor_code_arg.is_some() {
				log::info!(
					"Ignoring the two-factor code, which is only used with an email and a password"
				);
			}
			Ok(None)
		}
	}
}

/// Authenticate using SDK config stored in a file ("auth config") exported from the CLI.
/// Checks the path provided via CLI argument (if any), or default locations: (! referenced in module docs)
/// - `./filen-cli-auth-config.txt` (current working directory)
/// - `{config_dir}/filen-cli-auth-config.txt`
fn authenticate_from_auth_config(
	config: &CliConfig,
	ui: &mut UI,
	path_arg: Option<&str>,
	client_config_args: &ClientConfigArgs,
) -> Result<Option<(Client, PathBuf)>> {
	let (path, location) = if let Some(path) = path_arg {
		// a path given explicitly must be used, never skipped for another account's credentials
		(PathBuf::from(path), AuthConfigLocation::Given)
	} else if std::env::var("FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG")
		== Ok("1".to_string())
	{
		return Ok(None);
	} else if let Some(found) = get_auth_config_default_locations(config).into_iter().next() {
		found
	} else {
		return Ok(None);
	};
	check_auth_config_access(ui, &path, location)?;
	let sdk_config = read_auth_config(&path)?;
	let client = deserialize_auth_config(&sdk_config, client_config_args)?;
	if location == AuthConfigLocation::WorkingDir {
		// a file in whatever directory the CLI was started in is easy to overlook
		ui.print_muted_to_stderr(&format!(
			"Logged in as {} from the auth config file {}",
			client.email(),
			path.display()
		));
	}
	Ok(Some((client, path)))
}

/// Where an auth config file was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthConfigLocation {
	/// Given with `--auth-config-path`
	Given,
	/// `./filen-cli-auth-config.txt`, in the current working directory
	WorkingDir,
	/// `{config_dir}/filen-cli-auth-config.txt`
	ConfigDir,
}

/// Refuse an autodetected auth config file that users other than its owner can access, and warn
/// about a given one: a file given explicitly is often mounted from elsewhere (a Docker secret is
/// readable by everyone by default), where the CLI cannot be the one to decide.
fn check_auth_config_access(ui: &mut UI, path: &Path, location: AuthConfigLocation) -> Result<()> {
	let open_to_others =
		is_open_to_other_users(path).map_err(|e| auth_config_read_failure(path, &e))?;
	if !open_to_others {
		return Ok(());
	}
	let problem = format!(
		"Other users can access the auth config file {}, which holds your account's keys: \
		 restrict it with `chmod 600 {}`",
		path.display(),
		path.display()
	);
	match location {
		AuthConfigLocation::Given => {
			ui.print_warning(&problem);
			Ok(())
		}
		AuthConfigLocation::WorkingDir | AuthConfigLocation::ConfigDir => {
			Err(UI::failure(&problem))
		}
	}
}

/// Whether the permissions of the file at `path` let users other than its owner access it.
#[cfg(unix)]
fn is_open_to_other_users(path: &Path) -> std::io::Result<bool> {
	Ok(std::fs::metadata(path)?.permissions().mode() & 0o077 != 0)
}

/// Whether the permissions of the file at `path` let users other than its owner access it. Not
/// checked outside Unix, where the file's ACL decides.
#[cfg(not(unix))]
fn is_open_to_other_users(_path: &Path) -> std::io::Result<bool> {
	Ok(false)
}

/// Read the auth config file at `path`.
fn read_auth_config(path: &Path) -> Result<String> {
	std::fs::read_to_string(path).map_err(|e| auth_config_read_failure(path, &e))
}

/// The failure for an auth config file at `path` that could not be read.
fn auth_config_read_failure(path: &Path, e: &std::io::Error) -> anyhow::Error {
	if e.kind() == std::io::ErrorKind::NotFound {
		UI::failure(&format!("Auth config file not found: {}", path.display()))
	} else {
		UI::failure(&format!(
			"Failed to read auth config file {}: {}",
			path.display(),
			e
		))
	}
}

/// Get the default locations for auth config files that exist: inside the current working
/// directory, and the config directory, in the order they are used in.
pub(crate) fn get_auth_config_default_locations(
	config: &CliConfig,
) -> Vec<(PathBuf, AuthConfigLocation)> {
	let mut locations = Vec::new();
	match std::env::current_dir() {
		Ok(current_dir) => locations.push((
			current_dir.join(AUTH_CONFIG_FILENAME),
			AuthConfigLocation::WorkingDir,
		)),
		Err(e) => log::warn!(
			"Failed to get current working directory, so not checking there for an auth config file: {:?}",
			e
		),
	}
	locations.push((
		config.config_dir.join(AUTH_CONFIG_FILENAME),
		AuthConfigLocation::ConfigDir,
	));
	locations
		.into_iter()
		.filter(|(path, _)| path.exists())
		.collect()
}

const KEYRING_SDK_CONFIG_NAME: &str = "sdk-config";

/// Whether the tests turned the keyring off, so that they neither read nor delete the
/// credentials of whoever runs them.
fn keyring_disabled_for_testing() -> bool {
	std::env::var("FILEN_CLI_TESTING_DISABLE_KEYRING") == Ok("1".to_string())
}

/// Authenticate using SDK config stored in the keyring.
async fn authenticate_from_keyring(
	client_config_args: &ClientConfigArgs,
) -> Result<Option<Client>> {
	if keyring_disabled_for_testing() {
		return Ok(None);
	}
	let sdk_config = LongKeyringEntry::new(KEYRING_SDK_CONFIG_NAME)
		.read()
		.context("Failed to read SDK config from keyring")?;
	let Some(sdk_config) = sdk_config else {
		return Ok(None);
	};
	Ok(Some(deserialize_auth_config(
		&sdk_config,
		client_config_args,
	)?))
}

/// Authenticate using credentials provided interactively.
async fn authenticate_from_prompt(
	config: &CliConfig,
	ui: &mut UI,
	client_config_args: &ClientConfigArgs,
) -> Result<Client> {
	ui.ensure_can_prompt("Email:")
		.context("Not logged in: log in with --email and --password, or with --auth-config-path")?;
	let email = ui.prompt("Email:")?;
	let password = ui.prompt_password("Password: ")?;
	let client = login_and_optionally_prompt_two_factor_code(
		ui,
		email.trim(),
		// as typed: a password can start or end with a space, as one given with --password can
		&password,
		None,
		client_config_args,
	)
	.await?;

	// optionally, save credentials
	if ui.prompt_confirm("Keep me logged in?", true)? {
		let sdk_config = serialize_auth_config(&client)?;
		match LongKeyringEntry::new(KEYRING_SDK_CONFIG_NAME).write(&sdk_config) {
			Ok(_) => {
				ui.print_success("Saved credentials");
			}
			Err(_) => {
				ui.print_failure("Failed to save credentials in keyring");
				// a choice of where to keep the credentials, which --yes does not make
				if ui.prompt_yes_no(
					&format!(
						"Instead, export an auth config to {}?",
						config.config_dir.join(AUTH_CONFIG_FILENAME).display()
					),
					false,
				)? {
					export_auth_config(&client, &config.config_dir)
						.context("Failed to export auth config")?;
				}
			}
		}
	}

	Ok(client)
}

const AUTH_CONFIG_FILENAME: &str = "filen-cli-auth-config.txt"; // (!) referenced in module docs

/// Write `contents` to `path`, creating or truncating it with owner-only permissions
/// (mode `0o600` on Unix). The auth config holds the master keys, private key and API key,
/// so it must never be left group- or world-readable for other users on the machine.
fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
	use std::io::Write;

	let mut options = std::fs::OpenOptions::new();
	options.write(true).create(true).truncate(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.mode(0o600);
	}
	let mut file = options.open(path)?;
	// A pre-existing file keeps its previous mode through create+truncate, so tighten it
	// explicitly while it is still empty, before any secret bytes are written.
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
	}
	file.write_all(contents.as_bytes())
}

/// Export an auth config to `{parent_dir}/filen-cli-auth-config.txt`.
pub(crate) fn export_auth_config(client: &Client, parent_dir: &Path) -> Result<PathBuf> {
	let path = parent_dir.join(AUTH_CONFIG_FILENAME);
	let sdk_config = serialize_auth_config(client)?;
	write_private_file(&path, &sdk_config).context(format!(
		"Failed to write auth config to file {}",
		path.display()
	))?;
	Ok(path)
}

/// Log out by deleting stored credentials from the keyring and auth config files, prompting the
/// user to confirm each (`--yes` confirms them all).
pub(crate) fn logout(config: &CliConfig, ui: &mut UI) -> Result<bool> {
	let mut found_any_credentials = false;
	if !keyring_disabled_for_testing()
		&& let Ok(Some(_)) = LongKeyringEntry::new(KEYRING_SDK_CONFIG_NAME).read()
	{
		found_any_credentials = true;
		if ui.prompt_confirm("Delete credentials stored in system keyring?", false)? {
			let deleted = LongKeyringEntry::new(KEYRING_SDK_CONFIG_NAME)
				.delete()
				.context("Failed to delete SDK config from keyring")?;
			if deleted {
				ui.print_success("Credentials deleted from keyring");
			} else {
				ui.print_failure("No credentials found in keyring");
			}
		}
	}
	for (path, _) in get_auth_config_default_locations(config) {
		if path.exists() {
			found_any_credentials = true;
			if ui.prompt_confirm(
				&format!("Delete auth config file at {}?", path.display()),
				false,
			)? {
				std::fs::remove_file(&path).with_context(|| {
					format!("Failed to delete auth config file at {}", path.display())
				})?;
				ui.print_success(&format!("Deleted auth config file at {}", path.display()));
			}
		}
	}
	if found_any_credentials {
		Ok(true)
	} else {
		ui.print_muted("No credentials found in system keyring or auth config default locations");
		Ok(false)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{error::exit_code, ui::test_support::Printed};

	/// An auth config file in a fresh directory, with the permissions `mode`.
	#[cfg(unix)]
	fn auth_config_with_mode(mode: u32) -> (assert_fs::TempDir, PathBuf) {
		let dir = assert_fs::TempDir::new().unwrap();
		let path = dir.path().join(AUTH_CONFIG_FILENAME);
		std::fs::write(&path, "secret-config").unwrap();
		std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
		(dir, path)
	}

	#[cfg(unix)]
	#[test]
	fn a_file_readable_by_the_group_and_others_is_open_to_other_users() {
		let (_dir, path) = auth_config_with_mode(0o644);
		assert!(is_open_to_other_users(&path).unwrap());
	}

	#[cfg(unix)]
	#[test]
	fn a_file_only_its_owner_can_access_is_not_open_to_other_users() {
		let (_dir, path) = auth_config_with_mode(0o600);
		assert!(!is_open_to_other_users(&path).unwrap());
	}

	#[cfg(unix)]
	#[test]
	fn an_autodetected_auth_config_open_to_other_users_is_refused() {
		let (_dir, path) = auth_config_with_mode(0o644);
		for location in [
			AuthConfigLocation::WorkingDir,
			AuthConfigLocation::ConfigDir,
		] {
			let printed = Printed::default();
			let mut ui = printed.ui();
			let err = check_auth_config_access(&mut ui, &path, location).unwrap_err();
			assert_eq!(
				format!("{err:#}"),
				format!(
					"Other users can access the auth config file {}, which holds your account's \
					 keys: restrict it with `chmod 600 {}`",
					path.display(),
					path.display()
				)
			);
			assert_eq!(exit_code(&err), 1);
			assert_eq!(printed.lines_or_empty(), "");
		}
	}

	#[cfg(unix)]
	#[test]
	fn a_given_auth_config_open_to_other_users_is_used_with_a_warning() {
		let (_dir, path) = auth_config_with_mode(0o644);
		let out = Printed::default();
		let err = Printed::default();
		let mut ui = Printed::split_ui(&out, &err);
		check_auth_config_access(&mut ui, &path, AuthConfigLocation::Given).unwrap();
		assert_eq!(out.lines_or_empty(), "");
		assert_eq!(
			console::strip_ansi_codes(&err.lines()),
			format!(
				"⚠ Other users can access the auth config file {}, which holds your account's \
				 keys: restrict it with `chmod 600 {}`",
				path.display(),
				path.display()
			)
		);
	}

	#[cfg(unix)]
	#[test]
	fn an_autodetected_auth_config_only_its_owner_can_access_is_used_silently() {
		let (_dir, path) = auth_config_with_mode(0o600);
		let printed = Printed::default();
		let mut ui = printed.ui();
		check_auth_config_access(&mut ui, &path, AuthConfigLocation::WorkingDir).unwrap();
		assert_eq!(printed.lines_or_empty(), "");
	}

	#[test]
	fn no_credentials_in_the_arguments_are_none() {
		assert!(cli_credentials(None, None, None).unwrap().is_none());
	}

	#[test]
	fn a_two_factor_code_alone_is_ignored() {
		assert!(
			cli_credentials(None, None, Some("123456"))
				.unwrap()
				.is_none()
		);
	}

	#[test]
	fn an_email_without_a_password_fails() {
		let Err(err) = cli_credentials(Some("a@example.com"), None, None) else {
			panic!("an email without a password must fail");
		};
		assert_eq!(format!("{err:#}"), "Password is required");
		assert_eq!(exit_code(&err), 1);
	}

	#[test]
	fn a_password_without_an_email_fails() {
		let Err(err) = cli_credentials(None, Some("secret"), Some("123456")) else {
			panic!("a password without an email must fail");
		};
		assert_eq!(format!("{err:#}"), "Email is required");
	}

	#[test]
	fn an_email_and_a_password_are_the_credentials() {
		let credentials = cli_credentials(Some("a@example.com"), Some("secret"), Some("123456"))
			.unwrap()
			.unwrap();
		assert_eq!(credentials.email, "a@example.com");
		assert_eq!(credentials.password, "secret");
		assert_eq!(credentials.two_factor_code, Some("123456"));
	}

	#[test]
	fn a_missing_auth_config_path_fails_instead_of_falling_back() {
		let config_dir = assert_fs::TempDir::new().unwrap();
		// the autodetected file, which must not be read in its place
		std::fs::write(config_dir.path().join(AUTH_CONFIG_FILENAME), "not a config").unwrap();
		let missing = config_dir.path().join("missing.txt");
		let config = CliConfig {
			config_dir: config_dir.path().to_path_buf(),
		};

		let Err(err) = authenticate_from_auth_config(
			&config,
			&mut Printed::default().ui(),
			Some(missing.to_str().unwrap()),
			&ClientConfigArgs::default(),
		) else {
			panic!("a missing auth config path must fail");
		};

		assert_eq!(
			format!("{err:#}"),
			format!("Auth config file not found: {}", missing.display())
		);
		assert_eq!(exit_code(&err), 1);
	}

	#[test]
	fn write_private_file_writes_contents() {
		let dir = assert_fs::TempDir::new().unwrap();
		let path = dir.path().join(AUTH_CONFIG_FILENAME);
		write_private_file(&path, "secret-config").unwrap();
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret-config");
	}

	#[cfg(unix)]
	#[test]
	fn write_private_file_creates_owner_only() {
		use std::os::unix::fs::PermissionsExt;
		let dir = assert_fs::TempDir::new().unwrap();
		let path = dir.path().join(AUTH_CONFIG_FILENAME);
		write_private_file(&path, "secret-config").unwrap();
		let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
		assert_eq!(
			mode, 0o600,
			"auth config must not be group- or world-readable"
		);
	}

	#[cfg(unix)]
	#[test]
	fn write_private_file_tightens_existing_lax_file() {
		use std::os::unix::fs::PermissionsExt;
		let dir = assert_fs::TempDir::new().unwrap();
		let path = dir.path().join(AUTH_CONFIG_FILENAME);
		// Simulate a config left behind by an older CLI version with lax permissions.
		std::fs::write(&path, "old").unwrap();
		std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
		write_private_file(&path, "new-secret").unwrap();
		let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
		assert_eq!(mode, 0o600);
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "new-secret");
	}
}
