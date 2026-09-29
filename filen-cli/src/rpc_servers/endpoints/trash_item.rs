use crate::{
	rpc_servers::endpoints::{
		Endpoint,
		support::{find_item_or_err, reject_root},
	},
	util::RemotePath,
};
use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{HasUUID as _, categories::NonRootItemType},
};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct TrashItemInput {
	path: String,
	/// Permanently delete instead of moving to trash (defaults to false). This bypasses the
	/// trash entirely and cannot be undone.
	permanent: Option<bool>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct TrashItemOutput {
	uuid: String,
	permanent: bool,
}

pub(crate) struct TrashItem;

impl Endpoint for TrashItem {
	type Input = TrashItemInput;
	type Output = TrashItemOutput;

	fn description() -> &'static str {
		"Moves a file or directory to the trash, or permanently deletes it if `permanent` is set"
	}

	async fn handle(&self, client: &Client, input: Self::Input) -> Result<Self::Output> {
		let path = RemotePath::new(&input.path);
		let permanent = input.permanent.unwrap_or(false);

		let item = find_item_or_err(client, &path.0, "file or directory").await?;
		let item = reject_root(item, "Cannot trash or delete the root directory")?;

		let uuid = match item {
			NonRootItemType::File(file) => {
				let mut file = file.into_owned();
				let uuid = file.uuid().to_string();
				if permanent {
					client
						.delete_file_permanently(file)
						.await
						.context("Failed to permanently delete file")?;
				} else {
					client
						.trash_file(&mut file)
						.await
						.context("Failed to trash file")?;
				}
				uuid
			}
			NonRootItemType::Dir(dir) => {
				let mut dir = dir.into_owned();
				let uuid = dir.uuid().to_string();
				if permanent {
					client
						.delete_dir_permanently(dir)
						.await
						.context("Failed to permanently delete directory")?;
				} else {
					client
						.trash_dir(&mut dir)
						.await
						.context("Failed to trash directory")?;
				}
				uuid
			}
		};

		Ok(TrashItemOutput { uuid, permanent })
	}
}
