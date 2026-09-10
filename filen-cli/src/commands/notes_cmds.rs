use std::{
	collections::HashSet,
	path::{Path, PathBuf},
	sync::Arc,
};

use anyhow::{Context as _, Result};
use filen_sdk_rs::{auth::Client, notes::Note};
use filen_types::api::v3::notes::NoteType;

use crate::{auth::LazyClient, ui::UI};

use checklist_parser::checklist_html_to_markdown;

const ILLEGAL_FILE_NAME_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

pub(crate) async fn export_notes(
	ui: &mut UI,
	client: &mut LazyClient,
	export_root: Option<&str>,
) -> Result<()> {
	let export_dir = resolve_export_root(export_root.unwrap_or("."))?;

	let client = client.get(ui).await?;
	let notes = client.list_notes().await.context("Failed to list notes")?;
	if notes.is_empty() {
		ui.print_success("No notes to export");
		return Ok(());
	}
	let total = notes.len();

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

fn resolve_export_root(path_str: &str) -> Result<PathBuf> {
	let path = PathBuf::from(path_str);
	if path.exists() && !path.is_dir() {
		return Err(UI::failure(&format!("Not a directory: {path_str}")));
	}

	let timestamp = chrono::Local::now().format("%Y-%m-%d-%H-%M-%S");
	let export_root = path.join(format!("filen-notes-export-{timestamp}"));
	std::fs::create_dir_all(&export_root).map_err(|e| {
		UI::failure(&format!(
			"Failed to create directory {}: {e}",
			export_root.display()
		))
	})?;
	Ok(export_root)
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

mod checklist_parser {
	/// Converts a checklist note's HTML to markdown task list items.
	///
	/// Checklist notes are stored as a series of `<ul data-checked="true|false">` blocks, each
	/// holding one or more `<li>` items, e.g.
	/// `<ul data-checked="false"><li>Item</li></ul><ul data-checked="true"><li>Done</li></ul>`.
	/// Anything that doesn't match that shape is ignored rather than producing garbage.
	pub(super) fn checklist_html_to_markdown(html: &str) -> String {
		let mut items: Vec<String> = Vec::new();
		let mut rest = html;

		while let Some(ul_start) = rest.find("<ul") {
			let after_tag_name = &rest[ul_start + "<ul".len()..];
			let Some(tag_end) = after_tag_name.find('>') else {
				break;
			};
			let checked = attribute_value(&after_tag_name[..tag_end], "data-checked")
				.is_some_and(|value| value == "true");

			let body_start = &after_tag_name[tag_end + 1..];
			// A missing closing tag means the rest of the input is the list body.
			let (body, remainder) = match body_start.find("</ul>") {
				Some(ul_end) => (&body_start[..ul_end], &body_start[ul_end + "</ul>".len()..]),
				None => (body_start, ""),
			};

			for text in list_item_texts(body) {
				items.push(format!("- [{}] {}", if checked { "x" } else { " " }, text));
			}
			rest = remainder;
		}

		items.join("\n")
	}

	/// Reads the value of a double- or single-quoted attribute out of a tag's attribute list.
	fn attribute_value<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
		let start = attributes.find(name)? + name.len();
		let rest = attributes[start..].trim_start();
		let rest = rest.strip_prefix('=')?.trim_start();
		let quote = rest.chars().next()?;
		if quote != '"' && quote != '\'' {
			return None;
		}
		let value = &rest[quote.len_utf8()..];
		value.find(quote).map(|end| &value[..end])
	}

	/// Yields the text of every `<li>` in `body`, with nested inline tags stripped and entities
	/// decoded — the equivalent of cheerio's `$(li).text().trim()`.
	fn list_item_texts(body: &str) -> Vec<String> {
		let mut texts = Vec::new();
		let mut rest = body;

		while let Some(li_start) = rest.find("<li") {
			let after_tag_name = &rest[li_start + "<li".len()..];
			let Some(tag_end) = after_tag_name.find('>') else {
				break;
			};
			let content_start = &after_tag_name[tag_end + 1..];
			let (content, remainder) = match content_start.find("</li>") {
				Some(li_end) => (
					&content_start[..li_end],
					&content_start[li_end + "</li>".len()..],
				),
				None => (content_start, ""),
			};
			texts.push(decode_entities(&strip_tags(content)).trim().to_string());
			rest = remainder;
		}

		texts
	}

	fn strip_tags(html: &str) -> String {
		let mut result = String::with_capacity(html.len());
		let mut in_tag = false;
		for c in html.chars() {
			match c {
				'<' => in_tag = true,
				'>' => in_tag = false,
				_ if !in_tag => result.push(c),
				_ => {}
			}
		}
		result
	}

	fn decode_entities(text: &str) -> String {
		// `&amp;` is decoded last so that e.g. `&amp;lt;` stays the literal text "&lt;".
		text.replace("&nbsp;", " ")
			.replace("&lt;", "<")
			.replace("&gt;", ">")
			.replace("&quot;", "\"")
			.replace("&#39;", "'")
			.replace("&apos;", "'")
			.replace("&amp;", "&")
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		#[test]
		fn convert_checklist() {
			let html = "<ul data-checked=\"false\"><li>Item 1</li><li>Item 2</li></ul>\
			<ul data-checked=\"true\"><li>Checked item</li></ul>\
			<ul data-checked=\"false\"><li>other</li></ul>";
			assert_eq!(
				checklist_html_to_markdown(html),
				"- [ ] Item 1\n- [ ] Item 2\n- [x] Checked item\n- [ ] other"
			);
		}

		#[test]
		fn convert_checklist_edge_cases() {
			assert_eq!(checklist_html_to_markdown(""), "");
			assert_eq!(checklist_html_to_markdown("<p>not a checklist</p>"), "");
			// nested inline formatting is flattened, entities are decoded
			assert_eq!(
				checklist_html_to_markdown(
					"<ul data-checked=\"true\"><li> <strong>bold</strong> &amp; <em>italic</em> </li></ul>"
				),
				"- [x] bold & italic"
			);
			// a missing data-checked attribute means unchecked
			assert_eq!(
				checklist_html_to_markdown("<ul><li>Item</li></ul>"),
				"- [ ] Item"
			);
			// single quotes are accepted too
			assert_eq!(
				checklist_html_to_markdown("<ul data-checked='true'><li>Item</li></ul>"),
				"- [x] Item"
			);
			// unterminated markup doesn't panic or loop forever
			assert_eq!(
				checklist_html_to_markdown("<ul data-checked=\"true\"><li>Item"),
				"- [x] Item"
			);
			assert_eq!(checklist_html_to_markdown("<ul"), "");
		}

		#[test]
		fn decode_entity_sequences() {
			assert_eq!(decode_entities("a &amp; b"), "a & b");
			assert_eq!(decode_entities("&lt;tag&gt;"), "<tag>");
			// a double-escaped entity stays literal
			assert_eq!(decode_entities("&amp;lt;"), "&lt;");
		}
	}
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
	fn export_root_is_always_a_fresh_timestamped_directory() {
		let parent = assert_fs::TempDir::new().unwrap();

		// an existing directory is accepted whether or not it already holds something
		std::fs::write(parent.path().join("some file"), "").unwrap();
		let root = resolve_export_root(parent.path().to_str().unwrap()).unwrap();
		assert_eq!(root.parent(), Some(parent.path()));
		assert!(
			root.file_name()
				.unwrap()
				.to_string_lossy()
				.starts_with("filen-notes-export-")
		);
		assert!(std::fs::read_dir(&root).unwrap().next().is_none());

		// a directory that doesn't exist yet is created along the way
		let missing = parent.path().join("missing").join("nested");
		let root = resolve_export_root(missing.to_str().unwrap()).unwrap();
		assert_eq!(root.parent(), Some(missing.as_path()));
		assert!(root.is_dir());

		// but a path that exists and isn't a directory is rejected
		let file = parent.path().join("some file");
		assert!(resolve_export_root(file.to_str().unwrap()).is_err());
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
