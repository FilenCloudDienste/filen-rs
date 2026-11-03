//! [cli-doc] notes-export
//! `export-notes <dir>` writes every note to a file in a local directory. A directory that does
//! not exist yet, or is empty, gets the notes directly (it is created only when there are notes to
//! export). One that already holds something gets a new `filen-notes-export-<timestamp>`
//! subdirectory for them, so that no existing file is overwritten.
//!
//! Notes go into the export directory itself, trashed notes into its `trash/` subdirectory and
//! archived notes into `archive/`; each subdirectory is created only when a note goes into it. A
//! file is named after its note's title, with characters a file name cannot hold replaced by `_`
//! (a note whose title cannot be decrypted is named `note-<uuid>`), and `-1`, `-2`, ... appended
//! to tell notes with the same title apart. The extension follows the note's type: rich text notes
//! are exported as `.html`, Markdown notes and checklists as `.md` (a checklist as a Markdown task
//! list), and plain text and code notes as `.txt`.

use std::{
	collections::HashSet,
	io,
	path::{Path, PathBuf},
	sync::Arc,
};

use anyhow::{Context as _, Result};
use filen_sdk_rs::{auth::Client, notes::Note};
use filen_types::api::v3::notes::NoteType;

use super::checklist::checklist_html_to_markdown;
use crate::{auth::LazyClient, ui::UI};

const ILLEGAL_FILE_NAME_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

pub(super) async fn export_notes(
	ui: &mut UI,
	client: &mut LazyClient,
	export_root: Option<&str>,
) -> Result<()> {
	// a bad path fails before logging in, but nothing is created until there is something to
	// export
	let export_dir = choose_export_root(export_root.unwrap_or("."))?;

	let client = client.get(ui).await?;
	let notes = client.list_notes().await.context("Failed to list notes")?;
	if notes.is_empty() {
		ui.print_success("No notes to export");
		return Ok(());
	}
	let total = notes.len();
	create_export_root(&export_dir)?;

	// Assign every note its target path up front, sequentially, so that the disambiguating
	// suffixes for duplicate titles don't depend on the order the fetches happen to finish in.
	let mut used_paths: HashSet<PathBuf> = HashSet::new();
	let notes = notes
		.into_iter()
		.map(|note| {
			let path = target_path(&export_dir, &note, &mut used_paths);
			(note, path)
		})
		.collect::<Vec<(Note, PathBuf)>>();

	// Create the `trash` and `archive` subdirectories, but only if notes actually land in them.
	for (_, path) in &notes {
		let parent = path.parent().expect("target paths always have a parent");
		std::fs::create_dir_all(parent)
			.with_context(|| format!("Failed to create directory {}", parent.display()))?;
	}

	// Fetch the contents concurrently; the SDK's HTTP stack rate-limits us.
	let mut join_set = tokio::task::JoinSet::new();
	for (note, path) in notes {
		let client = Arc::clone(client);
		join_set.spawn(async move { export_note(&client, note, path).await });
	}

	let mut exported = 0usize;
	let mut failures: Vec<String> = Vec::new();
	while let Some(result) = join_set.join_next().await {
		match result.context("Failed to join note export task")? {
			Ok(()) => exported += 1,
			Err(e) => failures.push(format!("{e:#}")),
		}
	}

	for failure in &failures {
		ui.print_warning(failure);
	}
	ui.print_success(&format!(
		"Exported {} notes to {}",
		exported,
		export_dir.display()
	));
	if !failures.is_empty() {
		return Err(UI::failure(&format!(
			"Failed to export {} of {} notes",
			failures.len(),
			total
		)));
	}

	Ok(())
}

async fn export_note(client: &Client, mut note: Note, path: PathBuf) -> Result<()> {
	let note_str = format!(
		"note \"{}\" ({})",
		note.title().unwrap_or("<undecryptable title>"),
		note.uuid()
	);

	let content = client
		.get_note_content(&mut note)
		.await
		.with_context(|| format!("Failed to fetch {note_str}"))?
		.ok_or_else(|| UI::failure(&format!("Failed to decrypt {note_str}")))?;
	let content = match note.note_type() {
		NoteType::Checklist => checklist_html_to_markdown(&content),
		_ => content,
	};

	std::fs::write(&path, content)
		.with_context(|| format!("Failed to write {} to {}", note_str, path.display()))
}

/// Decides which directory the notes are exported to, without creating anything:
/// [`create_export_root`] does that once there are notes to export. A missing or empty directory
/// is used as it is, and one that already holds something gets a fresh
/// `filen-notes-export-<timestamp>` subdirectory, so that no existing file is overwritten.
fn choose_export_root(path_str: &str) -> Result<PathBuf> {
	let path = PathBuf::from(path_str);
	let metadata = match std::fs::metadata(&path) {
		Ok(metadata) => metadata,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(path),
		Err(e) => return Err(UI::failure(&format!("Failed to read {path_str}: {e}"))),
	};
	if !metadata.is_dir() {
		return Err(UI::failure(&format!("Not a directory: {path_str}")));
	}
	let mut entries = std::fs::read_dir(&path)
		.map_err(|e| UI::failure(&format!("Failed to read directory {path_str}: {e}")))?;
	if entries.next().is_none() {
		return Ok(path);
	}

	let timestamp = chrono::Local::now().format("%Y-%m-%d-%H-%M-%S");
	Ok(path.join(format!("filen-notes-export-{timestamp}")))
}

