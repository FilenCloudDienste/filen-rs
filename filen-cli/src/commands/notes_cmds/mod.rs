//! The notes commands.

mod checklist;
mod export;

use anyhow::Result;
use clap::Subcommand;
use clap_complete::engine::{ArgValueCompleter, PathCompleter};

use crate::{CommandResult, commands::CommandContext, docs::DocElement};

#[derive(Debug, Subcommand)]
pub(crate) enum NotesCommands {
	/// Export all notes to a local directory
	///
	/// A directory that doesn't exist yet is created, and the notes are exported into it, as they
	/// are into an empty directory. A directory that already holds something gets a new
	/// `filen-notes-export-<timestamp>` subdirectory for them. Rich text notes are exported as
	/// HTML, checklists as Markdown, and trashed and archived notes go into `trash` and `archive`
	/// subdirectories.
	ExportNotes {
		/// Local directory to export the notes to (default: the current local directory). Used
		/// directly when it is missing or empty, otherwise the notes go into a new timestamped
		/// subdirectory of it
		#[arg(add = ArgValueCompleter::new(PathCompleter::dir()))]
		path: Option<String>,
	},
}

/// The docs' "Notes" section.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::DocFragment("notes-export"),
	DocElement::CommandHelp("export-notes"),
];

impl NotesCommands {
	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext { ui, client, .. } = ctx;
		match self {
			Self::ExportNotes { path } => export::export_notes(ui, client, path.as_deref()).await?,
		}
		Ok(CommandResult::default())
	}
}
