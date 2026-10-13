use crate::{
	rpc_servers::endpoints::{Endpoint, util::find_item_or_err},
	ui::UI,
	ui_utilities::PrintEndpointOutput,
};
use anyhow::{Context, Result};
use filen_sdk_rs::{
	fs::{categories::NonRootFileType, file::traits::HasFileInfo as _},
	io::client_impl::IoSharedClientExt as _,
};

/// Prevent users from accidentally reading large files (over 1 MiB) without confirmation
const CONFIRM_FOR_FILE_SIZE: u64 = 1024 * 1024;

/// Start of the error message for large files; used both to construct and to detect that error
const LARGE_FILE_ERROR: &str = "File is larger than 1 MiB";

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema, Clone)]
pub(crate) enum ReadFileLines {
	/// Print the entire file
	Full,
	/// Only return the first n lines of the file
	Head(usize),
	/// Only return the last n lines of the file
	Tail(usize),
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema, Clone)]
pub(crate) struct ReadFileInput {
	pub file_path: String,
	pub lines: ReadFileLines,
	/// Force reading a large file (over 1 MiB) without confirmation
	pub force_large_file: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ReadFileOutput {
	pub content: String,
}

pub(crate) struct ReadFile;

impl Endpoint for ReadFile {
	type Input = ReadFileInput;
	type Output = ReadFileOutput;

	fn description() -> &'static str {
		"Reads the contents of a text file, optionally only its first or last lines (files larger than 1 MiB need confirmation)"
	}

	async fn handle(
		&self,
		client: &filen_sdk_rs::auth::Client,
		input: Self::Input,
	) -> Result<Self::Output> {
		let file = find_item_or_err(client, &input.file_path, "file").await?;
		let file = match file {
			NonRootFileType::File(file) => file,
			_ => return Err(UI::failure(&format!("Not a file: {}", input.file_path))),
		};
		if file.size() > CONFIRM_FOR_FILE_SIZE && !input.force_large_file {
			return Err(UI::failure(&format!(
				"{} ({} bytes), specify force_large_file to read it",
				LARGE_FILE_ERROR,
				file.size(),
			)));
		}
		let content = client
			.download_file(file.as_ref())
			.await
			.context("Failed to download file")?;
		let content = String::from_utf8_lossy(&content);
		let content = match input.lines {
			ReadFileLines::Head(n) => content.lines().take(n).collect::<Vec<&str>>().join("\n"),
			ReadFileLines::Tail(n) => content
				.lines()
				.rev()
				.take(n)
				.collect::<Vec<&str>>()
				.into_iter()
				.rev()
				.collect::<Vec<&str>>()
				.join("\n"),
			ReadFileLines::Full => content.to_string(),
		};

		Ok(ReadFileOutput { content })
	}
}

pub(crate) async fn handle_large_file_confirmation(
	client: &filen_sdk_rs::auth::Client,
	ui: &mut UI,
	input: ReadFileInput,
) -> Result<Option<ReadFileOutput>> {
	match ReadFile.handle(client, input.clone()).await {
		Ok(output) => Ok(Some(output)),
		Err(e) => {
			if e.to_string().contains(LARGE_FILE_ERROR) {
				if ui.prompt_confirm(
					&format!("{}, do you want to continue?", LARGE_FILE_ERROR),
					false,
				)? {
					ReadFile
						.handle(
							client,
							ReadFileInput {
								force_large_file: true,
								..input
							},
						)
						.await
						.map(Some)
				} else {
					Ok(None)
				}
			} else {
				Err(e)
			}
		}
	}
}

impl PrintEndpointOutput for ReadFileOutput {
	fn print_output(self, ui: &mut crate::ui::UI) {
		ui.print(&self.content);
	}
}
