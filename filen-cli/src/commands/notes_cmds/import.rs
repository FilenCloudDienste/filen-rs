//! [cli-doc] notes-import
//! `import-notes <dir>` creates a note from every `.md`, `.txt` and `.html` file in a local
//! directory, the reverse of `export-notes`: a note titled after the file's name without its
//! extension, a Markdown note from a `.md` file, a plain text note from a `.txt` file and a rich
//! text note from an `.html` file. Other files, hidden files and subdirectories are left out.
//! The `trash/` and `archive/` subdirectories an export writes are left out too, unless
//! `--include-trashed` is given: then their notes are imported and moved to the trash or the
//! archive. A checklist exported as Markdown comes back as a Markdown note, and a code note as a
//! plain text one. A file that fails is reported, the others are still imported, and the command
//! then fails.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use filen_sdk_rs::auth::Client;
use filen_types::api::v3::notes::NoteType;

use super::{content::type_for_extension, create_filled_note};
use crate::{auth::LazyClient, ui::UI};

/// Where an imported note goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
	Notes,
	Trash,
	Archive,
}

/// A file to import as a note.
#[derive(Debug, PartialEq, Eq)]
struct ImportFile {
	path: PathBuf,
	title: String,
	note_type: NoteType,
	placement: Placement,
}

/// The files of a directory that `import-notes` imports, and how many it leaves out for their
/// extension.
#[derive(Debug, Default, PartialEq, Eq)]
struct ImportPlan {
	files: Vec<ImportFile>,
	unsupported: usize,
}

pub(super) async fn import_notes(
	ui: &mut UI,
	client: &mut LazyClient,
	dir: &Path,
	include_trashed: bool,
) -> Result<()> {
	// a bad directory fails before logging in
	let plan = plan_import(dir, include_trashed)?;
	if plan.unsupported > 0 {
		ui.print_muted_to_stderr(&format!(
			"Skipping {} files that are not .md, .txt or .html files",
			plan.unsupported
		));
	}
	if plan.files.is_empty() {
		ui.print_success("No notes to import");
		return Ok(());
	}

	let client = client.get(ui).await?;
	let total = plan.files.len();
	let mut failed = 0usize;
	for file in plan.files {
		if let Err(e) = import_note(client, file).await {
			failed += 1;
			ui.print_warning(&format!("{e:#}"));
		}
	}

	ui.print_success(&format!(
		"Imported {} notes from {}",
		total - failed,
		dir.display()
	));
	if failed > 0 {
		return Err(UI::failure(&format!(
			"Failed to import {failed} of {total} notes"
		)));
	}
	Ok(())
}

async fn import_note(client: &Client, file: ImportFile) -> Result<()> {
	let content = std::fs::read_to_string(&file.path)
		.with_context(|| format!("Failed to read {}", file.path.display()))?;
	let mut note = create_filled_note(client, Some(file.title), file.note_type, &content)
		.await
		.with_context(|| format!("Failed to import {}", file.path.display()))?;
	let moved = match file.placement {
		Placement::Notes => return Ok(()),
		Placement::Trash => client.trash_note(&mut note).await,
		Placement::Archive => client.archive_note(&mut note).await,
	};
	moved.with_context(|| {
		format!(
			"Imported {} as note {}, but failed to move it to the {}",
			file.path.display(),
			note.uuid(),
			match file.placement {
				Placement::Trash => "trash",
				Placement::Notes | Placement::Archive => "archive",
			}
		)
	})
}

/// The files `import-notes` imports from `dir`, in the order of their names, without reading
/// them: the notes from `dir` itself, then with `include_trashed` those of its `archive/` and
/// `trash/` subdirectories.
fn plan_import(dir: &Path, include_trashed: bool) -> Result<ImportPlan> {
	let mut plan = ImportPlan::default();
	add_import_files(&mut plan, dir, Placement::Notes)?;
	if include_trashed {
		for (name, placement) in [("archive", Placement::Archive), ("trash", Placement::Trash)] {
			let subdir = dir.join(name);
			if subdir.is_dir() {
				add_import_files(&mut plan, &subdir, placement)?;
			}
		}
	}
	Ok(plan)
}

