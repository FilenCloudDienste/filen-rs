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

use std::path::Path;

use anyhow::{Context as _, Result};

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

	let edited = match source {
		EditSource::File(path) => read_content_file(path).await?,
		EditSource::Editor(editor) => {
			let edited = edit_in_editor(&editor, &current, file_extension(note_type)).await?;
			without_added_final_newline(edited, &current)
		}
	};
	if edited == current {
		ui.print_success(&format!("Note {} is unchanged", describe_note(&note)));
		return Ok(());
	}

	let new_stored = stored_content(note_type, &edited);
	let preview = note_preview(note_type, &new_stored);
	client
		.set_note_content(&mut note, &new_stored, preview)
		.await
		.with_context(|| format!("Failed to save note {}", describe_note(&note)))?;
	ui.print_success(&format!("Saved note {}", describe_note(&note)));
	Ok(())
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
/// knows the format, and returns what the file holds once the editor exits. The file is removed
/// afterwards.
async fn edit_in_editor(editor: &EditorCommand, content: &str, extension: &str) -> Result<String> {
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
	path.close()
		.context("Failed to remove the note's temporary file")?;
	Ok(edited)
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
		assert_eq!(
			edit_in_editor(&editor, "before", "md").await.unwrap(),
			"after\n"
		);
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
