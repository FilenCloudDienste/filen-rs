use anyhow::{Context as _, Result};
use clap::Subcommand;

use crate::{
	CommandResult,
	commands::CommandContext,
	completion::FilenCompleter,
	docs::{DocElement, print_in_app_docs, serve_markdown_docs_as_html},
};

#[derive(Debug, Subcommand)]
pub(crate) enum MiscCommands {
	/// Print help about a command or topic (default: general help)
	Help {
		/// Command or topic to show help about
		#[arg(add = FilenCompleter::help_topic())]
		command_or_topic: Option<String>,
	},
	/// View the documentation (same as --help) locally in a browser rendered as HTML
	ViewHtmlDocs,
	/// Exit the REPL
	Exit,
}

/// The commands listed in the docs' usage section.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::CommandHelp("help"),
	DocElement::CommandHelp("view-html-docs"),
	DocElement::CommandHelp("exit"),
];

impl MiscCommands {
	pub(super) fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext { ui, .. } = ctx;
		match self {
			Self::Help { command_or_topic } => print_in_app_docs(ui, command_or_topic)?,
			Self::ViewHtmlDocs => {
				serve_markdown_docs_as_html(ui).context("Failed to serve markdown docs as HTML")?;
			}
			Self::Exit => {
				return Ok(CommandResult {
					exit: true,
					..Default::default()
				});
			}
		}
		Ok(CommandResult::default())
	}
}
