use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as base64};
use filen_sdk_rs::auth::{Client, StringifiedClient, http::ClientConfig, unauth::UnauthClient};

const AUTH_CONFIG_PREFIX: &str = "filen_cli_auth_config_1:";

pub fn serialize_auth_config(client: &Client) -> Result<String> {
	let sdk_config = client.to_stringified();
	let sdk_config = serde_json::to_string(&sdk_config).unwrap();
	let sdk_config = format!("{}{}", AUTH_CONFIG_PREFIX, base64.encode(sdk_config));
	Ok(sdk_config)
}

pub fn deserialize_auth_config(sdk_config: &str) -> Result<Client> {
	let sdk_config = sdk_config
		.strip_prefix(AUTH_CONFIG_PREFIX)
		.ok_or_else(|| anyhow!("Invalid auth config format (missing or invalid prefix)"))?;
	let sdk_config = base64.decode(sdk_config)?;
	let sdk_config = serde_json::from_slice::<StringifiedClient>(&sdk_config)
		.context("Failed to parse auth config (it may be corrupt)")?;
	let client = UnauthClient::from_config(ClientConfig::default())?;
	let client = client
		.from_stringified(sdk_config)
		.context("Failed to create client from SDK config")?;
	Ok(client)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn test_serialize_deserialize_auth_config() {
		let original_client = test_utils::RESOURCES.client().await;
		let serialized = serialize_auth_config(&original_client).unwrap();
		let deserialized_client = deserialize_auth_config(&serialized).unwrap();
		assert_eq!(original_client.email(), deserialized_client.email());
	}
}
