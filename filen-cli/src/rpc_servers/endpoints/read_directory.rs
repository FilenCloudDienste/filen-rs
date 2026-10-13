use crate::{
	rpc_servers::endpoints::{
		Endpoint,
		util::{find_item_or_err, require_dir},
	},
	ui_utilities::PrintEndpointOutput,
};
use anyhow::{Context, Result};
use filen_sdk_rs::fs::categories::Normal;

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ReadDirectoryInput {
	pub directory_path: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ReadDirectoryOutput {
	pub files_with_size: Vec<(String, u64)>,
	pub directories: Vec<String>,
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
		let directory = require_dir(
			find_item_or_err(client, &input.directory_path, "directory").await?,
			&input.directory_path,
		)?;

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
				(
					f.meta
						.name()
						.map(|str| str.to_string())
						.unwrap_or("<unknown>".into()),
					f.size,
				)
			})
			.collect();

		Ok(ReadDirectoryOutput {
			files_with_size: files,
			directories,
		})
	}
}

impl PrintEndpointOutput for ReadDirectoryOutput {
	fn print_output(self, ui: &mut crate::ui::UI) {
		self.print_output(ui, false);
	}
}

impl ReadDirectoryOutput {
	pub(crate) fn print_output(self, ui: &mut crate::ui::UI, long: bool) {
		ui.print_item_names(
			self.directories,
			self.files_with_size,
			"Directory is empty.",
			long,
		);
	}
}
