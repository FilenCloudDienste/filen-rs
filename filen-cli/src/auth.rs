use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as base64};
use filen_sdk_rs::{
	ErrorKind,
	auth::{Client, StringifiedClient},
};
use filen_types::error::ResponseError;

use crate::{prompt, prompt_confirm, util::LongKeyringEntry};

/// A lazily authenticated client.
/// Since some commands (e. g. logout) don't need the user to be authenticated, we only authenticate when necessary.
pub(crate) enum LazyClient {
	Unauthenticated {
		email_arg: Option<String>,
		password_arg: Option<String>,
	},
	Authenticated {
		client: Box<Client>,
	},
}

impl LazyClient {
	pub(crate) fn new(email_arg: Option<String>, password_arg: Option<String>) -> Self {
		Self::Unauthenticated {
			email_arg,
			password_arg,
		}
	}

	pub(crate) async fn get(&mut self) -> Result<&Client> {
		match self {
			Self::Authenticated { client } => Ok(client),
			Self::Unauthenticated {
				email_arg,
				password_arg,
			} => {
				let client = authenticate(email_arg.to_owned(), password_arg.as_deref()).await?;
				*self = Self::Authenticated {
					client: Box::new(client),
				};
				let Self::Authenticated { client } = self else {
					unreachable!();
				};
				Ok(client)
			}
		}
	}
}

/// Authenticate by one of the available authentication methods.
pub(crate) async fn authenticate(
	email_arg: Option<String>,
	password_arg: Option<&str>,
) -> Result<Client> {
	if let Ok(client) = authenticate_from_cli_args(email_arg, password_arg).await {
		Ok(client)
	} else if let Ok(client) = authenticate_from_environment_variables().await {
		Ok(client)
	} else if let Ok(client) = authenticate_from_keyring().await {
		Ok(client)
	} else {
		authenticate_from_prompt().await
	}
}

async fn login_and_optionally_prompt_two_factor_code(
	email: String,
	password: &str,
) -> Result<Client> {
	let unhandled_err = match Client::login(email.clone(), password, "XXXXXX").await {
		Ok(client) => return Ok(client),
		Err(e) if e.kind() == ErrorKind::Server => match e.downcast::<ResponseError>() {
			Ok(ResponseError::ApiError { code, .. }) => {
				if code.as_deref() == Some("enter_2fa") {
					let two_factor_code = prompt("Two-factor authentication code: ")?;
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
	email_arg: Option<String>,
	password_arg: Option<&str>,
) -> Result<Client> {
	let email = email_arg.context("Email is required")?;
	let password = password_arg.context("Password is required")?;
	let client = login_and_optionally_prompt_two_factor_code(email, password).await?;
	Ok(client)
}

/// Authenticate from credentials provided via environment variables.
async fn authenticate_from_environment_variables() -> Result<Client> {
	let email = std::env::var("FILEN_CLI_EMAIL")
		.context("FILEN_CLI_EMAIL environment variable is required")?;
	let password = std::env::var("FILEN_CLI_PASSWORD")
		.context("FILEN_CLI_PASSWORD environment variable is required")?;
	let client = login_and_optionally_prompt_two_factor_code(email, &password).await?;
	Ok(client)
}

const KEYRING_SDK_CONFIG_NAME: &str = "sdk-config";

/// Authenticate using SDK config stored in the keyring.
async fn authenticate_from_keyring() -> Result<Client> {
	let sdk_config = LongKeyringEntry::new(KEYRING_SDK_CONFIG_NAME)
		.read()
		.context("Failed to read SDK config from keyring")?;
	let Some(sdk_config) = sdk_config else {
		return Err(anyhow!("No SDK config found in keyring"));
	};
	let sdk_config = base64.decode(sdk_config)?;
	let Ok(sdk_config) = serde_json::from_slice::<StringifiedClient>(&sdk_config) else {
		eprintln!("Invalid SDK config in keyring! Try to `logout`.");
		return Err(anyhow!("Failed to parse SDK config from keyring"));
	};
	let client = Client::from_strings(
		sdk_config.email,
		&sdk_config.root_uuid,
		&sdk_config.auth_info,
		&sdk_config.private_key,
		sdk_config.api_key,
		sdk_config.auth_version,
	)
	.context("Failed to create client from SDK config")?;
	Ok(client)
}

/// Authenticate using credentials provided interactively.
async fn authenticate_from_prompt() -> Result<Client> {
	let email = prompt("Email: ")?;
	let password = prompt("Password: ")?;
	let client =
		login_and_optionally_prompt_two_factor_code(email.trim().to_string(), password.trim())
			.await?;

	// optionally, save credentials
	if prompt_confirm("Keep me logged in?", true)? {
		let sdk_config = client.to_stringified();
		let sdk_config = serde_json::to_string(&sdk_config).unwrap();
		let sdk_config = base64.encode(sdk_config);
		LongKeyringEntry::new(KEYRING_SDK_CONFIG_NAME)
			.write(&sdk_config)
			.context("Failed to write SDK config to keyring")?;
		println!("Saved credentials.");
	}

	Ok(client)
}

/// Deletes credentials from the keyring. Returns true if successful.
pub(crate) fn delete_credentials() -> Result<bool> {
	let deleted = LongKeyringEntry::new(KEYRING_SDK_CONFIG_NAME)
		.delete()
		.context("Failed to delete SDK config from keyring")?;
	Ok(deleted)
}
