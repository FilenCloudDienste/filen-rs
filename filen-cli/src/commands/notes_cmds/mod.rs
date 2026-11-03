//! [cli-doc] notes-overview
//! `notes` lists your notes, most recently edited first: when each was last edited (UTC), its
//! type, its UUID and its title, marked `[archived]` or `[trashed]`. Notes in the trash are left
//! out unless `--all` is given. With `--json`, the notes are printed as an array of objects with
//! `uuid`, `title` (`null` when it cannot be decrypted), `type`, `trashed`, `archived`, `pinned`,
//! `favorited`, `created` and `edited`.
//!
//! The commands that work on one note (`note-cat`, `note-edit`, `note-rm`) take its exact title or
//! its UUID. When several notes have that title, they fail and list their UUIDs, so that you can
//! pass the one you mean.
//!
//! `note-cat <note>` prints a note's content: a checklist as a Markdown task list, a rich text
//! note as its HTML, and any other note as it is.
//!
//! `note-new [title]` creates a note, titled with the current time when no title is given. `--type`
//! sets its type (`text`, the default, `md`, `code`, `rich` or `checklist`). Its content is read
//! from `--content-file <file>` (`-` for stdin), or from stdin when stdin is not a terminal; a
//! checklist's content is a Markdown task list, as `note-edit` takes it. It prints the new note's
//! UUID (with `--json`, the note as `notes --json` shows it).
//!
//! `note-rm <note>` moves a note to the trash. `--permanent` deletes it for good instead, after
//! asking to confirm, which `--yes` answers.

mod checklist;
mod content;
mod edit;
mod export;
mod import;

use std::{
	cmp::Reverse,
	io::{self, IsTerminal as _, Read as _},
	path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use clap::Subcommand;
use clap_complete::engine::{ArgValueCompleter, PathCompleter};
use filen_sdk_rs::{auth::Client, notes::Note};
use filen_types::{api::v3::notes::NoteType, fs::Uuid};
use serde_json::json;

use self::content::{NoteKind, note_preview, readable_content, stored_content, type_name};
use crate::{
	CommandResult,
	auth::LazyClient,
	commands::CommandContext,
	docs::DocElement,
	ui::{self, UI},
};

#[derive(Debug, Subcommand)]
pub(crate) enum NotesCommands {
	/// List your notes, most recently edited first
	Notes {
		/// Include the notes in the trash
		#[arg(long)]
		all: bool,
	},
	/// Print a note's content
	NoteCat {
		/// The note's exact title, or its UUID
		note: String,
	},
	/// Create a note
	NoteNew {
		/// The note's title (default: the current time)
		title: Option<String>,
		/// The note's type
		#[arg(long = "type", value_enum, default_value_t = NoteKind::Text)]
		note_type: NoteKind,
		/// Read the content from this file, or from stdin for "-" (default: stdin when it is not a
		/// terminal, else an empty note)
		#[arg(long, value_name = "FILE", add = ArgValueCompleter::new(PathCompleter::file()))]
		content_file: Option<PathBuf>,
	},
	/// Edit a note's content in an editor ($VISUAL, $EDITOR or vi)
	NoteEdit {
		/// The note's exact title, or its UUID
		note: String,
		/// Replace the content with this file's, or with stdin's for "-", instead of opening an
		/// editor
		#[arg(long, value_name = "FILE", add = ArgValueCompleter::new(PathCompleter::file()))]
		content_file: Option<PathBuf>,
	},
	/// Move a note to the trash, or delete it permanently
	NoteRm {
		/// The note's exact title, or its UUID
		note: String,
		/// Delete the note permanently instead (asks to confirm, which --yes answers)
		#[arg(long)]
		permanent: bool,
	},
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
	/// Import the .md, .txt and .html files of a local directory as notes
	ImportNotes {
		/// Local directory to import the notes from, such as one export-notes wrote
		#[arg(add = ArgValueCompleter::new(PathCompleter::dir()))]
		path: PathBuf,
		/// Also import the notes in its trash/ and archive/ subdirectories, into the trash and the
		/// archive
		#[arg(long)]
		include_trashed: bool,
	},
}

/// The docs' "Notes" section.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::DocFragment("notes-overview"),
	DocElement::CommandHelp("notes"),
	DocElement::CommandHelp("note-cat"),
	DocElement::CommandHelp("note-new"),
	DocElement::DocFragment("notes-edit"),
	DocElement::CommandHelp("note-edit"),
	DocElement::CommandHelp("note-rm"),
	DocElement::DocFragment("notes-export"),
	DocElement::CommandHelp("export-notes"),
	DocElement::DocFragment("notes-import"),
	DocElement::CommandHelp("import-notes"),
];

