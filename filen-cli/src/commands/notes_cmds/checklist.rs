//! Checklist notes are stored as HTML, a series of `<ul data-checked="true|false">` blocks each
//! holding one or more `<li>` items, e.g.
//! `<ul data-checked="false"><li>Item</li></ul><ul data-checked="true"><li>Done</li></ul>`.
//! The CLI shows and edits them as a Markdown task list instead.

/// Converts a checklist note's HTML to Markdown task list items.
///
/// Anything that doesn't match the checklist shape is ignored rather than producing garbage.
pub(super) fn checklist_html_to_markdown(html: &str) -> String {
	let mut items: Vec<String> = Vec::new();
	let mut rest = html;

	while let Some((_, after_tag_name)) = rest.split_once("<ul") {
		let Some((attributes, body_start)) = after_tag_name.split_once('>') else {
			break;
		};
		let checked =
			attribute_value(attributes, "data-checked").is_some_and(|value| value == "true");

		// A missing closing tag means the rest of the input is the list body.
		let (body, remainder) = body_start.split_once("</ul>").unwrap_or((body_start, ""));

		for text in list_item_texts(body) {
			items.push(format!("- [{}] {}", if checked { "x" } else { " " }, text));
		}
		rest = remainder;
	}

	items.join("\n")
}

/// Converts a Markdown task list to a checklist note's HTML, the reverse of
/// [`checklist_html_to_markdown`].
///
/// Every non-empty line is one item: `- [ ] text` an unchecked one, `- [x] text` (or `[X]`) a
/// checked one, with `*` or `+` accepted as the bullet too. A line without a checkbox becomes an
/// unchecked item holding the line, without its bullet. Consecutive items of the same state share
/// one `<ul>`, as Filen's apps write them.
pub(super) fn markdown_to_checklist_html(markdown: &str) -> String {
	let mut html = String::new();
	let mut open_list: Option<bool> = None;
	for line in markdown.lines() {
		let line = line.trim();
		if line.is_empty() {
			continue;
		}
		let (checked, text) = task_list_item(line);
		if open_list != Some(checked) {
			if open_list.is_some() {
				html.push_str("</ul>");
			}
			html.push_str(if checked {
				"<ul data-checked=\"true\">"
			} else {
				"<ul data-checked=\"false\">"
			});
			open_list = Some(checked);
		}
		html.push_str("<li>");
		html.push_str(&encode_entities(text));
		html.push_str("</li>");
	}
	if open_list.is_some() {
		html.push_str("</ul>");
	}
	html
}

/// Whether a trimmed Markdown line is a checked item, and the item's text.
fn task_list_item(line: &str) -> (bool, &str) {
	let Some(rest) = ["- ", "* ", "+ "]
		.iter()
		.find_map(|bullet| line.strip_prefix(bullet))
		.or_else(|| ["-", "*", "+"].contains(&line).then_some(""))
	else {
		return (false, line);
	};
	let rest = rest.trim_start();
	if let Some(text) = rest.strip_prefix("[ ]") {
		(false, text.trim())
	} else if let Some(text) = rest
		.strip_prefix("[x]")
		.or_else(|| rest.strip_prefix("[X]"))
	{
		(true, text.trim())
	} else {
		(false, rest)
	}
}

/// Reads the value of a double- or single-quoted attribute out of a tag's attribute list.
fn attribute_value<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
	let (_, rest) = attributes.split_once(name)?;
	let rest = rest.trim_start().strip_prefix('=')?.trim_start();
	let mut value = rest.chars();
	let quote = value.next().filter(|c| matches!(c, '"' | '\''))?;
	value.as_str().split_once(quote).map(|(value, _)| value)
}

/// Yields the text of every `<li>` in `body`, with nested inline tags stripped and entities
/// decoded — the equivalent of cheerio's `$(li).text().trim()`.
fn list_item_texts(body: &str) -> Vec<String> {
	let mut texts = Vec::new();
	let mut rest = body;

	while let Some((_, after_tag_name)) = rest.split_once("<li") {
		let Some((_, content_start)) = after_tag_name.split_once('>') else {
			break;
		};
		let (content, remainder) = content_start
			.split_once("</li>")
			.unwrap_or((content_start, ""));
		texts.push(decode_entities(&strip_tags(content)).trim().to_string());
		rest = remainder;
	}

	texts
}

/// Removes every `<...>` tag from `html`, keeping the text between them.
pub(super) fn strip_tags(html: &str) -> String {
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

fn encode_entities(text: &str) -> String {
	// `&` is encoded first, so that the `&` of the other entities is not encoded again.
	text.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;")
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

	#[test]
	fn a_task_list_converts_to_checklist_html_grouped_by_state() {
		assert_eq!(
			markdown_to_checklist_html("- [ ] Item 1\n- [ ] Item 2\n- [x] Done\n- [ ] other"),
			"<ul data-checked=\"false\"><li>Item 1</li><li>Item 2</li></ul>\
			<ul data-checked=\"true\"><li>Done</li></ul>\
			<ul data-checked=\"false\"><li>other</li></ul>"
		);
	}

	#[test]
	fn task_list_lines_in_other_shapes_still_become_items() {
		assert_eq!(markdown_to_checklist_html(""), "");
		assert_eq!(markdown_to_checklist_html("\n  \n"), "");
		assert_eq!(
			markdown_to_checklist_html(
				"* [X] star\r\n+ [ ]   plus  \n\n  - plain bullet\nno bullet"
			),
			"<ul data-checked=\"true\"><li>star</li></ul>\
			<ul data-checked=\"false\"><li>plus</li><li>plain bullet</li><li>no bullet</li></ul>"
		);
		// a bullet needs a space after it, so a leading minus sign stays in the text
		assert_eq!(
			markdown_to_checklist_html("-5 degrees"),
			"<ul data-checked=\"false\"><li>-5 degrees</li></ul>"
		);
		assert_eq!(
			markdown_to_checklist_html("- [ ]\n-"),
			"<ul data-checked=\"false\"><li></li><li></li></ul>"
		);
	}

	#[test]
	fn item_text_is_encoded_as_html() {
		assert_eq!(
			markdown_to_checklist_html("- [x] a & b <c> &lt;"),
			"<ul data-checked=\"true\"><li>a &amp; b &lt;c&gt; &amp;lt;</li></ul>"
		);
	}

	#[test]
	fn checklist_html_survives_a_round_trip_through_markdown() {
		for html in [
			"",
			"<ul data-checked=\"false\"><li>Item 1</li><li>Item 2</li></ul>\
			<ul data-checked=\"true\"><li>Checked item</li></ul>",
			"<ul data-checked=\"true\"><li>a &amp; b &lt;c&gt; &amp;lt; \"q\" 'a'</li></ul>",
			"<ul data-checked=\"false\"><li>ünïcödé ✔</li><li></li></ul>",
		] {
			assert_eq!(
				markdown_to_checklist_html(&checklist_html_to_markdown(html)),
				html
			);
		}
	}

	#[test]
	fn a_task_list_survives_a_round_trip_through_checklist_html() {
		for markdown in [
			"",
			"- [ ] Item 1\n- [x] Done\n- [x] Also done\n- [ ] other",
			"- [x] a & b <c> &lt; &amp;",
			"- [ ] [x] looks checked but is not",
		] {
			assert_eq!(
				checklist_html_to_markdown(&markdown_to_checklist_html(markdown)),
				markdown
			);
		}
	}
}