fn create_export_root(export_root: &Path) -> Result<()> {
	std::fs::create_dir_all(export_root).map_err(|e| {
		UI::failure(&format!(
			"Failed to create directory {}: {e}",
			export_root.display()
		))
	})
}

/// Picks a unique path for a note, inserting it into `used_paths`.
/// Active notes go into the export root, trashed and archived ones into a subdirectory each.
fn target_path(export_root: &Path, note: &Note, used_paths: &mut HashSet<PathBuf>) -> PathBuf {
	let directory = if note.trashed() {
		export_root.join("trash")
	} else if note.archived() {
		export_root.join("archive")
	} else {
		export_root.to_path_buf()
	};

	let file_name = match note.title().map(sanitize_file_name) {
		Some(title) if !title.is_empty() => title,
		// The title is missing when its decryption failed, so fall back to something that at
		// least identifies the note.
		_ => format!("note-{}", note.uuid()),
	};

	let extension = match note.note_type() {
		NoteType::Rich => "html",
		NoteType::Md | NoteType::Checklist => "md",
		NoteType::Text | NoteType::Code => "txt",
	};

	unique_path(&directory, &file_name, extension, used_paths)
}

/// Returns `<directory>/<file_name>.<extension>`, appending `-1`, `-2`, ... to the file name
/// until the path is one that hasn't been claimed yet.
fn unique_path(
	directory: &Path,
	file_name: &str,
	extension: &str,
	used_paths: &mut HashSet<PathBuf>,
) -> PathBuf {
	for i in 0.. {
		let suffix = if i == 0 {
			String::new()
		} else {
			format!("-{i}")
		};
		let path = directory.join(format!("{file_name}{suffix}.{extension}"));
		if used_paths.insert(path.clone()) {
			return path;
		}
	}
	unreachable!()
}

fn sanitize_file_name(file_name: &str) -> String {
	file_name
		.replace(ILLEGAL_FILE_NAME_CHARS, "_")
		.trim()
		.to_string()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sanitize_file_names() {
		assert_eq!(sanitize_file_name("Plain Text"), "Plain Text");
		assert_eq!(
			sanitize_file_name("a/b\\c:d*e?f\"g<h>i|j"),
			"a_b_c_d_e_f_g_h_i_j"
		);
		assert_eq!(sanitize_file_name("  padded  "), "padded");
		assert_eq!(sanitize_file_name("///"), "___");
		assert_eq!(sanitize_file_name(""), "");
	}

	#[test]
	fn a_missing_export_directory_is_exported_into_directly() {
		let parent = assert_fs::TempDir::new().unwrap();
		let missing = parent.path().join("missing").join("nested");

		let root = choose_export_root(missing.to_str().unwrap()).unwrap();
		assert_eq!(root, missing);
		create_export_root(&root).unwrap();
		assert!(root.is_dir());
	}

	#[test]
	fn an_empty_export_directory_is_exported_into_directly() {
		let dir = assert_fs::TempDir::new().unwrap();

		let root = choose_export_root(dir.path().to_str().unwrap()).unwrap();
		assert_eq!(root, dir.path());
	}

	#[test]
	fn a_non_empty_export_directory_gets_a_timestamped_subdirectory() {
		let dir = assert_fs::TempDir::new().unwrap();
		std::fs::write(dir.path().join("some file"), "").unwrap();

		let root = choose_export_root(dir.path().to_str().unwrap()).unwrap();
		assert_eq!(root.parent(), Some(dir.path()));
		assert!(
			root.file_name()
				.unwrap()
				.to_string_lossy()
				.starts_with("filen-notes-export-")
		);
		create_export_root(&root).unwrap();
		assert!(std::fs::read_dir(&root).unwrap().next().is_none());
	}

	#[test]
	fn an_export_path_that_is_a_file_is_rejected() {
		let dir = assert_fs::TempDir::new().unwrap();
		let file = dir.path().join("some file");
		std::fs::write(&file, "").unwrap();

		let err = choose_export_root(file.to_str().unwrap()).unwrap_err();
		assert_eq!(
			err.to_string(),
			format!("Not a directory: {}", file.display())
		);
	}

	#[test]
	fn choosing_the_export_root_creates_nothing() {
		let parent = assert_fs::TempDir::new().unwrap();
		std::fs::write(parent.path().join("some file"), "").unwrap();

		choose_export_root(parent.path().to_str().unwrap()).unwrap();
		assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 1);

		let missing = parent.path().join("missing").join("nested");
		choose_export_root(missing.to_str().unwrap()).unwrap();
		assert!(!parent.path().join("missing").exists());
	}

	#[test]
	fn unique_paths() {
		let dir = Path::new("/export");
		let mut used = HashSet::new();
		assert_eq!(
			unique_path(dir, "Same Title", "txt", &mut used),
			dir.join("Same Title.txt")
		);
		assert_eq!(
			unique_path(dir, "Same Title", "txt", &mut used),
			dir.join("Same Title-1.txt")
		);
		assert_eq!(
			unique_path(dir, "Same Title", "txt", &mut used),
			dir.join("Same Title-2.txt")
		);
		// a different extension or directory is a different file
		assert_eq!(
			unique_path(dir, "Same Title", "md", &mut used),
			dir.join("Same Title.md")
		);
		assert_eq!(
			unique_path(&dir.join("trash"), "Same Title", "txt", &mut used),
			dir.join("trash").join("Same Title.txt")
		);
	}
}
