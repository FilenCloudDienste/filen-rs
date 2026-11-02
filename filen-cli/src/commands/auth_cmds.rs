use anyhow::{Context as _, Result};
use clap::Subcommand;

use crate::{
	CommandResult,
	auth::{self, export_auth_config},
	commands::CommandContext,
	docs::DocElement,
};

#[derive(Debug, Subcommand)]
pub(crate) enum AuthCommands {
	/// Export an auth config (to be used with --auth-config-path option)
	ExportAuthConfig,
	/// Delete saved credentials and exit, asking before each (--yes deletes them all)
	Logout,
}

/// The authentication section of the docs.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::DocFragment("auth-methods"),
	DocElement::CommandHelp("export-auth-config"),
	DocElement::CommandHelp("logout"),
];

impl AuthCommands {
	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext {
			config, ui, client, ..
		} = ctx;
		match self {
			Self::ExportAuthConfig => {
				let client = client.get(ui).await?;
				let export_path = export_auth_config(
					client,
					&std::env::current_dir().context("Failed to get current working directory")?,
				)?;
				ui.print_success(&format!(
					"Exported auth config to {}",
					export_path.display()
				));
			}
			Self::Logout => {
				if auth::logout(config, ui)? {
					return Ok(CommandResult {
						exit: true,
						..Default::default()
					});
				}
			}
		}
		Ok(CommandResult::default())
	}
}
