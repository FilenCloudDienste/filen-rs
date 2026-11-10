//! [cli-doc] notes-edit
//! `note-edit <note>` opens the note's content in an editor and saves it when the editor exits:
//! the editor `$VISUAL` names, else the one `$EDITOR` names, else `vi` (`notepad` on Windows). The
//! variable may hold arguments too, such as `code --wait`. A checklist is edited as a Markdown
//! task list (`- [ ] item`, `- [x] done`), and every other line of it becomes an unchecked item. A
//! rich text note is edited as its HTML. When the content is unchanged, nothing is saved.
//!
//! `--content-file <file>` replaces the content with the file's instead, without an editor, and
//! `--content-file -` with what is read from stdin. Without a terminal, or with `--no-input`,
//! `note-edit` needs one of them.
//!
//! Before saving, `note-edit` reads the note again. When it changed while it was being edited, the
//! edit is not saved: the error names the temporary file the editor's edit is kept in, or the
//! content file that was not saved. A change made between that check and the save is still
//! overwritten. When the edit cannot be saved for another reason, such as the note having been
//! deleted meanwhile or the network failing, the editor's edit is kept in its temporary file too,
//! and the error names it.

use std::path::Path;

use anyhow::{Context as _, Result};
use filen_sdk_rs::{auth::Client, notes::Note};
use tempfile::TempPath;

use super::{
	content::{file_extension, note_preview, readable_content, stored_content},
	describe_note, fetch_content, find_note, read_content_file,
};
use crate::{auth::LazyClient, command_line::split_command_line, ui::UI};

/// What `note-edit` would ask when it opens an editor, which fails when nobody could use one.
const OPEN_EDITOR: &str = "Open the note in an editor?";

/// The editor run when neither `$VISUAL` nor `$EDITOR` names one.
const DEFAULT_EDITOR: &str = if cfg!(windows) { "notepad" } else { "vi" };

/// Where `note-edit` takes the new content from.
#[derive(Debug, PartialEq, Eq)]
enum EditSource<'a> {
	/// A file, or stdin for `-`.
	File(&'a Path),
	Editor(EditorCommand),
}

pub(super) async fn edit_note(
	ui: &mut UI,
	client: &mut LazyClient,
	query: &str,
	content_file: Option<&Path>,
) -> Result<()> {
	// before logging in, so that a script learns at once what it has to pass
	let source = edit_source(ui, content_file, |name| std::env::var(name).ok())?;

	let client = client.get(ui).await?;
	let mut note = find_note(client, query).await?;
	let stored = fetch_content(client, &mut note).await?;
	let note_type = note.note_type();
	let current = readable_content(note_type, &stored);

	let (edited, edited_in) = match source {
		EditSource::File(path) => (read_content_file(path).await?, EditedIn::ContentFile(path)),
		EditSource::Editor(editor) => {
			let (edited, temp_file) =
				edit_in_editor(&editor, &current, file_extension(note_type)).await?;
			(
				without_added_final_newline(edited, &current),
				EditedIn::TempFile(temp_file),
			)
		}
	};
	if edited == current {
		edited_in.close()?;
		ui.print_success(&format!("Note {} is unchanged", describe_note(&note)));
		return Ok(());
	}

	match save_unless_changed(client, &note, &stored, &edited).await {
		Ok(Save::Saved(latest)) => {
			edited_in.close()?;
			ui.print_success(&format!("Saved note {}", describe_note(&latest)));
			Ok(())
		}
		Ok(Save::ChangedMeanwhile(latest)) => {
			let described = describe_note(&latest);
			Err(match edited_in {
				EditedIn::ContentFile(path) => {
					changed_meanwhile(&described, UnsavedEdit::ContentFile(path))
				}
				EditedIn::TempFile(temp_file) => {
					let kept = temp_file.keep().with_context(|| {
						format!(
							"Note {described} changed while it was being edited, and the \
							 temporary file holding the edit could not be kept"
						)
					})?;
					changed_meanwhile(&described, UnsavedEdit::TempFile(&kept))
				}
			})
		}
		Err(e) => Err(keep_unsaved_edit(e, edited_in)),
	}
}

/// What [`save_unless_changed`] did.
enum Save {
	/// The edit was saved to the note, as it is now.
	Saved(Note),
	/// The note, as it is now, changed while it was being edited, so nothing was saved.
	ChangedMeanwhile(Note),
}

