use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as base64};
use filen_sdk_rs::{
	ErrorKind,
	auth::{Client, StringifiedClient},
};
use filen_types::error::ResponseError;
use serde::{Deserialize, Serialize};

use crate::{ui::UI, util::LongKeyringEntry};

/// A lazily authenticated client.
/// Since some commands (e. g. logout) don't need the user to be authenticated, we only authenticate when necessary.
pub(crate) enum LazyClient {
	Unauthenticated {
		email_arg: Option<String>,
		password_arg: Option<String>,
		auth_config_path_arg: Option<String>,
	},
	Authenticated {
		client: Box<Client>,
		password: String, // todo: can actually remove this field and use the keys that are stored for the Rclone config using the internals fields
	},
}

impl LazyClient {
	pub(crate) fn new(
		email_arg: Option<String>,
		password_arg: Option<String>,
		auth_config_path_arg: Option<String>,
	) -> Self {
		Self::Unauthenticated {
			email_arg,
			password_arg,
			auth_config_path_arg,
		}
	}

	pub(crate) async fn get(&mut self, ui: &mut UI) -> Result<&Client> {
		self.get_with_password(ui).await.map(|(client, _)| client)
	}

	pub(crate) async fn get_with_password(&mut self, ui: &mut UI) -> Result<(&Client, &str)> {
		match self {
			Self::Authenticated { client, password } => Ok((client, password)),
			Self::Unauthenticated {
				email_arg,
				password_arg,
				auth_config_path_arg,
			} => {
				let (client, password) = authenticate_and_get_password(
					ui,
					email_arg.to_owned(),
					password_arg.as_deref(),
					auth_config_path_arg.as_deref(),
				)
				.await?;
				ui.set_user(Some(client.email()));
				*self = Self::Authenticated {
					client: Box::new(client),
					password,
				};
				let Self::Authenticated { client, password } = self else {
					unreachable!();
				};
				Ok((client, password))
			}
		}
	}
}

/// Authenticate by one of the available authentication methods.
/// Also returns the password (it's needed for Rclone config).
pub(crate) async fn authenticate_and_get_password(
	ui: &mut UI,
	email_arg: Option<String>,
	password_arg: Option<&str>,
	auth_config_path_arg: Option<&str>,
) -> Result<(Client, String)> {
	if let Some((client, pwd)) = authenticate_from_cli_args(ui, email_arg, password_arg).await? {
		Ok((client, pwd))
	} else if let Some((client, pwd)) = authenticate_from_auth_config(auth_config_path_arg)? {
		Ok((client, pwd))
	} else if let Some((client, pwd)) = authenticate_from_environment_variables(ui).await? {
		Ok((client, pwd))
	} else if let Some((client, pwd)) = authenticate_from_keyring().await? {
		Ok((client, pwd))
	} else {
		authenticate_from_prompt(ui).await
	}
}

async fn login_and_optionally_prompt_two_factor_code(
	ui: &mut UI,
	email: String,
	password: &str,
) -> Result<Client> {
	let unhandled_err = match Client::login(email.clone(), password, "XXXXXX").await {
		Ok(client) => return Ok(client),
		Err(e) if e.kind() == ErrorKind::Server => match e.downcast::<ResponseError>() {
			Ok(ResponseError::ApiError { code, .. }) => {
				if code.as_deref() == Some("enter_2fa") {
					let two_factor_code = ui.prompt("Two-factor authentication code: ")?;
					let client = Client::login(email, password, two_factor_code.trim())
						.await
						.context("Failed to log in (with 2fa code)")?;
					return Ok(client);
				} else {
					return Err(anyhow::anyhow!(
						"Failed to log in (code {})",
						code.as_deref().unwrap_or("")
					));
				}
			}
			Err(e) => anyhow!(e),
		},
		Err(e) => anyhow!(e),
	};
	eprintln!("Login error: {:?}", unhandled_err);
	Err(unhandled_err.context("Failed to log in"))
}

/// Authenticate using credentials provided in the CLI arguments.
async fn authenticate_from_cli_args(
	ui: &mut UI,
	email_arg: Option<String>,
	password_arg: Option<&str>,
) -> Result<Option<(Client, String)>> {
	if email_arg.is_none() && password_arg.is_none() {
		return Ok(None);
	}
	let email = email_arg.context("Email is required")?;
	let password = password_arg.context("Password is required")?;
	let client = login_and_optionally_prompt_two_factor_code(ui, email, password).await?;
	Ok(Some((client, password.to_string())))
}

