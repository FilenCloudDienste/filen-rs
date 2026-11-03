//! How a note's content is shown, written and named in files, depending on its type.

use std::borrow::Cow;

use clap::ValueEnum;
use filen_types::api::v3::notes::NoteType;

use super::checklist::{checklist_html_to_markdown, markdown_to_checklist_html, strip_tags};

/// A note type, as the command line names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum NoteKind {
	Text,
	Md,
	Code,
	Rich,
	Checklist,
}

impl From<NoteKind> for NoteType {
	fn from(kind: NoteKind) -> Self {
		match kind {
			NoteKind::Text => NoteType::Text,
			NoteKind::Md => NoteType::Md,
			NoteKind::Code => NoteType::Code,
			NoteKind::Rich => NoteType::Rich,
			NoteKind::Checklist => NoteType::Checklist,
		}
	}
}

/// The name of a note type, as `--type` takes it and the listings show it.
pub(super) fn type_name(note_type: NoteType) -> &'static str {
	match note_type {
		NoteType::Text => "text",
		NoteType::Md => "md",
		NoteType::Code => "code",
		NoteType::Rich => "rich",
		NoteType::Checklist => "checklist",
	}
}

/// The extension of the file a note of this type is exported to or edited in.
pub(super) fn file_extension(note_type: NoteType) -> &'static str {
	match note_type {
		NoteType::Rich => "html",
		NoteType::Md | NoteType::Checklist => "md",
		NoteType::Text | NoteType::Code => "txt",
	}
}

/// A note's content as a person reads and writes it: a checklist as a Markdown task list, the
/// other types as they are stored.
pub(super) fn readable_content(note_type: NoteType, stored: &str) -> Cow<'_, str> {
	match note_type {
		NoteType::Checklist => Cow::Owned(checklist_html_to_markdown(stored)),
		NoteType::Text | NoteType::Md | NoteType::Code | NoteType::Rich => Cow::Borrowed(stored),
	}
}

/// A note's content as it is stored, from what [`readable_content`] gives.
pub(super) fn stored_content(note_type: NoteType, readable: &str) -> Cow<'_, str> {
	match note_type {
		NoteType::Checklist => Cow::Owned(markdown_to_checklist_html(readable)),
		NoteType::Text | NoteType::Md | NoteType::Code | NoteType::Rich => Cow::Borrowed(readable),
	}
}

/// Most characters of a note's preview, as Filen's apps cut it.
const PREVIEW_MAX_CHARS: usize = 128;

/// The preview of a note's stored content that note lists show, derived as Filen's apps derive
/// it: the first paragraph of a rich text note, the first non-empty item of a checklist, and the
/// first line of any other note, without tags and cut to [`PREVIEW_MAX_CHARS`] characters.
pub(super) fn note_preview(note_type: NoteType, stored: &str) -> String {
	let first_part = match note_type {
		NoteType::Rich => match stored.split_once("<p><br></p>") {
			Some((first_paragraph, _)) => first_paragraph,
			None => first_line(stored),
		},
		NoteType::Checklist => {
			let without_lists = stored
				.replace("<ul data-checked=\"false\">", "")
				.replace("<ul data-checked=\"true\">", "")
				.replace('\n', "");
			let first_item = without_lists
				.split("<li>")
				.filter_map(|item| item.split("</li>").next())
				.map(str::trim)
				.find(|item| !item.is_empty())
				.map(strip_tags)
				.unwrap_or_default();
			return first_item.chars().take(PREVIEW_MAX_CHARS).collect();
		}
		NoteType::Text | NoteType::Md | NoteType::Code => first_line(stored),
	};
	strip_tags(first_part)
		.chars()
		.take(PREVIEW_MAX_CHARS)
		.collect()
}

fn first_line(text: &str) -> &str {
	text.split('\n').next().unwrap_or(text)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn note_kinds_map_to_the_note_types_of_the_same_name() {
		for kind in NoteKind::value_variants() {
			let name = kind.to_possible_value().unwrap().get_name().to_owned();
			assert_eq!(type_name(NoteType::from(*kind)), name);
		}
	}

	#[test]
	fn only_a_checklist_is_read_and_written_as_something_else() {
		let html = "<ul data-checked=\"true\"><li>Done</li></ul>";
		assert_eq!(readable_content(NoteType::Checklist, html), "- [x] Done");
		assert_eq!(
			stored_content(NoteType::Checklist, "- [x] Done"),
			html.to_owned()
		);
		for note_type in [NoteType::Text, NoteType::Md, NoteType::Code, NoteType::Rich] {
			assert_eq!(readable_content(note_type, html), html);
			assert_eq!(stored_content(note_type, html), html);
		}
	}

	#[test]
	fn previews_follow_the_note_type() {
		assert_eq!(note_preview(NoteType::Text, ""), "");
		assert_eq!(note_preview(NoteType::Text, "first\nsecond"), "first");
		assert_eq!(note_preview(NoteType::Md, "# Title\nbody"), "# Title");
		assert_eq!(
			note_preview(NoteType::Rich, "<p>one</p><p><br></p><p>two</p>"),
			"one"
		);
		assert_eq!(
			note_preview(NoteType::Rich, "<p><b>bold</b> line</p>\n<p>next</p>"),
			"bold line"
		);
		assert_eq!(
			note_preview(
				NoteType::Checklist,
				"<ul data-checked=\"false\"><li> </li><li><b>first</b> item</li></ul>"
			),
			"first item"
		);
		assert_eq!(note_preview(NoteType::Checklist, "<ul></ul>"), "");
	}

	#[test]
	fn a_preview_is_cut_to_its_maximum_in_characters() {
		let long = "ä".repeat(PREVIEW_MAX_CHARS + 1);
		assert_eq!(
			note_preview(NoteType::Text, &long),
			"ä".repeat(PREVIEW_MAX_CHARS)
		);
		let exact = "ä".repeat(PREVIEW_MAX_CHARS);
		assert_eq!(note_preview(NoteType::Text, &exact), exact);
	}
}
