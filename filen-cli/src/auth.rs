//! [cli-doc] auth-methods
//! There are multiple ways to authenticate:
//! - set the CLI arguments `--email` and `--password` (optionally `--two-factor-code`)  
//!   (when the two-factor code is omitted and required, you will be prompted for it)
//! - specify an auth config via the `--auth-config-path` flag
//!   (exported via `filen export-auth-config`),
//!   or put an auth config in one of the default locations:
//!   (unless overwritten by the `--config-dir` flag)
//!   - `./filen-cli-auth-config.txt` (current working directory)
//!   - Linux/macOS: `$HOME/.filen-cli/filen-cli-auth-config.txt`
//!   - Windows: `%appdata%\filen-cli\filen-cli-auth-config.txt`
//! - set environment variables (`FILEN_CLI_EMAIL` and `FILEN_CLI_PASSWORD`, optionally `FILEN_CLI_2FA_CODE`)  
//!   (when the two-factor code is omitted and required, you will be prompted for it)
//! - if none of these is set, you will be prompted for credentials,
//!   with the option to save them securely in the system keychain

use std::{
	path::{Path, PathBuf},
	sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use filen_cli::{deserialize_auth_config, serialize_auth_config};
use filen_sdk_rs::{ErrorKind, auth::Client};
use filen_types::error::ResponseError;

use crate::{CliConfig, ui::UI, util::LongKeyringEntry};

/// A lazily authenticated client.
/// Since some commands (e. g. logout) don't need the user to be authenticated, we only authenticate when necessary.
pub(crate) enum LazyClient {
	Unauthenticated {
		config: CliConfig,
		email_arg: Option<String>,
		password_arg: Option<String>,
		auth_config_path_arg: Option<String>,
		two_factor_code_arg: Option<String>,
	},
	Authenticated {
		client: Arc<Client>,
	},
}

impl LazyClient {
	pub(crate) fn new(
		config: CliConfig,
		email_arg: Option<String>,
		password_arg: Option<String>,
		two_factor_code_arg: Option<String>,
		auth_config_path_arg: Option<String>,
	) -> Self {
		Self::Unauthenticated {
			config,
			email_arg,
			password_arg,
			two_factor_code_arg,
			auth_config_path_arg,
		}
	}

	pub(crate) async fn get(&mut self, ui: &mut UI) -> Result<&Client> {
		match self {
			Self::Authenticated { client } => Ok(client),
			Self::Unauthenticated {
				config,
				email_arg,
				password_arg,
				auth_config_path_arg,
				two_factor_code_arg,
			} => {
				let client = authenticate_and_get_password(
					config,
					ui,
					email_arg.to_owned(),
					password_arg.as_deref(),
					two_factor_code_arg.as_deref(),
					auth_config_path_arg.as_deref(),
				)
				.await?;
				ui.set_user(Some(client.email()));
				*self = Self::Authenticated {
					client: Arc::new(client),
				};
				let Self::Authenticated { client } = self else {
					unreachable!();
				};
				Ok(client)
			}
		}
	}

	pub(crate) fn get_arc(&self) -> Option<Arc<Client>> {
		match self {
			Self::Authenticated { client } => Some(client.clone()),
			Self::Unauthenticated { .. } => None,
		}
	}
}

/// Authenticate by one of the available authentication methods.
/// Also returns the password (it's needed for Rclone config).
pub(crate) async fn authenticate_and_get_password(
	config: &CliConfig,
	ui: &mut UI,
	email_arg: Option<String>,
	password_arg: Option<&str>,
	two_factor_code_arg: Option<&str>,
	auth_config_path_arg: Option<&str>,
) -> Result<Client> {
	if let Some(client) =
		authenticate_from_cli_args(ui, email_arg, password_arg, two_factor_code_arg).await?
	{
		log::info!("Authenticated from CLI arguments");
		Ok(client)
	} else if let Some((client, export_path)) =
		authenticate_from_auth_config(config, auth_config_path_arg)?
	{
		log::info!(
			"Authenticated from auth config file {}",
			export_path.display()
		);
		Ok(client)
	} else if let Some(client) = authenticate_from_environment_variables(ui).await? {
		log::info!("Authenticated from environment variables");
		Ok(client)
	} else {
		match authenticate_from_keyring().await {
			Ok(Some(client)) => {
				log::info!("Authenticated from keyring");
				Ok(client)
			}
			Ok(None) => authenticate_from_prompt(config, ui).await,
			Err(e) => {
				log::warn!("Failed to authenticate from keyring: {:?}", e);
				authenticate_from_prompt(config, ui).await
			}
		}
	}
}

async fn login_and_optionally_prompt_two_factor_code(
	ui: &mut UI,
	email: String,
	password: &str,
	two_factor_code: Option<&str>,
) -> Result<Client> {
	match Client::login(email.clone(), password, two_factor_code.unwrap_or("XXXXXX")).await {
		Ok(client) => Ok(client),
		Err(e) if e.kind() == ErrorKind::Server => match e.downcast::<ResponseError>() {
			Ok(ResponseError::ApiError { code, .. }) => {
				if code.as_deref() == Some("enter_2fa") {
					let two_factor_code = ui.prompt("Two-factor authentication code: ")?;
					Client::login(email, password, two_factor_code.trim())
						.await
						.context("Failed to log in (with 2fa code)")
				} else if code.as_deref() == Some("email_or_password_wrong") {
					Err(UI::failure("Email or password wrong"))
				} else {
					Err(anyhow::anyhow!(
						"Failed to log in (code {})",
						code.as_deref().unwrap_or("")
					))
				}
			}
			Err(e) => Err(anyhow!(e)).context("Failed to log in"),
		},
		Err(e) => Err(anyhow!(e)).context("Failed to log in"),
	}
}

/// Authenticate using credentials provided in the CLI arguments.
async fn authenticate_from_cli_args(
	ui: &mut UI,
	email_arg: Option<String>,
	password_arg: Option<&str>,
	two_factor_code_arg: Option<&str>,
) -> Result<Option<Client>> {
	if email_arg.is_none() && password_arg.is_none() && two_factor_code_arg.is_none() {
		return Ok(None);
	}
	let client = login_and_optionally_prompt_two_factor_code(
		ui,
		email_arg.context("Email is required")?,
		password_arg.context("Password is required")?,
		two_factor_code_arg,
	)
	.await?;
	Ok(Some(client))
}

/// Authenticate using SDK config stored in a file ("auth config") exported from the CLI.
/// Checks the path provided via CLI argument (if any), or default locations: (! referenced in module docs)
/// - `./filen-cli-auth-config.txt` (current working directory)
/// - `{config_dir}/filen-cli-auth-config.txt`
fn authenticate_from_auth_config(
	config: &CliConfig,
	path_arg: Option<&str>,
) -> Result<Option<(Client, PathBuf)>> {
	let auth_config_paths = if let Some(path) = path_arg {
		vec![PathBuf::from(path)]
	} else {
		find_auth_config_default_locations(config)
	};
	for path in auth_config_paths {
		if path.exists() {
			let sdk_config = std::fs::read_to_string(&path).with_context(|| {
				format!("Failed to read auth config file from {}", path.display())
			})?;
			return Ok(Some((deserialize_auth_config(&sdk_config)?, path)));
		}
	}
	Ok(None)
}

/// Find default locations where auth config files are present.
pub(crate) fn find_auth_config_default_locations(config: &CliConfig) -> Vec<PathBuf> {
	vec![
		std::env::current_dir()
			.context("Failed to get current working directory")
			.unwrap()
			.join(AUTH_CONFIG_FILENAME),
		config.config_dir.join(AUTH_CONFIG_FILENAME),
	]
	.into_iter()
	.filter(|path| path.exists())
	.collect()
}

/// Authenticate from credentials provided via environment variables.
async fn authenticate_from_environment_variables(ui: &mut UI) -> Result<Option<Client>> {
	let email_var = std::env::var("FILEN_CLI_EMAIL").ok();
	let password_var = std::env::var("FILEN_CLI_PASSWORD").ok();
	let two_factor_code_var = std::env::var("FILEN_CLI_2FA_CODE").ok();
	if email_var.is_none() && password_var.is_none() && two_factor_code_var.is_none() {
		return Ok(None);
	}
	let client = login_and_optionally_prompt_two_factor_code(
		ui,
		email_var.context("FILEN_CLI_EMAIL is required")?,
		&password_var.context("FILEN_CLI_PASSWORD is required")?,
		two_factor_code_var.as_deref(),
	)
	.await?;
	Ok(Some(client))
}

const KEYRING_SDK_CONFIG_NAME: &str = "sdk-config";

/// Authenticate using SDK config stored in the keyring.
async fn authenticate_from_keyring() -> Result<Option<Client>> {
	if std::env::var("FILEN_CLI_TESTING_DISABLE_KEYRING") == Ok("1".to_string()) {
		return Ok(None);
	}
	let sdk_config = LongKeyringEntry::new(KEYRING_SDK_CONFIG_NAME)
		.read()
		.context("Failed to read SDK config from keyring")?;
	let Some(sdk_config) = sdk_config else {
		return Ok(None);
	};
	Ok(Some(deserialize_auth_config(&sdk_config)?))
}

/// Authenticate using credentials provided interactively.
async fn authenticate_from_prompt(config: &CliConfig, ui: &mut UI) -> Result<Client> {
	let email = ui.prompt("Email:")?;
	let password = ui.prompt_password("Password: ")?;
	let client = login_and_optionally_prompt_two_factor_code(
		ui,
		email.trim().to_string(),
		password.trim(),
		None,
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
				if ui.prompt_confirm(
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

/// Export an auth config to `{parent_dir}/filen-cli-auth-config.txt`.
pub(crate) fn export_auth_config(client: &Client, parent_dir: &Path) -> Result<PathBuf> {
	let path = parent_dir.join(AUTH_CONFIG_FILENAME);
	let sdk_config = serialize_auth_config(client)?;
	std::fs::write(&path, sdk_config).context(format!(
		"Failed to write auth config to file {}",
		path.display()
	))?;
	Ok(path)
}

/// Log out by deleting stored credentials from the keyring and auth config files, prompting the user for confirmation.
pub(crate) fn logout(config: &CliConfig, ui: &mut UI) -> Result<bool> {
	let mut found_any_credentials = false;
	if let Ok(Some(_)) = LongKeyringEntry::new(KEYRING_SDK_CONFIG_NAME).read() {
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
	for path in find_auth_config_default_locations(config) {
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
	if found_any_credentials {
		Ok(true)
	} else {
		ui.print_muted("No credentials found in system keyring or auth config default locations");
		Ok(false)
	}
}
