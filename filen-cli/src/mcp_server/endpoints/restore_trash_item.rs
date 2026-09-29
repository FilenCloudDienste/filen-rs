use std::borrow::Cow;

use crate::mcp_server::endpoints::Endpoint;
use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{HasUUID as _, categories::NonRootItemType},
};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct RestoreTrashItemInput {
	/// The `uuid` of a trashed item, as returned by ListTrash
	uuid: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct RestoreTrashItemOutput {
	/// The absolute path the item was restored to
	path: String,
}

pub(crate) struct RestoreTrashItem;

impl Endpoint for RestoreTrashItem {
	type Input = RestoreTrashItemInput;
	type Output = RestoreTrashItemOutput;

	fn description() -> &'static str {
		"Restores a trashed file or directory (identified by the `uuid` from ListTrash) to its original location"
	}

	async fn handle(&self, client: &Client, input: Self::Input) -> Result<Self::Output> {
		let uuid: filen_types::fs::Uuid = input
			.uuid
			.parse()
			.context("Invalid uuid: must be a valid UUID")?;

		let (dirs, files) = client
			.list_trash(None::<&fn(u64, Option<u64>)>)
			.await
			.context("Failed to list trash")?;

		let path = if let Some(mut dir) = dirs.into_iter().find(|dir| dir.uuid() == uuid) {
			client
				.restore_dir(&mut dir)
				.await
				.context("Failed to restore directory")?;
			let (path, _) = client
				.get_item_path(&NonRootItemType::Dir(Cow::Borrowed(&dir)))
				.await
				.context("Failed to resolve restored path")?;
			format!("/{}", path.trim_end_matches('/'))
		} else if let Some(mut file) = files.into_iter().find(|file| file.uuid() == uuid) {
			client
				.restore_file(&mut file)
				.await
				.context("Failed to restore file")?;
			let (path, _) = client
				.get_item_path(&NonRootItemType::File(Cow::Borrowed(&file)))
				.await
				.context("Failed to resolve restored path")?;
			format!("/{}", path.trim_end_matches('/'))
		} else {
			return Err(anyhow::anyhow!("No trashed item with uuid: {}", input.uuid));
		};

		Ok(RestoreTrashItemOutput { path })
	}
}