impl NotesCommands {
	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext { ui, client, .. } = ctx;
		match self {
			Self::Notes { all } => list_notes(ui, client, all).await?,
			Self::NoteCat { note } => print_note(ui, client, &note).await?,
			Self::NoteNew {
				title,
				note_type,
				content_file,
			} => new_note(ui, client, title, note_type.into(), content_file.as_deref()).await?,
			Self::NoteEdit { note, content_file } => {
				edit::edit_note(ui, client, &note, content_file.as_deref()).await?
			}
			Self::NoteRm { note, permanent } => remove_note(ui, client, &note, permanent).await?,
			Self::ExportNotes { path } => export::export_notes(ui, client, path.as_deref()).await?,
			Self::ImportNotes {
				path,
				include_trashed,
			} => import::import_notes(ui, client, &path, include_trashed).await?,
		}
		Ok(CommandResult::default())
	}
}

async fn list_notes(ui: &mut UI, client: &mut LazyClient, include_trashed: bool) -> Result<()> {
	let client = client.get(ui).await?;
	let mut notes = client.list_notes().await.context("Failed to list notes")?;
	notes.retain(|note| include_trashed || !note.trashed());
	notes.sort_by_key(|note| Reverse(note.edited()));

	if ui.json {
		return ui.print_json(serde_json::Value::Array(
			notes.iter().map(note_json).collect(),
		));
	}
	if notes.is_empty() {
		ui.print_muted("No notes");
		return Ok(());
	}
	for note in &notes {
		ui.print(&note_line(note));
	}
	Ok(())
}

/// A note's line in the `notes` listing.
fn note_line(note: &Note) -> String {
	let marker = if note.trashed() {
		" [trashed]"
	} else if note.archived() {
		" [archived]"
	} else {
		""
	};
	format!(
		"{}  {:<9}  {}  {}{marker}",
		ui::format_date(&note.edited()),
		type_name(note.note_type()),
		note.uuid(),
		note.title().unwrap_or("<undecryptable title>"),
	)
}

/// A note as `notes --json` prints it.
fn note_json(note: &Note) -> serde_json::Value {
	json!({
		"uuid": note.uuid(),
		"title": note.title(),
		"type": type_name(note.note_type()),
		"trashed": note.trashed(),
		"archived": note.archived(),
		"pinned": note.pinned(),
		"favorited": note.favorited(),
		"created": note.created(),
		"edited": note.edited(),
	})
}

/// How messages name a note: its title in quotes, or its UUID when the title cannot be decrypted.
fn describe_note(note: &Note) -> String {
	match note.title() {
		Some(title) => format!("\"{title}\""),
		None => note.uuid().to_string(),
	}
}

async fn print_note(ui: &mut UI, client: &mut LazyClient, query: &str) -> Result<()> {
	let client = client.get(ui).await?;
	let mut note = find_note(client, query).await?;
	let stored = fetch_content(client, &mut note).await?;
	let content = readable_content(note.note_type(), &stored);
	if ui.json {
		let mut object = note_json(&note);
		object["content"] = json!(content);
		return ui.print_json(object);
	}
	// printing ends the content with a newline, so one it already ends with is not doubled
	let content: &str = &content;
	let content = content.strip_suffix('\n').unwrap_or(content);
	if !content.is_empty() {
		ui.print(content);
	}
	Ok(())
}

