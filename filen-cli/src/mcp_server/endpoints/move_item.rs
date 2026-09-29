use crate::{mcp_server::endpoints::Endpoint, util::RemotePath};
use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName as _, HasParent as _, HasUUID as _,
		categories::{DirType, NonRootFileType},
		dir::meta::DirectoryMetaChanges,
		file::meta::FileMetaChanges,
	},
};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct MoveItemInput {
	source_path: String,
	/// Following the semantics of the Unix `mv` command: if this names an existing directory,
	/// the source is moved into it under its current name; otherwise this names the source's
	/// new path (moving it to that path's parent directory and renaming it to that path's base
	/// name)
	destination_path: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct MoveItemOutput {
	path: String,
}

pub(crate) struct MoveItem;

impl Endpoint for MoveItem {
	type Input = MoveItemInput;
	type Output = MoveItemOutput;

	fn description() -> &'static str {
		"Moves and/or renames a file or directory, following the semantics of the Unix `mv` command"
	}

	async fn handle(&self, client: &Client, input: Self::Input) -> Result<Self::Output> {
		let source_path = RemotePath::new(&input.source_path);
		let destination_path = RemotePath::new(&input.destination_path);

		let Some(source) = client
			.find_item_at_path(&source_path.0)
			.await
			.context("Failed to find source file or directory")?
		else {
			return Err(anyhow::anyhow!(
				"No such source file or directory: {}",
				source_path.0
			));
		};
		let source_filename = match &source {
			NonRootFileType::File(file) => file.name(),
			NonRootFileType::Dir(dir) => dir.name(),
			NonRootFileType::Root(_) => return Err(anyhow::anyhow!("Cannot move root directory")),
		}
		.context("Failed to decrypt source name")?
		.to_string();

		// resolve the destination into the directory the source ends up in, plus the name it
		// ends up under
		let destination_dir = match client
			.find_item_at_path(&destination_path.0)
			.await
			.context("Failed to find destination")?
		{
			Some(NonRootFileType::Dir(dir)) => Some(DirType::Dir(dir)),
			Some(NonRootFileType::Root(root)) => Some(DirType::Root(root)),
			Some(NonRootFileType::File(_)) => {
				return Err(anyhow::anyhow!(
					"Destination already exists: {}",
					destination_path.0
				));
			}
			None => None,
		};
		let (destination_dir, new_name, new_path) = match destination_dir {
			// the destination is an existing directory, so move the source into it as-is
			Some(destination_dir) => {
				let new_path = destination_path.navigate(&source_filename);
				if new_path == source_path {
					return Err(anyhow::anyhow!(
						"{} is already in {}",
						source_path.0,
						destination_path.0
					));
				}
				// check that the destination doesn't already exist
				if client
					.find_item_at_path(&new_path.0)
					.await
					.context("Failed to check destination")?
					.is_some()
				{
					return Err(anyhow::anyhow!(
						"Destination already exists: {}",
						new_path.0
					));
				}
				(destination_dir, source_filename.clone(), new_path)
			}
			// the destination doesn't exist, so it names the source's new path
			None => {
				let new_name = destination_path
					.basename()
					.context("Destination cannot be the root directory")?
					.to_string();
				let parent_path = destination_path.parent();
				let destination_dir = match client
					.find_item_at_path(&parent_path.0)
					.await
					.context("Failed to find destination parent directory")?
				{
					Some(NonRootFileType::Dir(dir)) => DirType::Dir(dir),
					Some(NonRootFileType::Root(root)) => DirType::Root(root),
					Some(NonRootFileType::File(_)) => {
						return Err(anyhow::anyhow!("Not a directory: {}", parent_path.0));
					}
					None => {
						return Err(anyhow::anyhow!(
							"No such destination directory: {}",
							parent_path.0
						));
					}
				};
				(destination_dir, new_name, destination_path.clone())
			}
		};

		if new_path.0.starts_with(&format!("{}/", source_path.0)) {
			return Err(anyhow::anyhow!(
				"Cannot move {} into itself: {}",
				source_path.0,
				new_path.0
			));
		}

		let needs_rename = new_name != source_filename;
		match source {
			NonRootFileType::File(file) => {
				let mut file = file.into_owned();
				if *file.parent() != destination_dir.uuid() {
					client
						.move_file(&mut file, &destination_dir)
						.await
						.context("Failed to move file")?;
				}
				if needs_rename {
					client
						.update_file_metadata(
							&mut file,
							FileMetaChanges::default()
								.name(&new_name)
								.context("Invalid destination file name")?,
						)
						.await
						.context("Failed to rename file")?;
				}
			}
			NonRootFileType::Dir(dir) => {
				let mut dir = dir.into_owned();
				if *dir.parent() != destination_dir.uuid() {
					client
						.move_dir(&mut dir, &destination_dir)
						.await
						.context("Failed to move directory")?;
				}
				if needs_rename {
					client
						.update_dir_metadata(
							&mut dir,
							DirectoryMetaChanges::default()
								.name(&new_name)
								.context("Invalid destination directory name")?,
						)
						.await
						.context("Failed to rename directory")?;
				}
			}
			NonRootFileType::Root(_) => return Err(anyhow::anyhow!("Cannot move root directory")),
		}

		Ok(MoveItemOutput { path: new_path.0 })
	}
}