/// Saves `edited` to `note` unless its stored content is no longer `stored`, the content the edit
/// started from.
async fn save_unless_changed(
	client: &Client,
	note: &Note,
	stored: &str,
	edited: &str,
) -> Result<Save> {
	// Without a compare-and-set on the server, a change made after this check is still
	// overwritten.
	let mut latest = find_note(client, &note.uuid().to_string()).await?;
	let latest_stored = fetch_content(client, &mut latest).await?;
	if latest_stored != stored {
		return Ok(Save::ChangedMeanwhile(latest));
	}

	let note_type = note.note_type();
	let new_stored = stored_content(note_type, edited);
	let preview = note_preview(note_type, &new_stored);
	client
		.set_note_content(&mut latest, &new_stored, preview)
		.await
		.with_context(|| format!("Failed to save note {}", describe_note(&latest)))?;
	Ok(Save::Saved(latest))
}

/// `e`, the failure of saving an edit made in `edited_in`, which keeps the editor's temporary file
/// and names it, so that the edit is not lost.
fn keep_unsaved_edit(e: anyhow::Error, edited_in: EditedIn<'_>) -> anyhow::Error {
	match edited_in {
		EditedIn::ContentFile(_) => e,
		EditedIn::TempFile(temp_file) => match temp_file.keep() {
			Ok(kept) => e.context(format!(
				"The edit was not saved; it is in {}",
				kept.display()
			)),
			Err(keep_err) => e.context(format!(
				"The edit was not saved, and the temporary file holding it could not be kept: \
				 {keep_err}"
			)),
		},
	}
}

/// Where the edited content of a note came from.
enum EditedIn<'a> {
	/// A content file, or stdin for `-`.
	ContentFile(&'a Path),
	/// The editor's temporary file, removed when dropped.
	TempFile(TempPath),
}

impl EditedIn<'_> {
	/// Removes the editor's temporary file, failing when it cannot be removed.
	fn close(self) -> Result<()> {
		match self {
			Self::ContentFile(_) => Ok(()),
			Self::TempFile(temp_file) => temp_file
				.close()
				.context("Failed to remove the note's temporary file"),
		}
	}
}

/// An edit `note-edit` did not save because the note changed meanwhile.
#[derive(Debug, Clone, Copy)]
enum UnsavedEdit<'a> {
	/// The content file, or stdin for `-`.
	ContentFile(&'a Path),
	/// The editor's temporary file, which was kept.
	TempFile(&'a Path),
}

/// The failure of an edit not saved because the note `described` names changed while it was
/// being edited.
fn changed_meanwhile(described: &str, unsaved: UnsavedEdit<'_>) -> anyhow::Error {
	UI::failure(&match unsaved {
		UnsavedEdit::TempFile(path) => format!(
			"Note {described} changed while it was being edited, so the edit was not saved; it is \
			 in {}",
			path.display()
		),
		UnsavedEdit::ContentFile(path) if path == Path::new("-") => format!(
			"Note {described} changed while it was being read, so the content read from stdin \
			 was not saved"
		),
		UnsavedEdit::ContentFile(path) => format!(
			"Note {described} changed while it was being read, so the content of {} was not saved",
			path.display()
		),
	})
}

/// The content file when one is given, else the editor, which needs a terminal: without one, or
/// with `--no-input`, it fails as a usage error. `var` reads an environment variable.
fn edit_source<'a>(
	ui: &UI,
	content_file: Option<&'a Path>,
	var: impl Fn(&str) -> Option<String>,
) -> Result<EditSource<'a>> {
	if let Some(path) = content_file {
		return Ok(EditSource::File(path));
	}
	ui.ensure_can_prompt(OPEN_EDITOR).map_err(|e| {
		UI::usage_failure(&format!(
			"{e} Pass --content-file <file>, or --content-file - to read the content from stdin."
		))
	})?;
	Ok(EditSource::Editor(editor_command(var)?))
}

/// The command line of an editor: the program and the arguments before the file's path.
#[derive(Debug, PartialEq, Eq)]
struct EditorCommand {
	program: String,
	args: Vec<String>,
}

/// The editor `$VISUAL` names, else the one `$EDITOR` names, else [`DEFAULT_EDITOR`]. `var` reads
/// an environment variable; an empty one counts as unset.
fn editor_command(var: impl Fn(&str) -> Option<String>) -> Result<EditorCommand> {
	let (source, line) = ["VISUAL", "EDITOR"]
		.into_iter()
		.find_map(|name| {
			var(name)
				.filter(|value| !value.trim().is_empty())
				.map(|value| (name, value))
		})
		.unwrap_or(("", DEFAULT_EDITOR.to_owned()));
	let mut words = split_command_line(&line)
		.ok_or_else(|| UI::failure(&format!("Cannot read the editor command ${source}: {line}")))?
		.into_iter();
	let program = words
		.next()
		.ok_or_else(|| UI::failure(&format!("Cannot read the editor command ${source}: {line}")))?;
	Ok(EditorCommand {
		program,
		args: words.collect(),
	})
}