async fn new_note(
	ui: &mut UI,
	client: &mut LazyClient,
	title: Option<String>,
	note_type: NoteType,
	content_file: Option<&Path>,
) -> Result<()> {
	// the content is read before logging in, so that a bad file creates nothing
	let content = match content_file {
		Some(path) => read_content_file(path).await?,
		None if !io::stdin().is_terminal() => read_stdin().await?,
		None => String::new(),
	};
	let client = client.get(ui).await?;
	let note = create_filled_note(client, title, note_type, &content).await?;
	if ui.json {
		return ui.print_json(note_json(&note));
	}
	ui.print_success(&format!(
		"Created note {} ({})",
		describe_note(&note),
		note.uuid()
	));
	Ok(())
}

async fn remove_note(
	ui: &mut UI,
	client: &mut LazyClient,
	query: &str,
	permanent: bool,
) -> Result<()> {
	ensure_can_remove(ui, permanent)?;
	let client = client.get(ui).await?;
	let mut note = find_note(client, query).await?;
	let described = describe_note(&note);
	if permanent {
		if !ui.prompt_confirm(&format!("Permanently delete note {described}?"), false)? {
			return Ok(());
		}
		client
			.delete_note(note)
			.await
			.with_context(|| format!("Failed to delete note {described}"))?;
		ui.print_success(&format!("Permanently deleted note {described}"));
		return Ok(());
	}
	if note.trashed() {
		return Err(UI::failure(&format!(
			"Note {described} is already in the trash (--permanent deletes it)"
		)));
	}
	client
		.trash_note(&mut note)
		.await
		.with_context(|| format!("Failed to move note {described} to the trash"))?;
	ui.print_success(&format!("Moved note {described} to the trash"));
	Ok(())
}

/// Fails when `note-rm` would ask to confirm but nobody could answer, before anything is looked
/// up, so that a script learns at once that it needs `--yes`.
fn ensure_can_remove(ui: &UI, permanent: bool) -> Result<()> {
	if permanent {
		ui.ensure_can_confirm("Permanently delete the note?")?;
	}
	Ok(())
}

/// The note `query` names, by its UUID or its exact title.
async fn find_note(client: &Client, query: &str) -> Result<Note> {
	let mut notes = client.list_notes().await.context("Failed to list notes")?;
	match match_note(notes.iter().map(|note| (*note.uuid(), note.title())), query) {
		NoteMatch::One(index) => Ok(notes.swap_remove(index)),
		NoteMatch::NotFound => Err(UI::failure(&format!(
			"No note is titled \"{query}\" or has that UUID"
		))),
		NoteMatch::Several(uuids) => Err(UI::failure(&format!(
			"{} notes are titled \"{query}\", pass the UUID of the one you mean:\n{}",
			uuids.len(),
			uuids
				.iter()
				.map(|uuid| format!("  {uuid}"))
				.collect::<Vec<_>>()
				.join("\n")
		))),
	}
}

/// Which of the notes a query names.
#[derive(Debug, PartialEq, Eq)]
enum NoteMatch {
	/// The note at this index.
	One(usize),
	NotFound,
	/// Several notes have the title, and none the UUID: these are theirs.
	Several(Vec<Uuid>),
}

/// The note, of `(uuid, title)` pairs, that `query` names: the one with that UUID, else the one
/// with exactly that title. Several with the title are [`NoteMatch::Several`], never one picked
/// out of them.
fn match_note<'a>(
	notes: impl Iterator<Item = (Uuid, Option<&'a str>)> + Clone,
	query: &str,
) -> NoteMatch {
	if let Ok(uuid) = query.parse::<Uuid>()
		&& let Some(index) = notes.clone().position(|(note_uuid, _)| note_uuid == uuid)
	{
		return NoteMatch::One(index);
	}
	let mut titled = notes
		.enumerate()
		.filter(|(_, (_, title))| *title == Some(query));
	match (titled.next(), titled.next()) {
		(None, _) => NoteMatch::NotFound,
		(Some((index, _)), None) => NoteMatch::One(index),
		(Some(first), Some(second)) => NoteMatch::Several(
			[first, second]
				.into_iter()
				.chain(titled)
				.map(|(_, (uuid, _))| uuid)
				.collect(),
		),
	}
}

