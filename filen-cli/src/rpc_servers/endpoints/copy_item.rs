use crate::{
	rpc_servers::endpoints::{
		Endpoint,
		util::{find_item_or_err, reject_root, require_dir},
	},
	ui::UI,
	ui_utilities::PrintEndpointOutput,
	util::RemotePath,
};
use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName as _, HasUUID as _,
		categories::{DirType, NonRootItemType, Normal},
	},
	io::{HasFileInfo as _, RemoteDirectory, RemoteFile, client_impl::IoSharedClientExt as _},
};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct CopyItemInput {
	pub source_path: String,
	/// The source is copied into the destination directory (recursively, for a
	/// directory) under its current name
	pub destination_parent_dir: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct CopyItemOutput {
	pub source_path: String,
	pub new_path: String,
}

pub(crate) struct CopyItem;

impl Endpoint for CopyItem {
	type Input = CopyItemInput;
	type Output = CopyItemOutput;

	fn description() -> &'static str {
		"Copies a file or directory (recursively) into an existing destination directory, keeping its name"
	}

	async fn handle(&self, client: &Client, input: Self::Input) -> Result<Self::Output> {
		let source_path = RemotePath::new(&input.source_path);
		let destination_path = RemotePath::new(&input.destination_parent_dir);

		let source = find_item_or_err(client, &source_path.0, "source file or directory").await?;
		let source = reject_root(source, "Cannot copy root directory")?;
		let source_name = match &source {
			NonRootItemType::File(file) => file.name(),
			NonRootItemType::Dir(dir) => dir.name(),
		}
		.context("Failed to decrypt source name")?
		.to_string();

		let destination =
			find_item_or_err(client, &destination_path.0, "destination directory").await?;
		let destination_dir = require_dir(destination, &destination_path.0)?;

		let new_path = destination_path.navigate(&source_name);
		if new_path.0.starts_with(&format!("{}/", source_path.0)) {
			return Err(UI::failure(&format!(
				"Cannot copy {} into itself: {}",
				source_path.0, new_path.0
			)));
		}
		if client
			.find_item_at_path(&new_path.0)
			.await
			.context("Failed to check destination")?
			.is_some()
		{
			return Err(UI::failure(&format!(
				"Destination already exists: {}",
				new_path.0
			)));
		}

		match source {
			NonRootItemType::File(file) => {
				copy_file(client, file.as_ref(), &destination_dir)
					.await
					.context("Failed to copy file")?;
			}
			NonRootItemType::Dir(dir) => {
				copy_dir_recursive(client, dir.as_ref(), &destination_dir)
					.await
					.context("Failed to copy directory")?;
			}
		};

		Ok(CopyItemOutput {
			source_path: source_path.0,
			new_path: new_path.0,
		})
	}
}

pub(crate) async fn copy_file(
	client: &Client,
	file: &RemoteFile,
	destination_dir: &DirType<'_, Normal>,
) -> Result<RemoteFile> {
	let name = file.name().context("Failed to decrypt file name")?;
	let mut builder = client
		.make_file_builder(name, destination_dir.uuid())
		.context("Failed to prepare file copy")?;
	if let Some(mime) = file.mime() {
		builder = builder.mime(mime.to_string());
	}
	if let Some(created) = file.created() {
		builder = builder.created(created);
	}
	if let Some(modified) = file.last_modified() {
		builder = builder.modified(modified);
	}
	let data = client
		.download_file(file)
		.await
		.context("Failed to download file for copying")?;
	// todo: does this consume too much memory for large files? maybe we should stream the data
	client
		.upload_file(builder, &data)
		.await
		.context("Failed to upload copied file")
}

pub(crate) async fn copy_dir_recursive(
	client: &Client,
	source_dir: &RemoteDirectory,
	destination_parent: &DirType<'_, Normal>,
) -> Result<RemoteDirectory> {
	let name = source_dir
		.name()
		.context("Failed to decrypt directory name")?;
	let new_dir = client
		.create_dir(destination_parent, name)
		.await
		.context("Failed to create destination directory for copying")?;
	let (subdirs, files) = client
		.list_dir::<_, Normal>(
			&DirType::Dir(std::borrow::Cow::Borrowed(source_dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.context("Failed to list source directory for copying")?;
	for file in &files {
		copy_file(
			client,
			file,
			&DirType::Dir(std::borrow::Cow::Borrowed(&new_dir)),
		)
		.await?;
	}
	for subdir in &subdirs {
		Box::pin(copy_dir_recursive(
			client,
			subdir,
			&DirType::Dir(std::borrow::Cow::Borrowed(&new_dir)),
		))
		.await?;
	}
	Ok(new_dir)
}

impl PrintEndpointOutput for CopyItemOutput {
	fn print_output(self, ui: &mut UI) {
		ui.print_success(&format!("Copied {} to {}", self.source_path, self.new_path));
	}
}