/// Authenticate using SDK config stored in a file ("auth config").
fn authenticate_from_auth_config(path_arg: Option<&str>) -> Result<Option<(Client, String)>> {
	let Some(path) = path_arg else {
		return Ok(None);
	};
	let sdk_config = std::fs::read_to_string(path).context("Failed to read auth config file")?;
	Ok(Some(deserialize_auth_config(&sdk_config)?))
}

/// Authenticate from credentials provided via environment variables.
async fn authenticate_from_environment_variables(ui: &mut UI) -> Result<Option<(Client, String)>> {
	let email_var = std::env::var("FILEN_CLI_EMAIL").ok();
	let password_var = std::env::var("FILEN_CLI_PASSWORD").ok();
	if email_var.is_none() && password_var.is_none() {
		return Ok(None);
	}
	let email = email_var.context("FILEN_CLI_EMAIL is required")?;
	let password = password_var.context("FILEN_CLI_PASSWORD is required")?;
	let client = login_and_optionally_prompt_two_factor_code(ui, email, &password).await?;
	Ok(Some((client, password)))
}

const KEYRING_SDK_CONFIG_NAME: &str = "sdk-config";

/// Authenticate using SDK config stored in the keyring.
async fn authenticate_from_keyring() -> Result<Option<(Client, String)>> {
	let sdk_config = LongKeyringEntry::new(KEYRING_SDK_CONFIG_NAME)
		.read()
		.context("Failed to read SDK config from keyring")?;
	let Some(sdk_config) = sdk_config else {
		return Ok(None);
	};
	Ok(Some(deserialize_auth_config(&sdk_config)?))
}

/// Authenticate using credentials provided interactively.
async fn authenticate_from_prompt(ui: &mut UI) -> Result<(Client, String)> {
	let email = ui.prompt("Email:")?;
	let password = ui.prompt_password("Password: ")?;
	let client =
		login_and_optionally_prompt_two_factor_code(ui, email.trim().to_string(), password.trim())
			.await?;

	// optionally, save credentials
	if ui.prompt_confirm("Keep me logged in?", true)? {
		let sdk_config = serialize_auth_config(&client, &password)?;
		LongKeyringEntry::new(KEYRING_SDK_CONFIG_NAME)
			.write(&sdk_config)
			.context("Failed to write SDK config to keyring")?;
		ui.print_success("Saved credentials");
	}

	Ok((client, password))
}

pub(crate) fn export_auth_config(client: &Client, password: &str, path: &PathBuf) -> Result<()> {
	let sdk_config = serialize_auth_config(client, password)?;
	std::fs::write(path, sdk_config).context(format!(
		"Failed to write auth config to file {}",
		path.display()
	))?;
	Ok(())
}

/// Deletes credentials from the keyring. Returns true if successful.
pub(crate) fn delete_credentials() -> Result<bool> {
	let deleted = LongKeyringEntry::new(KEYRING_SDK_CONFIG_NAME)
		.delete()
		.context("Failed to delete SDK config from keyring")?;
	Ok(deleted)
}

#[derive(Serialize, Deserialize)]
struct StringifiedClientWithPassword {
	stringified_client: StringifiedClient,
	password: String,
}

const AUTH_CONFIG_PREFIX: &str = "filen_cli_auth_config_1:";

fn serialize_auth_config(client: &Client, password: &str) -> Result<String> {
	let sdk_config = StringifiedClientWithPassword {
		stringified_client: client.to_stringified(),
		password: password.to_string(),
	};
	let sdk_config = serde_json::to_string(&sdk_config).unwrap();
	let sdk_config = format!("{}{}", AUTH_CONFIG_PREFIX, base64.encode(sdk_config));
	Ok(sdk_config)
}

fn deserialize_auth_config(sdk_config: &str) -> Result<(Client, String)> {
	let sdk_config = sdk_config
		.strip_prefix(AUTH_CONFIG_PREFIX)
		.ok_or_else(|| anyhow!("Invalid auth config format (missing or invalid prefix)"))?;
	let sdk_config = base64.decode(sdk_config)?;
	let sdk_config = serde_json::from_slice::<StringifiedClientWithPassword>(&sdk_config)
		.context("Failed to parse auth config (it may be corrupt)")?;
	let client = Client::from_stringified(sdk_config.stringified_client)
		.context("Failed to create client from SDK config")?;
	Ok((client, sdk_config.password))
}