/// A note's stored content, failing when it cannot be decrypted.
async fn fetch_content(client: &Client, note: &mut Note) -> Result<String> {
	let described = describe_note(note);
	client
		.get_note_content(note)
		.await
		.with_context(|| format!("Failed to fetch note {described}"))?
		.ok_or_else(|| UI::failure(&format!("Failed to decrypt note {described}")))
}

/// Creates a note of `note_type` holding `readable` content (see
/// [`content::readable_content`]). A note that was created but could not be given its type or
/// content is deleted again.
async fn create_filled_note(
	client: &Client,
	title: Option<String>,
	note_type: NoteType,
	readable: &str,
) -> Result<Note> {
	let mut note = client
		.create_note(title)
		.await
		.context("Failed to create the note")?;
	let Err(e) = fill_note(client, &mut note, note_type, readable).await else {
		return Ok(note);
	};
	let uuid = *note.uuid();
	match client.delete_note(note).await {
		Ok(()) => Err(e),
		Err(delete_error) => Err(e.context(format!(
			"Failed to delete the unfinished note {uuid} again: {delete_error}"
		))),
	}
}

async fn fill_note(
	client: &Client,
	note: &mut Note,
	note_type: NoteType,
	readable: &str,
) -> Result<()> {
	if note_type != NoteType::Text {
		client
			.set_note_type(note, note_type, Some(""))
			.await
			.context("Failed to set the note's type")?;
	}
	if !readable.is_empty() {
		let stored = stored_content(note_type, readable);
		let preview = note_preview(note_type, &stored);
		client
			.set_note_content(note, &stored, preview)
			.await
			.context("Failed to set the note's content")?;
	}
	Ok(())
}

/// Reads a note's content from `path`, or from stdin when `path` is `-`.
async fn read_content_file(path: &Path) -> Result<String> {
	if path == Path::new("-") {
		return read_stdin().await;
	}
	std::fs::read_to_string(path)
		.map_err(|e| UI::failure(&format!("Failed to read {}: {e}", path.display())))
}

