use crate::{commands::fs_cmds, mcp_server::endpoints::Endpoint, util::RemotePath};
use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName as _, HasUUID as _,
		categories::{DirType, NonRootFileType},
	},
};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct CopyItemInput {
	source_path: String,
	/// Must be an existing directory; the source is copied into it (recursively, for a
	/// directory) under its current name
	destination_path: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct CopyItemOutput {
	uuid: String,
	path: String,
}

pub(crate) struct CopyItem;

impl Endpoint for CopyItem {
	type Input = CopyItemInput;
	type Output = CopyItemOutput;

	fn description() -> &'static str {
		"Copies a file or directory (recursively) into an existing destination directory, keeping its name"
	}

	async fn handle(&self, client: &Client, input: Self::Input) -> Result<Self::Output> {
		let source_path = RemotePath::new(&input.source_path);
		let destination_path = RemotePath::new(&input.destination_path);

		let Some(source) = client
			.find_item_at_path(&source_path.0)
			.await
			.context("Failed to find source file or directory")?
		else {
			return Err(anyhow::anyhow!(
				"No such source file or directory: {}",
				source_path.0
			));
		};
		let source_name = match &source {
			NonRootFileType::File(file) => file.name(),
			NonRootFileType::Dir(dir) => dir.name(),
			NonRootFileType::Root(_) => return Err(anyhow::anyhow!("Cannot copy root directory")),
		}
		.context("Failed to decrypt source name")?
		.to_string();

		let Some(destination) = client
			.find_item_at_path(&destination_path.0)
			.await
			.context("Failed to find destination directory")?
		else {
			return Err(anyhow::anyhow!(
				"No such destination directory: {}",
				destination_path.0
			));
		};
		let destination_dir = match destination {
			NonRootFileType::Dir(dir) => DirType::Dir(dir),
			NonRootFileType::Root(root) => DirType::Root(root),
			NonRootFileType::File(_) => {
				return Err(anyhow::anyhow!("Not a directory: {}", destination_path.0));
			}
		};

		let new_path = destination_path.navigate(&source_name);
		if new_path.0.starts_with(&format!("{}/", source_path.0)) {
			return Err(anyhow::anyhow!(
				"Cannot copy {} into itself: {}",
				source_path.0,
				new_path.0
			));
		}
		if client
			.find_item_at_path(&new_path.0)
			.await
			.context("Failed to check destination")?
			.is_some()
		{
			return Err(anyhow::anyhow!(
				"Destination already exists: {}",
				new_path.0
			));
		}

		let uuid = match source {
			NonRootFileType::File(file) => {
				fs_cmds::copy_file(client, file.as_ref(), &destination_dir)
					.await
					.context("Failed to copy file")?
					.uuid()
					.to_string()
			}
			NonRootFileType::Dir(dir) => {
				fs_cmds::copy_dir_recursive(client, dir.as_ref(), &destination_dir)
					.await
					.context("Failed to copy directory")?
					.uuid()
					.to_string()
			}
			NonRootFileType::Root(_) => unreachable!("checked above"),
		};

		Ok(CopyItemOutput {
			uuid,
			path: new_path.0,
		})
	}
}
