use crate::{
	rpc_servers::endpoints::{
		Endpoint,
		public_link_expiration::PublicLinkExpiration,
		util::{find_item_or_err, map_link_status_err, reject_root},
	},
	util::RemotePath,
};
use anyhow::Result;
use filen_sdk_rs::{auth::Client, connect::PasswordState, fs::categories::NonRootItemType};

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct GetPublicLinkInput {
	path: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetPublicLinkOutput {
	exists: bool,
	uuid: Option<String>,
	expiration: Option<PublicLinkExpiration>,
	has_password: Option<bool>,
	download_enabled: Option<bool>,
}

pub(crate) struct GetPublicLink;

impl Endpoint for GetPublicLink {
	type Input = GetPublicLinkInput;
	type Output = GetPublicLinkOutput;

	fn description() -> &'static str {
		"Gets the public link status for a file or directory, if one exists. Does not return a \
		 shareable URL, only the link's settings (expiration, password, download setting) - this \
		 SDK does not construct link URLs."
	}

	async fn handle(&self, client: &Client, input: Self::Input) -> Result<Self::Output> {
		let path = RemotePath::new(&input.path);

		let item = find_item_or_err(client, &path.0, "file or directory").await?;
		let item = reject_root(item, "The root directory cannot have a public link")?;

		match item {
			NonRootItemType::File(file) => {
				let link = client
					.get_file_link_status(&file)
					.await
					.map_err(map_link_status_err)?;
				Ok(match link {
					Some(link) => GetPublicLinkOutput {
						exists: true,
						uuid: Some(link.uuid().to_string()),
						expiration: Some(link.expiration().into()),
						has_password: Some(!matches!(link.password(), PasswordState::None)),
						download_enabled: Some(link.downloadable()),
					},
					None => GetPublicLinkOutput {
						exists: false,
						uuid: None,
						expiration: None,
						has_password: None,
						download_enabled: None,
					},
				})
			}
			NonRootItemType::Dir(dir) => {
				let link = client
					.get_dir_link_rw(&dir)
					.await
					.map_err(map_link_status_err)?;
				Ok(match link {
					Some(link) => GetPublicLinkOutput {
						exists: true,
						uuid: Some(link.uuid().to_string()),
						expiration: Some(link.expiration().into()),
						has_password: Some(!matches!(link.password(), PasswordState::None)),
						download_enabled: Some(link.download_enabled()),
					},
					None => GetPublicLinkOutput {
						exists: false,
						uuid: None,
						expiration: None,
						has_password: None,
						download_enabled: None,
					},
				})
			}
		}
	}
}
