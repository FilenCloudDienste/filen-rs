use std::borrow::Cow;

use crate::{
	mcp_server::endpoints::{Endpoint, support::require_dir},
	util::RemotePath,
};
use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasUUID as _,
		categories::{DirType, Normal},
	},
	io::RemoteDirectory,
};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct CreateDirectoryInput {
	path: String,
	/// Create missing parent directories as needed, like `mkdir -p` (defaults to false)
	recursive: Option<bool>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct CreateDirectoryOutput {
	uuid: String,
	path: String,
}

pub(crate) struct CreateDirectory;

impl Endpoint for CreateDirectory {
	type Input = CreateDirectoryInput;
	type Output = CreateDirectoryOutput;

	fn description() -> &'static str {
		"Creates a directory. Fails if the parent directory doesn't exist, unless `recursive` is set to true."
	}

	async fn handle(&self, client: &Client, input: Self::Input) -> Result<Self::Output> {
		let path = RemotePath::new(&input.path);
		if path.is_root() {
			return Err(anyhow::anyhow!("Cannot create root directory"));
		}
		let dir = create_dir_recursive(client, &path, input.recursive.unwrap_or(false)).await?;
		Ok(CreateDirectoryOutput {
			uuid: dir.uuid().to_string(),
			path: path.0,
		})
	}
}

async fn create_dir_recursive(
	client: &Client,
	directory: &RemotePath,
	recursive: bool,
) -> Result<RemoteDirectory> {
	let parent = directory.parent();
	let existing = client
		.find_item_at_path(&parent.0)
		.await
		.context("Failed to find parent directory")?;
	let parent_dir: DirType<'_, Normal> = match existing {
		Some(item) => require_dir(item, &parent.0)?,
		None if recursive => DirType::Dir(Cow::Owned(
			Box::pin(create_dir_recursive(client, &parent, true)).await?,
		)),
		None => return Err(anyhow::anyhow!("No such parent directory: {}", parent.0)),
	};
	client
		.create_dir(
			&parent_dir,
			directory.basename().context("Invalid directory name")?,
		)
		.await
		.context("Failed to create directory")
}
