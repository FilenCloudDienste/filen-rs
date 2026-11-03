//! The CLI's commands. Each domain module owns a clap `Subcommand` enum for its commands,
//! executes them, and exports the doc outline parts that place them in the docs
//! (see [`crate::docs`]). [`Commands`] flattens the domain enums into one command line.

use anyhow::Result;
use clap::Subcommand;

use crate::{CliConfig, CommandResult, auth::LazyClient, ui::UI, util::RemotePath};

pub(crate) mod account_cmds;
pub(crate) mod auth_cmds;
pub(crate) mod fs_cmds;
pub(crate) mod misc_cmds;
pub(crate) mod notes_cmds;
pub(crate) mod public_links_cmds;
pub(crate) mod rclone_cmds;
pub(crate) mod search_cmd;
pub(crate) mod shell_cmds;
pub(crate) mod sync_cmd;
pub(crate) mod transfer_cmds;

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
	#[command(flatten)]
	Misc(misc_cmds::MiscCommands),
	#[command(flatten)]
	Fs(fs_cmds::FsCommands),
	#[command(flatten)]
	Transfer(transfer_cmds::TransferCommands),
	#[command(flatten)]
	Sync(sync_cmd::SyncCommands),
	#[command(flatten)]
	Notes(notes_cmds::NotesCommands),
	#[command(flatten)]
	Search(search_cmd::SearchCommands),
	#[command(flatten)]
	PublicLinks(public_links_cmds::PublicLinksCommands),
	#[command(flatten)]
	Auth(auth_cmds::AuthCommands),
	#[command(flatten)]
	Rclone(rclone_cmds::RcloneCommands),
	#[command(flatten)]
	Shell(shell_cmds::ShellCommands),
	#[command(flatten)]
	Account(account_cmds::AccountCommands),
}

impl Commands {
	/// Whether the update check when the CLI starts is skipped for this command.
	pub(crate) fn skips_update_check(&self) -> bool {
		match self {
			Commands::Shell(command) => command.skips_update_check(),
			// every other command runs after the check
			_ => false,
		}
	}
}
// (!) every command needs to be in its module's doc outline parts (`DOCS`), which
// `docs::DOC_OUTLINE` places into the doc sections

/// What a command runs with.
struct CommandContext<'a> {
	config: &'a CliConfig,
	ui: &'a mut UI,
	client: &'a mut LazyClient,
	working_path: &'a RemotePath,
}

pub(crate) async fn execute_command(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	command: Commands,
) -> Result<CommandResult> {
	let ctx = CommandContext {
		config,
		ui,
		client,
		working_path,
	};
	match command {
		Commands::Misc(command) => command.execute(ctx),
		Commands::Fs(command) => command.execute(ctx).await,
		Commands::Transfer(command) => command.execute(ctx).await,
		Commands::Sync(command) => command.execute(ctx).await,
		Commands::Notes(command) => command.execute(ctx).await,
		Commands::Search(command) => command.execute(ctx).await,
		Commands::PublicLinks(command) => command.execute(ctx).await,
		Commands::Auth(command) => command.execute(ctx).await,
		Commands::Rclone(command) => command.execute(ctx).await,
		Commands::Shell(command) => command.execute(ctx).await,
		Commands::Account(command) => command.execute(ctx).await,
	}
}
