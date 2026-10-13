use std::borrow::Cow;

use crate::{
	rpc_servers::endpoints::{Endpoint, util::require_dir},
	ui::UI,
	ui_utilities::PrintEndpointOutput,
	util::RemotePath,
};
use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::categories::{DirType, Normal},
	io::RemoteDirectory,
};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct CreateDirectoryInput {
	pub path: String,
	/// Create missing parent directories as needed, like `mkdir -p`
	pub recursive: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct CreateDirectoryOutput {
	pub path: String,
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
			return Err(UI::failure("Cannot create root directory"));
		}
		let _ = create_dir_recursive(client, &path, input.recursive).await?;
		Ok(CreateDirectoryOutput { path: path.into() })
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
		None => {
			return Err(UI::failure(&format!(
				"No such parent directory: {}",
				parent.0
			)));
		}
	};
	client
		.create_dir(
			&parent_dir,
			directory
				.basename()
				.ok_or_else(|| UI::failure("Invalid directory name"))?,
		)
		.await
		.context("Failed to create directory")
}

impl PrintEndpointOutput for CreateDirectoryOutput {
	fn print_output(self, ui: &mut UI) {
		ui.print_success(&format!("Created directory: {}", self.path));
	}
}
