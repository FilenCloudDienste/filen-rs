use crate::{
	rpc_servers::endpoints::{
		Endpoint,
		support::{find_item_or_err, require_dir},
	},
	util::RemotePath,
};
use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{HasUUID as _, categories::NonRootFileType},
};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct WriteFileInput {
	path: String,
	content: String,
	/// Must be set to true to replace a file that already exists at this path (defaults to
	/// false)
	overwrite: Option<bool>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct WriteFileOutput {
	uuid: String,
	path: String,
	size: u64,
}

pub(crate) struct WriteFile;

impl Endpoint for WriteFile {
	type Input = WriteFileInput;
	type Output = WriteFileOutput;

	fn description() -> &'static str {
		"Creates a text file with the given content. Fails if a file already exists at this path, unless `overwrite` is set to true (in which case the old content becomes a previous version of the file)."
	}

	async fn handle(&self, client: &Client, input: Self::Input) -> Result<Self::Output> {
		let path = RemotePath::new(&input.path);
		let name = path
			.basename()
			.context("Cannot write to the root directory")?;
		let parent_path = path.parent();
		let parent_dir = require_dir(
			find_item_or_err(client, &parent_path.0, "directory").await?,
			&parent_path.0,
		)?;

		match client
			.find_item_at_path(&path.0)
			.await
			.context("Failed to check destination")?
		{
			Some(NonRootFileType::Dir(_)) => {
				return Err(anyhow::anyhow!("Path is a directory: {}", path.0));
			}
			Some(NonRootFileType::File(_)) if !input.overwrite.unwrap_or(false) => {
				return Err(anyhow::anyhow!(
					"File already exists: {} (pass overwrite: true to replace it)",
					path.0
				));
			}
			_ => {}
		}

		let builder = client
			.make_file_builder(name, parent_dir.uuid())
			.context("Invalid file name")?;
		let file = client
			.upload_file(builder, input.content.as_bytes())
			.await
			.context("Failed to write file")?;

		Ok(WriteFileOutput {
			uuid: file.uuid().to_string(),
			path: path.0,
			size: input.content.len() as u64,
		})
	}
}
