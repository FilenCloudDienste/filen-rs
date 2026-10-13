use crate::{
	rpc_servers::endpoints::{
		Endpoint,
		util::{find_item_or_err, reject_root},
	},
	ui_utilities::PrintEndpointOutput,
	util::RemotePath,
};
use anyhow::{Context, Result};
use filen_sdk_rs::{auth::Client, fs::categories::NonRootItemType};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct DeleteItemInput {
	pub path: String,
	/// Permanently delete instead of moving to trash. This bypasses the trash entirely and
	/// cannot be undone.
	pub permanent: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct DeleteItemOutput {
	pub path: String,
	pub item_type: String,
	pub permanent: bool,
}

pub(crate) struct DeleteItem;

impl Endpoint for DeleteItem {
	type Input = DeleteItemInput;
	type Output = DeleteItemOutput;

	fn description() -> &'static str {
		"Moves a file or directory to the trash, or permanently deletes it if `permanent` is set"
	}

	async fn handle(&self, client: &Client, input: Self::Input) -> Result<Self::Output> {
		let path = RemotePath::new(&input.path);

		let item = find_item_or_err(client, &path.0, "file or directory").await?;
		let item = reject_root(item, "Delete the root directory")?;

		let item_type = match item {
			NonRootItemType::File(file) => {
				let mut file = file.into_owned();
				if input.permanent {
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
				"file"
			}
			NonRootItemType::Dir(dir) => {
				let mut dir = dir.into_owned();
				if input.permanent {
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
				"directory"
			}
		};

		Ok(DeleteItemOutput {
			path: input.path,
			item_type: item_type.to_string(),
			permanent: input.permanent,
		})
	}
}

impl PrintEndpointOutput for DeleteItemOutput {
	fn print_output(self, ui: &mut crate::ui::UI) {
		if self.permanent {
			ui.print_success(&format!(
				"Permanently deleted {}: {}",
				self.item_type, self.path,
			));
		} else {
			ui.print_success(&format!("Trashed {}: {}", self.item_type, self.path,));
		}
	}
}
