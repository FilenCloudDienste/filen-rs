use crate::{
	mcp_server::endpoints::{Endpoint, public_link_expiration::PublicLinkExpiration},
	util::RemotePath,
};
use anyhow::{Context, Result, anyhow};
use filen_sdk_rs::{
	ErrorKind, auth::Client, connect::PasswordState, fs::categories::NonRootFileType,
};

fn is_subscription_needed(e: &filen_sdk_rs::Error) -> bool {
	e.kind() == ErrorKind::Server && e.server_code().as_deref() == Some("subscription_needed")
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct GetPublicLinkInput {
	path: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetPublicLinkOutput {
	exists: bool,
	uuid: Option<String>,
	expiration: Option<PublicLinkExpiration>,
	has_password: Option<bool>,
	download_enabled: Option<bool>,
}

pub(crate) struct GetPublicLink;

impl Endpoint for GetPublicLink {
	type Input = GetPublicLinkInput;
	type Output = GetPublicLinkOutput;

	fn description() -> &'static str {
		"Gets the public link status for a file or directory, if one exists. Does not return a \
		 shareable URL, only the link's settings (expiration, password, download setting) - this \
		 SDK does not construct link URLs."
	}

	async fn handle(&self, client: &Client, input: Self::Input) -> Result<Self::Output> {
		let path = RemotePath::new(&input.path);

		let Some(item) = client
			.find_item_at_path(&path.0)
			.await
			.context("Failed to find file or directory")?
		else {
			return Err(anyhow!("No such file or directory: {}", path.0));
		};

		match item {
			NonRootFileType::File(file) => {
				let link = client.get_file_link_status(&file).await.map_err(|e| {
					if is_subscription_needed(&e) {
						anyhow!(
							"Public links are not available for this account (subscription required)"
						)
					} else {
						anyhow::Error::new(e).context("Failed to get public link status")
					}
				})?;
				Ok(match link {
					Some(link) => GetPublicLinkOutput {
						exists: true,
						uuid: Some(link.uuid().to_string()),
						expiration: Some(link.expiration().into()),
						has_password: Some(!matches!(link.password(), PasswordState::None)),
						download_enabled: Some(link.downloadable()),
					},
					None => GetPublicLinkOutput {
						exists: false,
						uuid: None,
						expiration: None,
						has_password: None,
						download_enabled: None,
					},
				})
			}
			NonRootFileType::Dir(dir) => {
				let link = client.get_dir_link_rw(&dir).await.map_err(|e| {
					if is_subscription_needed(&e) {
						anyhow!(
							"Public links are not available for this account (subscription required)"
						)
					} else {
						anyhow::Error::new(e).context("Failed to get public link status")
					}
				})?;
				Ok(match link {
					Some(link) => GetPublicLinkOutput {
						exists: true,
						uuid: Some(link.uuid().to_string()),
						expiration: Some(link.expiration().into()),
						has_password: Some(!matches!(link.password(), PasswordState::None)),
						download_enabled: Some(link.download_enabled()),
					},
					None => GetPublicLinkOutput {
						exists: false,
						uuid: None,
						expiration: None,
						has_password: None,
						download_enabled: None,
					},
				})
			}
			NonRootFileType::Root(_) => {
				Err(anyhow!("The root directory cannot have a public link"))
			}
		}
	}
}
