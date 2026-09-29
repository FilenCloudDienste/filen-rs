use crate::mcp_server::endpoints::Endpoint;
use anyhow::{Context, Result};
use filen_sdk_rs::auth::Client;

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct WhoAmIInput {}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WhoAmIOutput {
	email: String,
	user_id: u64,
	is_premium: bool,
	/// Used storage in bytes
	storage_used: u64,
	/// Total storage in bytes
	max_storage: u64,
}

pub(crate) struct WhoAmI;

impl Endpoint for WhoAmI {
	type Input = WhoAmIInput;
	type Output = WhoAmIOutput;

	fn description() -> &'static str {
		"Gets the currently logged-in account's email address and basic account info (storage \
		 used/total, premium status)"
	}

	async fn handle(&self, client: &Client, _input: Self::Input) -> Result<Self::Output> {
		let user_info = client
			.get_user_info()
			.await
			.context("Failed to get user info")?;

		Ok(WhoAmIOutput {
			email: user_info.email,
			user_id: user_info.id,
			is_premium: user_info.is_premium,
			storage_used: user_info.storage_used,
			max_storage: user_info.max_storage,
		})
	}
}
