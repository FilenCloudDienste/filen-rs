use crate::{mcp_server::endpoints::Endpoint, util::RemotePath};
use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasUUID as _,
		categories::{DirType, NonRootFileType, Normal},
	},
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
		let parent_dir: DirType<'_, Normal> = match client
			.find_item_at_path(&parent_path.0)
			.await
			.context("Failed to find parent directory")?
		{
			Some(NonRootFileType::Dir(dir)) => DirType::Dir(dir),
			Some(NonRootFileType::Root(root)) => DirType::Root(root),
			Some(_) => return Err(anyhow::anyhow!("Not a directory: {}", parent_path.0)),
			None => return Err(anyhow::anyhow!("No such directory: {}", parent_path.0)),
		};

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