/// Adds the files directly in `dir` to `plan`, leaving out subdirectories and hidden files.
fn add_import_files(plan: &mut ImportPlan, dir: &Path, placement: Placement) -> Result<()> {
	let read_error = |e: std::io::Error| {
		UI::failure(&format!("Failed to read directory {}: {e}", dir.display()))
	};
	let mut paths = std::fs::read_dir(dir)
		.map_err(read_error)?
		.map(|entry| entry.map(|entry| entry.path()))
		.collect::<Result<Vec<_>, _>>()
		.map_err(read_error)?;
	paths.sort();

	for path in paths {
		let (Some(stem), Some(name)) = (path.file_stem(), path.file_name()) else {
			continue;
		};
		if name.to_string_lossy().starts_with('.') || path.is_dir() {
			continue;
		}
		let note_type = path
			.extension()
			.and_then(|extension| type_for_extension(&extension.to_string_lossy()));
		let Some(note_type) = note_type else {
			plan.unsupported += 1;
			continue;
		};
		plan.files.push(ImportFile {
			title: stem.to_string_lossy().into_owned(),
			path,
			note_type,
			placement,
		});
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn file(path: PathBuf, title: &str, note_type: NoteType, placement: Placement) -> ImportFile {
		ImportFile {
			path,
			title: title.to_owned(),
			note_type,
			placement,
		}
	}

	/// A directory as `export-notes` leaves it, plus files and directories it does not write.
	fn export_dir() -> assert_fs::TempDir {
		let dir = assert_fs::TempDir::new().unwrap();
		for name in [
			"Shopping.md",
			"plain.txt",
			"Rich Text.HTML",
			"photo.jpg",
			"no extension",
			".hidden.md",
			"trash/Old.txt",
			"archive/Kept.md",
			"archive/notes.pdf",
			"other/Nested.md",
		] {
			let path = dir.path().join(name);
			std::fs::create_dir_all(path.parent().unwrap()).unwrap();
			std::fs::write(path, "content").unwrap();
		}
		dir
	}

	#[test]
	fn files_are_imported_by_extension_and_trash_and_archive_are_left_out() {
		let dir = export_dir();
		let root = dir.path();
		assert_eq!(
			plan_import(root, false).unwrap(),
			ImportPlan {
				files: vec![
					file(
						root.join("Rich Text.HTML"),
						"Rich Text",
						NoteType::Rich,
						Placement::Notes
					),
					file(
						root.join("Shopping.md"),
						"Shopping",
						NoteType::Md,
						Placement::Notes
					),
					file(
						root.join("plain.txt"),
						"plain",
						NoteType::Text,
						Placement::Notes
					),
				],
				unsupported: 2,
			}
		);
	}

	#[test]
	fn include_trashed_imports_the_archive_and_the_trash_too() {
		let dir = export_dir();
		let root = dir.path();
		let plan = plan_import(root, true).unwrap();
		assert_eq!(plan.unsupported, 3);
		assert_eq!(
			plan.files
				.iter()
				.map(|file| (file.title.as_str(), file.placement))
				.collect::<Vec<_>>(),
			[
				("Rich Text", Placement::Notes),
				("Shopping", Placement::Notes),
				("plain", Placement::Notes),
				("Kept", Placement::Archive),
				("Old", Placement::Trash),
			]
		);
	}

	#[test]
	fn include_trashed_without_those_subdirectories_imports_the_rest() {
		let dir = assert_fs::TempDir::new().unwrap();
		std::fs::write(dir.path().join("a.txt"), "a").unwrap();
		let plan = plan_import(dir.path(), true).unwrap();
		assert_eq!(plan.files.len(), 1);
	}

	#[test]
	fn a_missing_directory_fails() {
		let dir = assert_fs::TempDir::new().unwrap();
		let missing = dir.path().join("missing");
		let err = plan_import(&missing, false).unwrap_err();
		assert!(
			err.to_string()
				.starts_with(&format!("Failed to read directory {}: ", missing.display())),
			"{err:#}"
		);
	}
}
