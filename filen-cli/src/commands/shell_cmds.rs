//! Commands about the CLI itself rather than the drive.

use anyhow::Result;
use clap::Subcommand;

use crate::{
	CommandResult,
	commands::CommandContext,
	docs::DocElement,
	updater::{self, ReleaseRequest},
};

#[derive(Debug, Subcommand)]
pub(crate) enum ShellCommands {
	/// Install a release of the CLI in place of this one, also an older one
	Install {
		/// "latest", or the version to install, such as v0.2.9
		#[arg(value_name = "latest|vX.Y.Z")]
		release: ReleaseRequest,
	},
}

/// The commands listed in the docs' usage section.
pub(crate) const DOCS: &[DocElement] = &[DocElement::CommandHelp("install")];

impl ShellCommands {
	/// Whether the update check when the CLI starts is skipped for this command: it works on the
	/// CLI itself and needs no other release announced.
	pub(crate) fn skips_update_check(&self) -> bool {
		match self {
			Self::Install { .. } => true,
		}
	}

	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext { ui, .. } = ctx;
		match self {
			Self::Install { release } => updater::install(ui, &release).await?,
		}
		Ok(CommandResult::default())
	}
}
