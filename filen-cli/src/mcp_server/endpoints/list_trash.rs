use crate::mcp_server::endpoints::Endpoint;
use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{HasName as _, HasUUID as _, file::traits::HasFileInfo as _},
};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ListTrashInput {}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TrashItemType {
	File,
	Directory,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrashListItem {
	uuid: String,
	name: String,
	#[serde(rename = "type")]
	item_type: TrashItemType,
	/// Size in bytes; only set for files
	size: Option<u64>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ListTrashOutput {
	items: Vec<TrashListItem>,
}

pub(crate) struct ListTrash;

impl Endpoint for ListTrash {
	type Input = ListTrashInput;
	type Output = ListTrashOutput;

	fn description() -> &'static str {
		"Lists the items currently in the trash. Use their `uuid` with RestoreTrashItem to \
		 restore them, or TrashItem with `permanent: true` on their original path to permanently \
		 delete a live item; to permanently delete an already-trashed item, restore it first."
	}

	async fn handle(&self, client: &Client, _input: Self::Input) -> Result<Self::Output> {
		let (dirs, files) = client
			.list_trash(None::<&fn(u64, Option<u64>)>)
			.await
			.context("Failed to list trash")?;

		let mut items = dirs
			.iter()
			.map(|dir| TrashListItem {
				uuid: dir.uuid().to_string(),
				name: dir
					.name()
					.map(str::to_string)
					.unwrap_or_else(|| dir.uuid().to_string()),
				item_type: TrashItemType::Directory,
				size: None,
			})
			.collect::<Vec<_>>();
		items.extend(files.iter().map(|file| {
			TrashListItem {
				uuid: file.uuid().to_string(),
				name: file
					.name()
					.map(str::to_string)
					.unwrap_or_else(|| file.uuid().to_string()),
				item_type: TrashItemType::File,
				size: Some(file.size()),
			}
		}));

		Ok(ListTrashOutput { items })
	}
}
