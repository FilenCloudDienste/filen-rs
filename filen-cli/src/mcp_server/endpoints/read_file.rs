use crate::mcp_server::endpoints::Endpoint;
use anyhow::{Context, Result};
use filen_sdk_rs::{
	fs::{categories::NonRootFileType, file::traits::HasFileInfo as _},
	io::client_impl::IoSharedClientExt as _,
};

/// Files larger than this are not read, so a single call can't pull a huge file into the context
const MAX_FILE_SIZE: u64 = 1024 * 1024;

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ReadFileInput {
	file_path: String,
	/// Only return the first n lines of the file
	head: Option<usize>,
	/// Only return the last n lines of the file
	tail: Option<usize>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ReadFileOutput {
	content: String,
}

pub(crate) struct ReadFile;

impl Endpoint for ReadFile {
	type Input = ReadFileInput;
	type Output = ReadFileOutput;

	fn description() -> &'static str {
		"Reads the contents of a text file, optionally only its first or last lines (files larger than 1 MiB are rejected)"
	}

	async fn handle(
		&self,
		client: &filen_sdk_rs::auth::Client,
		input: Self::Input,
	) -> Result<Self::Output> {
		if input.head.is_some() && input.tail.is_some() {
			return Err(anyhow::anyhow!(
				"Only one of head and tail can be specified"
			));
		}
		let Some(file) = client
			.find_item_at_path(&input.file_path)
			.await
			.context("Failed to find file")?
		else {
			return Err(anyhow::anyhow!("No such file: {}", input.file_path));
		};
		let file = match file {
			NonRootFileType::File(file) => file,
			_ => return Err(anyhow::anyhow!("Not a file: {}", input.file_path)),
		};
		if file.size() > MAX_FILE_SIZE {
			return Err(anyhow::anyhow!(
				"File is too large to read ({} bytes, limit is {} bytes): {}",
				file.size(),
				MAX_FILE_SIZE,
				input.file_path
			));
		}

		let content = client
			.download_file(file.as_ref())
			.await
			.context("Failed to download file")?;
		let content = String::from_utf8_lossy(&content);
		let content = match (input.head, input.tail) {
			(Some(n), _) => content.lines().take(n).collect::<Vec<&str>>().join("\n"),
			(_, Some(n)) => content
				.lines()
				.rev()
				.take(n)
				.collect::<Vec<&str>>()
				.into_iter()
				.rev()
				.collect::<Vec<&str>>()
				.join("\n"),
			(None, None) => content.to_string(),
		};

		Ok(ReadFileOutput { content })
	}
}
