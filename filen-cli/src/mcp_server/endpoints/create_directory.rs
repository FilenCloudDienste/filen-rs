use std::borrow::Cow;

use crate::{mcp_server::endpoints::Endpoint, util::RemotePath};
use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasUUID as _,
		categories::{DirType, NonRootFileType, Normal},
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
	let parent_dir: DirType<'_, Normal> = match client
		.find_item_at_path(&parent.0)
		.await
		.context("Failed to find parent directory")?
	{
		Some(NonRootFileType::Dir(dir)) => DirType::Dir(dir),
		Some(NonRootFileType::Root(root)) => DirType::Root(root),
		Some(_) => return Err(anyhow::anyhow!("Not a directory: {}", parent.0)),
		None => {
			if recursive {
				DirType::Dir(Cow::Owned(
					Box::pin(create_dir_recursive(client, &parent, true)).await?,
				))
			} else {
				return Err(anyhow::anyhow!("No such parent directory: {}", parent.0));
			}
		}
	};
	client
		.create_dir(
			&parent_dir,
			directory.basename().context("Invalid directory name")?,
		)
		.await
		.context("Failed to create directory")
}
