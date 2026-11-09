//! Splitting the command line `$VISUAL` or `$EDITOR` holds into the editor's program and arguments.

/// Splits an editor command line into words. A shell's quoting rules on Unix; on Windows, where a
/// path is full of backslashes, only double quotes group words.
pub(crate) fn split_command_line(line: &str) -> Option<Vec<String>> {
	if cfg!(windows) {
		split_on_double_quotes(line)
	} else {
		shlex::split(line)
	}
}

/// Splits `line` at whitespace outside double quotes, with no escapes. `None` for an unclosed
/// quote.
fn split_on_double_quotes(line: &str) -> Option<Vec<String>> {
	let mut words = Vec::new();
	let mut word: Option<String> = None;
	let mut quoted = false;
	for c in line.chars() {
		match c {
			'"' => {
				quoted = !quoted;
				word.get_or_insert_with(String::new);
			}
			c if c.is_whitespace() && !quoted => words.extend(word.take()),
			c => word.get_or_insert_with(String::new).push(c),
		}
	}
	if quoted {
		return None;
	}
	words.extend(word);
	Some(words)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn double_quotes_group_words_without_escapes() {
		assert_eq!(
			split_on_double_quotes(r#""C:\Program Files\Editor\editor.exe" -multiInst  -n"#),
			Some(vec![
				r"C:\Program Files\Editor\editor.exe".to_owned(),
				"-multiInst".to_owned(),
				"-n".to_owned(),
			])
		);
		assert_eq!(
			split_on_double_quotes(r"notepad"),
			Some(vec!["notepad".to_owned()])
		);
		assert_eq!(
			split_on_double_quotes(r#"a "" b"#),
			Some(vec!["a".to_owned(), String::new(), "b".to_owned()])
		);
		assert_eq!(split_on_double_quotes(r#"a "b"#), None);
		assert_eq!(split_on_double_quotes("  "), Some(vec![]));
	}

	#[test]
	fn the_backslashes_of_an_unquoted_windows_path_are_kept() {
		assert_eq!(
			split_on_double_quotes(r"C:\Tools\vim\vim.exe"),
			Some(vec![r"C:\Tools\vim\vim.exe".to_owned()])
		);
	}

	#[test]
	fn a_quoted_windows_path_with_spaces_is_one_word() {
		assert_eq!(
			split_on_double_quotes(r#""C:\Program Files\Vim\vim.exe" -f"#),
			Some(vec![
				r"C:\Program Files\Vim\vim.exe".to_owned(),
				"-f".to_owned()
			])
		);
	}
}
