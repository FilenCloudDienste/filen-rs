use crate::{mcp_server::endpoints::Endpoint, util::RemotePath};
use anyhow::{Context, Result, anyhow};
use filen_sdk_rs::{ErrorKind, auth::Client, fs::categories::NonRootFileType};

fn is_subscription_needed(e: &filen_sdk_rs::Error) -> bool {
	e.kind() == ErrorKind::Server && e.server_code().as_deref() == Some("subscription_needed")
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct DeletePublicLinkInput {
	path: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct DeletePublicLinkOutput {}

pub(crate) struct DeletePublicLink;

impl Endpoint for DeletePublicLink {
	type Input = DeletePublicLinkInput;
	type Output = DeletePublicLinkOutput;

	fn description() -> &'static str {
		"Removes the public link from a file or directory. Fails if it has no active public link."
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
				let Some(link) = link else {
					return Err(anyhow!("No public link exists for: {}", path.0));
				};
				client
					.remove_file_link(&file, link)
					.await
					.context("Failed to delete public link")?;
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
				if link.is_none() {
					return Err(anyhow!("No public link exists for: {}", path.0));
				}
				client
					.remove_dir_link(&dir)
					.await
					.context("Failed to delete public link")?;
			}
			NonRootFileType::Root(_) => {
				return Err(anyhow!("The root directory cannot have a public link"));
			}
		}

		Ok(DeletePublicLinkOutput {})
	}
}
