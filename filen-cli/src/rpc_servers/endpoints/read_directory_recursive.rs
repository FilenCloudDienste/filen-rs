use std::borrow::Cow;

use crate::rpc_servers::endpoints::{
	Endpoint,
	support::{find_item_or_err, require_dir},
};
use anyhow::{Context, Result};
use filen_sdk_rs::fs::categories::{DirType, Normal};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ReadDirectoryRecursiveInput {
	directory_path: String,
	/// How many levels of directories to descend into (1 lists only the directory itself)
	#[schemars(range(min = 1))]
	depth: usize,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ReadDirectoryRecursiveOutput {
	/// Paths relative to the listed directory
	files: Vec<String>,
	/// Paths relative to the listed directory
	directories: Vec<String>,
}

pub(crate) struct ReadDirectoryRecursive;

impl Endpoint for ReadDirectoryRecursive {
	type Input = ReadDirectoryRecursiveInput;
	type Output = ReadDirectoryRecursiveOutput;

	fn description() -> &'static str {
		"Reads the contents of a directory and its subdirectories, up to a given depth"
	}

	async fn handle(
		&self,
		client: &filen_sdk_rs::auth::Client,
		input: Self::Input,
	) -> Result<Self::Output> {
		if input.depth == 0 {
			return Err(anyhow::anyhow!("Depth must be at least 1"));
		}
		let directory = require_dir(
			find_item_or_err(client, &input.directory_path, "directory").await?,
			&input.directory_path,
		)?;

		let mut output = ReadDirectoryRecursiveOutput {
			files: Vec::new(),
			directories: Vec::new(),
		};
		list_recursive(client, &directory, "", input.depth, &mut output).await?;
		Ok(output)
	}
}

async fn list_recursive(
	client: &filen_sdk_rs::auth::Client,
	directory: &DirType<'_, Normal>,
	prefix: &str,
	depth: usize,
	output: &mut ReadDirectoryRecursiveOutput,
) -> Result<()> {
	let (dirs, files) = client
		.list_dir::<_, Normal>(directory, None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list directory")?;

	output.files.extend(
		files
			.into_iter()
			.map(|f| format!("{}{}", prefix, f.meta.name().unwrap_or("<unknown>"))),
	);
	for dir in dirs {
		let path = format!("{}{}", prefix, dir.meta.name().unwrap_or("<unknown>"));
		if depth > 1 {
			Box::pin(list_recursive(
				client,
				&DirType::Dir(Cow::Borrowed(&dir)),
				&format!("{}/", path),
				depth - 1,
				output,
			))
			.await?;
		}
		output.directories.push(path);
	}
	Ok(())
}
