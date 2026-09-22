use crate::mcp_server::endpoints::Endpoint;
use anyhow::{Context, Result};
use filen_sdk_rs::fs::categories::{DirType, NonRootFileType, Normal};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ReadDirectoryInput {
	directory_path: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ReadDirectoryOutput {
	files: Vec<String>,
	directories: Vec<String>,
}

pub(crate) struct ReadDirectory;

impl Endpoint for ReadDirectory {
	type Input = ReadDirectoryInput;
	type Output = ReadDirectoryOutput;

	fn description() -> &'static str {
		"Reads the contents of a directory"
	}

	async fn handle(
		&self,
		client: &filen_sdk_rs::auth::Client,
		input: Self::Input,
	) -> Result<Self::Output> {
		let Some(directory) = client
			.find_item_at_path(&input.directory_path)
			.await
			.context("Failed to find parent directory")?
		else {
			return Err(anyhow::anyhow!(
				"No such directory: {}",
				input.directory_path
			));
		};
		let directory: DirType<'_, Normal> = match directory {
			NonRootFileType::Dir(dir) => DirType::Dir(dir),
			NonRootFileType::Root(root) => DirType::Root(root),
			_ => return Err(anyhow::anyhow!("Not a directory: {}", input.directory_path)),
		};

		let (dirs, files) = client
			.list_dir::<_, Normal>(&directory, None::<&fn(u64, Option<u64>)>)
			.await
			.context("Failed to list directory")?;

		let directories = dirs
			.into_iter()
			.map(|d| {
				d.meta
					.name()
					.map(|str| str.to_string())
					.unwrap_or("<unknown>".into())
			})
			.collect();
		let files = files
			.into_iter()
			.map(|f| {
				f.meta
					.name()
					.map(|str| str.to_string())
					.unwrap_or("<unknown>".into())
			})
			.collect();

		Ok(ReadDirectoryOutput { files, directories })
	}
}