async fn read_stdin() -> Result<String> {
	tokio::task::spawn_blocking(|| {
		let mut content = String::new();
		io::stdin()
			.read_to_string(&mut content)
			.map(|_| content)
			.map_err(|e| UI::failure(&format!("Failed to read the content from stdin: {e}")))
	})
	.await
	.context("Failed to join the stdin reader")?
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{commands::Commands, error::exit_code, parse_args, ui::test_support::Printed};

	const A: Uuid = Uuid::from_u128(1);
	const B: Uuid = Uuid::from_u128(2);
	const C: Uuid = Uuid::from_u128(3);

	fn match_in(notes: &[(Uuid, Option<&str>)], query: &str) -> NoteMatch {
		match_note(notes.iter().copied(), query)
	}

	#[test]
	fn a_note_is_found_by_its_exact_title() {
		let notes = [(A, Some("Shopping")), (B, Some("Todo")), (C, None)];
		assert_eq!(match_in(&notes, "Todo"), NoteMatch::One(1));
		assert_eq!(match_in(&notes, "todo"), NoteMatch::NotFound);
		assert_eq!(match_in(&notes, "Shop"), NoteMatch::NotFound);
		assert_eq!(match_in(&notes, ""), NoteMatch::NotFound);
	}

	#[test]
	fn a_note_is_found_by_its_uuid() {
		let notes = [(A, Some("Shopping")), (B, None)];
		assert_eq!(match_in(&notes, &B.to_string()), NoteMatch::One(1));
		assert_eq!(match_in(&notes, &B.simple().to_string()), NoteMatch::One(1));
		assert_eq!(match_in(&notes, &C.to_string()), NoteMatch::NotFound);
	}

	#[test]
	fn a_uuid_wins_over_a_note_titled_with_it() {
		let a = A.to_string();
		let notes = [(B, Some(a.as_str())), (A, Some("Shopping"))];
		assert_eq!(match_in(&notes, &a), NoteMatch::One(1));
		// a title that only looks like a UUID still finds its note
		let c = C.to_string();
		let notes = [(B, Some(c.as_str()))];
		assert_eq!(match_in(&notes, &c), NoteMatch::One(0));
	}

	#[test]
	fn an_ambiguous_title_lists_every_note_with_it() {
		let notes = [
			(A, Some("Same")),
			(B, Some("Other")),
			(C, Some("Same")),
			(Uuid::from_u128(4), Some("Same")),
		];
		assert_eq!(
			match_in(&notes, "Same"),
			NoteMatch::Several(vec![A, C, Uuid::from_u128(4)])
		);
	}

	fn parsed(args: &[&str]) -> Result<NotesCommands, clap::error::ErrorKind> {
		let command = std::iter::once("filen").chain(args.iter().copied());
		match parse_args(command).map_err(|e| e.kind())?.args.command {
			Some(Commands::Notes(command)) => Ok(command),
			other => panic!("parsed {other:?}"),
		}
	}

	#[test]
	fn note_new_takes_a_type_and_a_content_file() {
		assert!(matches!(
			parsed(&["note-new"]),
			Ok(NotesCommands::NoteNew {
				title: None,
				note_type: NoteKind::Text,
				content_file: None,
			})
		));
		let Ok(NotesCommands::NoteNew {
			title,
			note_type,
			content_file,
		}) = parsed(&[
			"note-new",
			"Title",
			"--type",
			"checklist",
			"--content-file",
			"-",
		])
		else {
			panic!("not parsed as note-new");
		};
		assert_eq!(title.as_deref(), Some("Title"));
		assert_eq!(note_type, NoteKind::Checklist);
		assert_eq!(content_file.as_deref(), Some(Path::new("-")));
		assert_eq!(
			parsed(&["note-new", "--type", "html"]).err(),
			Some(clap::error::ErrorKind::InvalidValue)
		);
	}

	#[test]
	fn note_rm_takes_permanent_and_the_global_yes_after_it() {
		assert!(matches!(
			parsed(&["note-rm", "a note", "--permanent", "--yes"]),
			Ok(NotesCommands::NoteRm { note, permanent: true }) if note == "a note"
		));
		assert_eq!(
			parsed(&["note-rm"]).err(),
			Some(clap::error::ErrorKind::MissingRequiredArgument)
		);
	}

	#[test]
	fn import_notes_needs_a_directory() {
		assert!(matches!(
			parsed(&["import-notes", "dir", "--include-trashed"]),
			Ok(NotesCommands::ImportNotes {
				include_trashed: true,
				..
			})
		));
		assert_eq!(
			parsed(&["import-notes"]).err(),
			Some(clap::error::ErrorKind::MissingRequiredArgument)
		);
	}

	#[test]
	fn a_permanent_note_rm_without_a_terminal_or_yes_is_refused() {
		let ui = Printed::default().ui();
		let err = ensure_can_remove(&ui, true).unwrap_err();
		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			err.to_string(),
			"Cannot ask \"Permanently delete the note?\": stdin is not a terminal. Pass --yes to \
			 answer yes."
		);
		// moving a note to the trash asks nothing
		ensure_can_remove(&ui, false).unwrap();
	}

	#[test]
	fn yes_lets_a_permanent_note_rm_run_without_a_terminal() {
		let mut ui = Printed::default().ui();
		ui.set_assume_yes(true);
		ensure_can_remove(&ui, true).unwrap();
	}
}
