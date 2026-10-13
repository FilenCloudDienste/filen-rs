use crate::{
	rpc_servers::endpoints::{Endpoint, util::find_item_or_err},
	ui,
	ui_utilities::PrintEndpointOutput,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use filen_sdk_rs::fs::{
	HasName as _, HasUUID as _,
	categories::{DirType, NonRootFileType, Normal, fs::CategoryFS as _},
	file::traits::HasFileInfo as _,
};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct StatInput {
	pub path: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ItemType {
	File,
	Directory,
	/// The root directory of the drive
	Drive,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StatOutput {
	#[serde(rename = "type")]
	pub item_type: ItemType,
	/// Not set for the drive
	pub name: Option<String>,
	/// Not set for the drive
	pub uuid: Option<String>,
	/// Size in bytes (for directories, the total size of their contents)
	pub size: Option<u64>,
	/// RFC 3339 timestamp; only set for files
	pub modified: Option<DateTime<Utc>>,
	/// RFC 3339 timestamp
	pub created: Option<DateTime<Utc>>,
	/// Number of files contained (recursively); only set for directories and the drive
	pub files: Option<u64>,
	/// Number of directories contained (recursively); only set for directories and the drive
	pub directories: Option<u64>,
	/// Used storage in bytes; only set for the drive
	pub used_storage: Option<u64>,
	/// Total storage in bytes; only set for the drive
	pub total_storage: Option<u64>,
}

pub(crate) struct Stat;

impl Endpoint for Stat {
	type Input = StatInput;
	type Output = StatOutput;

	fn description() -> &'static str {
		"Gets information about a file or directory, like its size and timestamps"
	}

	async fn handle(
		&self,
		client: &filen_sdk_rs::auth::Client,
		input: Self::Input,
	) -> Result<Self::Output> {
		let item = find_item_or_err(client, &input.path, "file or directory").await?;
		match item {
			NonRootFileType::File(file) => Ok(StatOutput {
				item_type: ItemType::File,
				name: Some(
					file.name()
						.map(str::to_string)
						.unwrap_or_else(|| file.uuid().to_string()),
				),
				uuid: Some(file.uuid().to_string()),
				size: Some(file.size()),
				modified: file.last_modified(),
				created: file.created(),
				files: None,
				directories: None,
				used_storage: None,
				total_storage: None,
			}),
			NonRootFileType::Dir(dir) => {
				let size_info = Normal::dir_size(client, &DirType::from(&*dir), ())
					.await
					.context("Failed to get directory size")?;
				Ok(StatOutput {
					item_type: ItemType::Directory,
					name: Some(
						dir.name()
							.map(str::to_string)
							.unwrap_or_else(|| dir.uuid().to_string()),
					),
					uuid: Some(dir.uuid().to_string()),
					size: Some(size_info.size),
					modified: None,
					created: dir.created(),
					files: Some(size_info.files),
					directories: Some(size_info.dirs),
					used_storage: None,
					total_storage: None,
				})
			}
			NonRootFileType::Root(root) => {
				let user_info = client
					.get_user_info()
					.await
					.context("Failed to get user info")?;
				let size_info = Normal::dir_size(client, &DirType::from(&*root), ())
					.await
					.context("Failed to get drive size")?;
				Ok(StatOutput {
					item_type: ItemType::Drive,
					name: None,
					uuid: None,
					size: None,
					modified: None,
					created: None,
					files: Some(size_info.files),
					directories: Some(size_info.dirs),
					used_storage: Some(user_info.storage_used),
					total_storage: Some(user_info.max_storage),
				})
			}
		}
	}
}

impl PrintEndpointOutput for StatOutput {
	fn print_output(self, ui: &mut ui::UI) {
		let table = [
			Some((
				"Type",
				match self.item_type {
					crate::rpc_servers::endpoints::stat::ItemType::File => "File".to_string(),
					crate::rpc_servers::endpoints::stat::ItemType::Directory => {
						"Directory".to_string()
					}
					crate::rpc_servers::endpoints::stat::ItemType::Drive => "Drive".to_string(),
				},
			)),
			self.name.map(|name| ("Name", name)),
			self.uuid.map(|uuid| ("UUID", uuid)),
			self.size.map(|size| ("Size", ui::format_size(size))),
			self.modified
				.map(|modified| ("Modified", ui::format_date(&modified))),
			self.created
				.map(|created| ("Created", ui::format_date(&created))),
			self.files.map(|files| ("Files", files.to_string())),
			self.directories
				.map(|dirs| ("Directories", dirs.to_string())),
			self.used_storage
				.map(|used| ("Used Storage", ui::format_size(used))),
			self.total_storage
				.map(|total| ("Total Storage", ui::format_size(total))),
		]
		.into_iter()
		.flatten()
		.collect::<Vec<_>>();
		ui.print_key_value_table(
			&table
				.iter()
				.map(|(k, v)| (*k, v.as_str()))
				.collect::<Vec<_>>(),
		);
	}
}
