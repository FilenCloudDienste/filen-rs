//! Checklist notes are stored as HTML, a series of `<ul data-checked="true|false">` blocks each
//! holding one or more `<li>` items, e.g.
//! `<ul data-checked="false"><li>Item</li></ul><ul data-checked="true"><li>Done</li></ul>`.
//! The CLI exports them as a Markdown task list instead.

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
