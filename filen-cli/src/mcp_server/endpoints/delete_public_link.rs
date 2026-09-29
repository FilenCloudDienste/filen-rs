use crate::{
	mcp_server::endpoints::{
		Endpoint,
		support::{find_item_or_err, map_link_status_err, reject_root},
	},
	util::RemotePath,
};
use anyhow::{Context, Result, anyhow};
use filen_sdk_rs::{auth::Client, fs::categories::NonRootItemType};

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

		let item = find_item_or_err(client, &path.0, "file or directory").await?;
		let item = reject_root(item, "The root directory cannot have a public link")?;

		match item {
			NonRootItemType::File(file) => {
				let link = client
					.get_file_link_status(&file)
					.await
					.map_err(map_link_status_err)?;
				let Some(link) = link else {
					return Err(anyhow!("No public link exists for: {}", path.0));
				};
				client
					.remove_file_link(&file, link)
					.await
					.context("Failed to delete public link")?;
			}
			NonRootItemType::Dir(dir) => {
				let link = client
					.get_dir_link_rw(&dir)
					.await
					.map_err(map_link_status_err)?;
				if link.is_none() {
					return Err(anyhow!("No public link exists for: {}", path.0));
				}
				client
					.remove_dir_link(&dir)
					.await
					.context("Failed to delete public link")?;
			}
		}

		Ok(DeletePublicLinkOutput {})
	}
}
