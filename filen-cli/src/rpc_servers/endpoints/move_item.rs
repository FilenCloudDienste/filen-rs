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
		HasName as _, HasParent as _, HasUUID as _,
		categories::{DirType, NonRootFileType, NonRootItemType},
		dir::meta::DirectoryMetaChanges,
		file::meta::FileMetaChanges,
	},
};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct MoveItemInput {
	pub source_path: String,
	/// Following the semantics of the Unix `mv` command: if this names an existing directory,
	/// the source is moved into it under its current name; otherwise this names the source's
	/// new path (moving it to that path's parent directory and renaming it to that path's base
	/// name)
	pub destination_path: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct MoveItemOutput {
	pub old_path: String,
	pub new_path: String,
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

		let source = find_item_or_err(client, &source_path.0, "source file or directory").await?;
		let source = reject_root(source, "Cannot move root directory")?;
		let source_filename = match &source {
			NonRootItemType::File(file) => file.name(),
			NonRootItemType::Dir(dir) => dir.name(),
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
				return Err(UI::failure(&format!(
					"Destination already exists: {}",
					destination_path.0
				)));
			}
			None => None,
		};
		let (destination_dir, new_name, new_path) = match destination_dir {
			// the destination is an existing directory, so move the source into it as-is
			Some(destination_dir) => {
				let new_path = destination_path.navigate(&source_filename);
				if new_path == source_path {
					return Err(UI::failure(&format!(
						"{} is already in {}",
						source_path.0, destination_path.0
					)));
				}
				// check that the destination doesn't already exist
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
				(destination_dir, source_filename.clone(), new_path)
			}
			// the destination doesn't exist, so it names the source's new path
			None => {
				let new_name = destination_path
					.basename()
					.ok_or_else(|| UI::failure("Destination cannot be the root directory"))?
					.to_string();
				let parent_path = destination_path.parent();
				let destination_dir = require_dir(
					find_item_or_err(client, &parent_path.0, "destination directory").await?,
					&parent_path.0,
				)?;
				(destination_dir, new_name, destination_path.clone())
			}
		};

		if new_path.0.starts_with(&format!("{}/", source_path.0)) {
			return Err(UI::failure(&format!(
				"Cannot move {} into itself: {}",
				source_path.0, new_path.0
			)));
		}

		let needs_rename = new_name != source_filename;
		match source {
			NonRootItemType::File(file) => {
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
							FileMetaChanges::default().name(&new_name).map_err(|e| {
								UI::failure(&format!("Invalid destination file name: {}", e))
							})?,
						)
						.await
						.context("Failed to rename file")?;
				}
			}
			NonRootItemType::Dir(dir) => {
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
								.map_err(|e| {
									UI::failure(&format!(
										"Invalid destination directory name: {}",
										e
									))
								})?,
						)
						.await
						.context("Failed to rename directory")?;
				}
			}
		}

		Ok(MoveItemOutput {
			old_path: source_path.0,
			new_path: new_path.0,
		})
	}
}

impl PrintEndpointOutput for MoveItemOutput {
	fn print_output(self, ui: &mut crate::ui::UI) {
		ui.print_success(&format!("Moved {} to {}", self.old_path, self.new_path));
	}
}
