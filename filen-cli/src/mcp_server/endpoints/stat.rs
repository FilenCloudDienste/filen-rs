use crate::mcp_server::endpoints::Endpoint;
use anyhow::{Context, Result};
use filen_sdk_rs::fs::{
	HasName as _, HasUUID as _,
	categories::{DirType, NonRootFileType, Normal, fs::CategoryFS as _},
	file::traits::HasFileInfo as _,
};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct StatInput {
	path: String,
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
	item_type: ItemType,
	/// Not set for the drive
	name: Option<String>,
	/// Not set for the drive
	uuid: Option<String>,
	/// Size in bytes (for directories, the total size of their contents)
	size: Option<u64>,
	/// RFC 3339 timestamp; only set for files
	modified: Option<String>,
	/// RFC 3339 timestamp
	created: Option<String>,
	/// Number of files contained (recursively); only set for directories and the drive
	files: Option<u64>,
	/// Number of directories contained (recursively); only set for directories and the drive
	directories: Option<u64>,
	/// Used storage in bytes; only set for the drive
	used_storage: Option<u64>,
	/// Total storage in bytes; only set for the drive
	total_storage: Option<u64>,
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
		let Some(item) = client
			.find_item_at_path(&input.path)
			.await
			.context("Failed to find item")?
		else {
			return Err(anyhow::anyhow!("No such file or directory: {}", input.path));
		};
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
				modified: file.last_modified().map(|d| d.to_rfc3339()),
				created: file.created().map(|d| d.to_rfc3339()),
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
					created: dir.created().map(|d| d.to_rfc3339()),
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