/// Runs `editor` on a temporary file holding `content`, named with `extension` so that the editor
/// knows the format, and returns what the file holds once the editor exits, with the file, which
/// is removed when dropped.
async fn edit_in_editor(
	editor: &EditorCommand,
	content: &str,
	extension: &str,
) -> Result<(String, TempPath)> {
	let file = tempfile::Builder::new()
		.prefix("filen-note-")
		.suffix(&format!(".{extension}"))
		.tempfile()
		.context("Failed to create a temporary file for the note")?;
	std::fs::write(file.path(), content).with_context(|| {
		format!(
			"Failed to write the note to the temporary file {}",
			file.path().display()
		)
	})?;
	// Closes the file, which an editor on Windows could not open while it is held, and removes
	// it when dropped.
	let path = file.into_temp_path();

	let status = tokio::process::Command::new(&editor.program)
		.args(&editor.args)
		.arg(&path)
		.status()
		.await
		.map_err(|e| UI::failure(&format!("Failed to run the editor {}: {e}", editor.program)))?;
	if !status.success() {
		return Err(UI::failure(&format!(
			"The editor {} exited with {status}; the note was not changed",
			editor.program
		)));
	}
	let edited = std::fs::read_to_string(&path)
		.with_context(|| format!("Failed to read the edited note from {}", path.display()))?;
	Ok((edited, path))
}

