use crate::{
	mcp_server::endpoints::{
		Endpoint,
		public_link_expiration::PublicLinkExpiration,
		support::{find_item_or_err, map_link_status_err, reject_root},
	},
	util::RemotePath,
};
use anyhow::{Context, Result};
use filen_sdk_rs::{auth::Client, connect::PasswordState, fs::categories::NonRootItemType};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetPublicLinkInput {
	path: String,
	/// Defaults to "never" if omitted
	expiration: Option<PublicLinkExpiration>,
	/// Omitted or an empty string means no password; a non-empty string sets/replaces the
	/// password. There is no way to leave a previously-set password unchanged - every call
	/// fully replaces the link's settings.
	password: Option<String>,
	/// Whether visitors can download the file/directory through the link (defaults to true)
	download_enabled: Option<bool>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetPublicLinkOutput {
	uuid: String,
	/// Whether a new public link was created (false if an existing one was updated instead)
	created: bool,
	expiration: PublicLinkExpiration,
	has_password: bool,
	download_enabled: bool,
}

pub(crate) struct SetPublicLink;

impl Endpoint for SetPublicLink {
	type Input = SetPublicLinkInput;
	type Output = SetPublicLinkOutput;

	fn description() -> &'static str {
		"Creates a public link for a file or directory, or fully replaces the settings of an \
		 existing one (expiration, password, download setting). Makes the item accessible to \
		 anyone with the link. Does not return a shareable URL, only the link's settings - this \
		 SDK does not construct link URLs."
	}

	async fn handle(&self, client: &Client, input: Self::Input) -> Result<Self::Output> {
		let path = RemotePath::new(&input.path);
		let expiration = input.expiration.unwrap_or(PublicLinkExpiration::Never);
		let download_enabled = input.download_enabled.unwrap_or(true);
		let password = input.password.filter(|p| !p.is_empty());

		let item = find_item_or_err(client, &path.0, "file or directory").await?;
		let item = reject_root(item, "The root directory cannot have a public link")?;

		match item {
			NonRootItemType::File(file) => {
				let existing = client
					.get_file_link_status(&file)
					.await
					.map_err(map_link_status_err)?;
				let created = existing.is_none();
				let mut link = match existing {
					Some(link) => link,
					None => client
						.public_link_file(&file)
						.await
						.context("Failed to create public link")?,
				};

				link.set_expiration(expiration.into());
				match &password {
					Some(password) => link.set_password(password.clone()),
					None => link.clear_password(),
				}
				link.set_downloadable(download_enabled);

				client
					.update_file_link(&file, &link)
					.await
					.context("Failed to save public link settings")?;

				Ok(SetPublicLinkOutput {
					uuid: link.uuid().to_string(),
					created,
					expiration,
					has_password: !matches!(link.password(), PasswordState::None),
					download_enabled: link.downloadable(),
				})
			}
			NonRootItemType::Dir(dir) => {
				let existing = client
					.get_dir_link_rw(&dir)
					.await
					.map_err(map_link_status_err)?;
				let created = existing.is_none();
				let mut link = match existing {
					Some(link) => link,
					None => client
						.public_link_dir(&dir, Some(&(|_, _| {})))
						.await
						.context("Failed to create public link")?,
				};

				link.set_expiration(expiration.into());
				match &password {
					Some(password) => link.set_password(password.clone()),
					None => link.clear_password(),
				}
				link.set_enable_download(download_enabled);

				client
					.update_dir_link(&dir, &link)
					.await
					.context("Failed to save public link settings")?;

				Ok(SetPublicLinkOutput {
					uuid: link.uuid().to_string(),
					created,
					expiration,
					has_password: !matches!(link.password(), PasswordState::None),
					download_enabled: link.download_enabled(),
				})
			}
		}
	}
}
