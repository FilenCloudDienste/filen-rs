use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as base64};
use filen_sdk_rs::auth::{Client, StringifiedClient};

use crate::{prompt, prompt_confirm, util::LongKeyringEntry};

/// Authenticate by one of the available authentication methods.
pub async fn authenticate(email_arg: Option<String>, password_arg: Option<&str>) -> Result<Client> {
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
	// todo: go over this paragraph again
	match Client::login(email.clone(), password, "XXXXXX").await {
		Ok(client) => Ok(client),
		Err(filen_sdk_rs::error::Error::ErrorWithContext(boxed_error, _))
			if matches!(
				*boxed_error,
				filen_sdk_rs::error::Error::RequestError(
					filen_types::error::ResponseError::ApiError { .. }
				)
			) =>
		{
			let code = if let filen_sdk_rs::error::Error::RequestError(
				filen_types::error::ResponseError::ApiError { code, .. },
			) = *boxed_error
			{
				code
			} else {
				None
			};
			if code.as_deref() == Some("enter_2fa") {
				let two_factor_code = prompt("Two-factor authentication code: ")?;
				let client = Client::login(email, password, two_factor_code.trim())
					.await
					.context("Failed to log in (with 2fa code)")?;
				Ok(client)
			} else {
				Err(anyhow::anyhow!(
					"Failed to log in (code {})",
					code.as_deref().unwrap_or("")
				))
			}
		}
		Err(e) => {
			// Debug: print the actual error to see its structure
			eprintln!("Login error: {:?}", e);
			Err(anyhow::Error::new(e).context("Failed to log in"))
		}
	}
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
	let sdk_config = base64.decode(sdk_config)?;
	let Ok(sdk_config) = serde_json::from_slice::<StringifiedClient>(&sdk_config) else {
		eprintln!("Invalid SDK config in keyring!"); // todo: ?
		return Err(anyhow::anyhow!("Failed to parse SDK config from keyring"));
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

/// Deletes credentials from the keyring.
pub fn delete_credentials() -> Result<()> {
	LongKeyringEntry::new(KEYRING_SDK_CONFIG_NAME)
		.delete()
		.context("Failed to delete SDK config from keyring")?;
	Ok(())
}