/// `edited` without the final newline an editor adds on saving, when `original` did not end with
/// one: that newline is not an edit.
fn without_added_final_newline(mut edited: String, original: &str) -> String {
	if original.ends_with('\n') {
		return edited;
	}
	if edited.ends_with("\r\n") {
		edited.truncate(edited.len() - 2);
	} else if edited.ends_with('\n') {
		edited.truncate(edited.len() - 1);
	}
	edited
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use super::*;
	use crate::{error::exit_code, ui::test_support::Printed};

	fn editor_from(vars: &[(&str, &str)]) -> Result<EditorCommand> {
		let vars = vars
			.iter()
			.map(|(name, value)| (name.to_string(), value.to_string()))
			.collect::<HashMap<_, _>>();
		editor_command(|name| vars.get(name).cloned())
	}

	fn editor(program: &str, args: &[&str]) -> EditorCommand {
		EditorCommand {
			program: program.to_owned(),
			args: args.iter().map(|arg| arg.to_string()).collect(),
		}
	}

	#[test]
	fn visual_is_preferred_over_editor() {
		assert_eq!(
			editor_from(&[("VISUAL", "nano"), ("EDITOR", "ed")]).unwrap(),
			editor("nano", &[])
		);
		assert_eq!(editor_from(&[("EDITOR", "ed")]).unwrap(), editor("ed", &[]));
	}

	#[test]
	fn an_empty_variable_counts_as_unset() {
		assert_eq!(
			editor_from(&[("VISUAL", "  "), ("EDITOR", "ed")]).unwrap(),
			editor("ed", &[])
		);
		assert_eq!(
			editor_from(&[("EDITOR", "")]).unwrap(),
			editor(DEFAULT_EDITOR, &[])
		);
	}

	#[test]
	fn without_variables_the_platform_default_is_used() {
		assert_eq!(editor_from(&[]).unwrap(), editor(DEFAULT_EDITOR, &[]));
	}

	#[cfg(not(windows))]
	#[test]
	fn an_editor_variable_may_hold_quoted_arguments() {
		assert_eq!(
			editor_from(&[("EDITOR", "code --wait '--title=a note'")]).unwrap(),
			editor("code", &["--wait", "--title=a note"])
		);
		let err = editor_from(&[("EDITOR", "code 'unclosed")]).unwrap_err();
		assert_eq!(
			err.to_string(),
			"Cannot read the editor command $EDITOR: code 'unclosed"
		);
	}

	#[test]
	fn the_newline_an_editor_adds_is_not_an_edit() {
		assert_eq!(
			without_added_final_newline("text\n".to_owned(), "text"),
			"text"
		);
		assert_eq!(
			without_added_final_newline("text\r\n".to_owned(), "text"),
			"text"
		);
		// only one newline is the editor's
		assert_eq!(
			without_added_final_newline("text\n\n".to_owned(), "text"),
			"text\n"
		);
		// content that ended with a newline keeps what the editor saved
		assert_eq!(
			without_added_final_newline("text\n".to_owned(), "text\n"),
			"text\n"
		);
		assert_eq!(
			without_added_final_newline("text".to_owned(), "text\n"),
			"text"
		);
	}

	#[test]
	fn editing_without_a_terminal_or_a_content_file_is_refused() {
		let ui = Printed::default().ui();
		let err = edit_source(&ui, None, |_| None).unwrap_err();
		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			err.to_string(),
			"Cannot ask \"Open the note in an editor?\": stdin is not a terminal. Pass \
			 --content-file <file>, or --content-file - to read the content from stdin."
		);
	}

	#[test]
	fn a_content_file_needs_no_terminal() {
		let ui = Printed::default().ui();
		let path = Path::new("-");
		assert_eq!(
			edit_source(&ui, Some(path), |_| None).unwrap(),
			EditSource::File(path)
		);
	}

	#[test]
	fn a_terminal_edits_in_the_editor() {
		let ui = Printed::default().prompting_ui();
		assert_eq!(
			edit_source(&ui, None, |name| (name == "EDITOR")
				.then(|| "ed".to_owned()))
			.unwrap(),
			EditSource::Editor(editor("ed", &[]))
		);
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn the_editor_edits_a_temporary_file_named_with_the_extension() {
		// the "editor" checks the extension and the content it is given, then replaces it
		let script = r#"case "$1" in *.md) ;; *) exit 3 ;; esac
			[ "$(cat "$1")" = "before" ] || exit 4
			printf 'after\n' > "$1""#;
		let editor = editor("sh", &["-c", script, "sh"]);
		let (edited, temp_file) = edit_in_editor(&editor, "before", "md").await.unwrap();
		assert_eq!(edited, "after\n");
		temp_file.close().unwrap();
	}

	#[test]
	fn an_edit_kept_after_a_concurrent_change_is_named() {
		let described = "\"Todo\"";
		assert_eq!(
			changed_meanwhile(
				described,
				UnsavedEdit::TempFile(Path::new("/tmp/filen-note-a.md"))
			)
			.to_string(),
			"Note \"Todo\" changed while it was being \
			 edited, so the edit was not saved; it is in /tmp/filen-note-a.md"
		);
		assert_eq!(
			changed_meanwhile(described, UnsavedEdit::ContentFile(Path::new("new.md"))).to_string(),
			"Note \"Todo\" changed while it was being \
			 read, so the content of new.md was not saved"
		);
		assert_eq!(
			changed_meanwhile(described, UnsavedEdit::ContentFile(Path::new("-"))).to_string(),
			"Note \"Todo\" changed while it was being \
			 read, so the content read from stdin was not saved"
		);
	}

	#[test]
	fn an_edit_that_could_not_be_saved_is_kept_and_named() {
		let temp_file = tempfile::NamedTempFile::new().unwrap().into_temp_path();
		let path = temp_file.to_path_buf();
		std::fs::write(&path, "the edit").unwrap();
		let err = keep_unsaved_edit(
			UI::failure("No note is titled \"x\" or has that UUID"),
			EditedIn::TempFile(temp_file),
		);
		assert_eq!(
			format!("{err:#}"),
			format!(
				"The edit was not saved; it is in {}: No note is titled \"x\" or has that UUID",
				path.display()
			)
		);
		assert_eq!(exit_code(&err), 1);
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "the edit");
		std::fs::remove_file(&path).unwrap();
	}

	#[test]
	fn a_content_file_that_could_not_be_saved_keeps_the_error() {
		let err = keep_unsaved_edit(
			UI::failure("Failed to save note \"Todo\""),
			EditedIn::ContentFile(Path::new("new.md")),
		);
		assert_eq!(format!("{err:#}"), "Failed to save note \"Todo\"");
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn an_editor_that_fails_changes_nothing() {
		let err = edit_in_editor(&editor("false", &[]), "before", "txt")
			.await
			.unwrap_err();
		assert_eq!(exit_code(&err), 1);
		assert!(
			err.to_string().starts_with("The editor false exited with "),
			"{err:#}"
		);
		assert!(
			err.to_string().ends_with("; the note was not changed"),
			"{err:#}"
		);
	}
}
