use std::borrow::Cow;

use crate::mcp_server::endpoints::Endpoint;
use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{HasName as _, HasUUID as _, categories::NonRootItemType},
};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ListPublicLinksInput {}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LinkedItemType {
	File,
	Directory,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LinkedItem {
	uuid: String,
	name: String,
	path: String,
	#[serde(rename = "type")]
	item_type: LinkedItemType,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ListPublicLinksOutput {
	items: Vec<LinkedItem>,
}

pub(crate) struct ListPublicLinks;

impl Endpoint for ListPublicLinks {
	type Input = ListPublicLinksInput;
	type Output = ListPublicLinksOutput;

	fn description() -> &'static str {
		"Lists all files and directories that currently have an active public link. Use \
		 GetPublicLink on a path for link details (expiration, password, download setting)."
	}

	async fn handle(&self, client: &Client, _input: Self::Input) -> Result<Self::Output> {
		let (dirs, files) = client
			.list_linked(None::<&fn(u64, Option<u64>)>)
			.await
			.context("Failed to list public links")?;

		let mut items = Vec::with_capacity(dirs.len() + files.len());
		for dir in &dirs {
			let (path, _) = client
				.get_item_path(&NonRootItemType::Dir(Cow::Borrowed(dir)))
				.await
				.context("Failed to resolve path")?;
			items.push(LinkedItem {
				uuid: dir.uuid().to_string(),
				name: dir
					.name()
					.map(str::to_string)
					.unwrap_or_else(|| dir.uuid().to_string()),
				path: format!("/{}", path.trim_end_matches('/')),
				item_type: LinkedItemType::Directory,
			});
		}
		for file in &files {
			let (path, _) = client
				.get_item_path(&NonRootItemType::File(Cow::Borrowed(file)))
				.await
				.context("Failed to resolve path")?;
			items.push(LinkedItem {
				uuid: file.uuid().to_string(),
				name: file
					.name()
					.map(str::to_string)
					.unwrap_or_else(|| file.uuid().to_string()),
				path: format!("/{}", path.trim_end_matches('/')),
				item_type: LinkedItemType::File,
			});
		}

		Ok(ListPublicLinksOutput { items })
	}
}
