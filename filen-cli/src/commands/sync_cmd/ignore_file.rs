//! The ignore rules a sync pairs file gives a pair, which the CLI writes into the `.filenignore` at
//! the pair's local root, where the sync engine reads them like any other `.filenignore`.

use std::{
	collections::HashSet,
	fs::{self, Permissions},
	io::{ErrorKind, Write as _},
	path::Path,
};

use anyhow::Result;

use crate::ui::UI;

/// The name of the rule file the sync engine reads in each directory.
const FILENIGNORE: &str = ".filenignore";

/// Adds each line of `rules` that the `.filenignore` at `root` does not hold yet to its end,
/// creating the file if there is none. Lines already there are left as they are, so adding the
/// same rules again changes nothing, and a rule that is no longer given is not removed.
///
/// The file is replaced in one rename, never left half written: the new content is written to a
/// temporary file beside it first, named as the sync engine's partial downloads are, which it never
/// syncs. A file that is not valid UTF-8, which the engine does not read either, is not changed.
pub(super) fn add_rules(root: &Path, rules: &[String]) -> Result<()> {
	let path = root.join(FILENIGNORE);
	let (text, permissions) = match fs::symlink_metadata(&path) {
		Ok(meta) if meta.is_file() => {
			let bytes = fs::read(&path)
				.map_err(|e| UI::failure(&format!("Failed to read {}: {e}", path.display())))?;
			let text = String::from_utf8(bytes).map_err(|_| {
				UI::failure(&format!(
					"{} is not valid UTF-8, so the sync pair's ignore rules cannot be added to it",
					path.display()
				))
			})?;
			(text, Some(meta.permissions()))
		}
		Ok(_) => {
			return Err(UI::failure(&format!(
				"{} is not a file, so the sync pair's ignore rules cannot be added to it",
				path.display()
			)));
		}
		Err(e) if e.kind() == ErrorKind::NotFound => (String::new(), None),
		Err(e) => {
			return Err(UI::failure(&format!(
				"Failed to read {}: {e}",
				path.display()
			)));
		}
	};
	let Some(merged) = merged_rules(&text, rules) else {
		return Ok(());
	};
	write_replacing(root, &path, &merged, permissions).map_err(|e| {
		UI::failure(&format!(
			"Failed to write the sync pair's ignore rules to {}: {e}",
			path.display()
		))
	})
}

/// `text`, the content of a `.filenignore`, with each line of `rules` it does not hold appended,
/// or `None` when it holds them all. Blank lines in `rules` are not added.
fn merged_rules(text: &str, rules: &[String]) -> Option<String> {
	let mut present = text.lines().collect::<HashSet<_>>();
	let missing = rules
		.iter()
		.flat_map(|rule| rule.lines())
		.filter(|line| !line.trim().is_empty() && present.insert(line))
		.collect::<Vec<_>>();
	if missing.is_empty() {
		return None;
	}
	let mut merged = String::from(text);
	if !merged.is_empty() && !merged.ends_with('\n') {
		merged.push('\n');
	}
	for line in missing {
		merged.push_str(line);
		merged.push('\n');
	}
	Some(merged)
}

/// Replaces `path`, a file in `root`, with one holding `content` and, if given, `permissions`, in
/// one rename.
fn write_replacing(
	root: &Path,
	path: &Path,
	content: &str,
	permissions: Option<Permissions>,
) -> std::io::Result<()> {
	// `.filendl` is the extension of the sync engine's partial downloads, which it never syncs, so
	// not even a temporary file a crash left behind reaches the other side.
	let mut temp = tempfile::Builder::new()
		.prefix(".filenignore.")
		.suffix(".filendl")
		.tempfile_in(root)?;
	temp.write_all(content.as_bytes())?;
	if let Some(permissions) = permissions {
		temp.as_file().set_permissions(permissions)?;
	}
	temp.as_file().sync_all()?;
	temp.persist(path).map_err(|e| e.error)?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use std::{fs, path::Path};

	use super::{add_rules, merged_rules};

	fn rules(lines: &[&str]) -> Vec<String> {
		lines.iter().map(|line| (*line).to_owned()).collect()
	}

	/// The names in `dir`, sorted.
	fn names(dir: &Path) -> Vec<String> {
		let mut names = fs::read_dir(dir)
			.unwrap()
			.map(|entry| entry.unwrap().file_name().into_string().unwrap())
			.collect::<Vec<_>>();
		names.sort();
		names
	}

	#[test]
	fn the_rules_are_written_into_a_new_filenignore() {
		let root = tempfile::tempdir().unwrap();

		add_rules(root.path(), &rules(&["*.log", "build/"])).unwrap();

		assert_eq!(
			fs::read_to_string(root.path().join(".filenignore")).unwrap(),
			"*.log\nbuild/\n"
		);
		// and nothing else, no temporary file
		assert_eq!(names(root.path()), [".filenignore"]);
	}

	#[test]
	fn without_rules_no_filenignore_is_created() {
		let root = tempfile::tempdir().unwrap();

		add_rules(root.path(), &[]).unwrap();

		assert_eq!(names(root.path()), Vec::<String>::new());
	}

	#[test]
	fn only_the_rules_a_filenignore_lacks_are_appended() {
		let root = tempfile::tempdir().unwrap();
		fs::write(root.path().join(".filenignore"), "# mine\n*.log").unwrap();

		add_rules(root.path(), &rules(&["*.log", "build/"])).unwrap();

		assert_eq!(
			fs::read_to_string(root.path().join(".filenignore")).unwrap(),
			"# mine\n*.log\nbuild/\n"
		);
	}

	#[test]
	fn adding_the_same_rules_again_changes_nothing() {
		let root = tempfile::tempdir().unwrap();
		add_rules(root.path(), &rules(&["*.log", "build/"])).unwrap();

		add_rules(root.path(), &rules(&["build/", "*.log"])).unwrap();

		assert_eq!(
			fs::read_to_string(root.path().join(".filenignore")).unwrap(),
			"*.log\nbuild/\n"
		);
		assert_eq!(names(root.path()), [".filenignore"]);
	}

	/// The engine does not read such a file, and rewriting it could lose what the user meant.
	#[test]
	fn a_filenignore_that_is_not_utf_8_is_not_changed() {
		let root = tempfile::tempdir().unwrap();
		let path = root.path().join(".filenignore");
		fs::write(&path, b"*.log\n\xff\n").unwrap();

		let error = add_rules(root.path(), &rules(&["build/"])).unwrap_err();

		assert_eq!(
			error.to_string(),
			format!(
				"{} is not valid UTF-8, so the sync pair's ignore rules cannot be added to it",
				path.display()
			)
		);
		assert_eq!(fs::read(&path).unwrap(), b"*.log\n\xff\n");
		assert_eq!(names(root.path()), [".filenignore"]);
	}

	#[test]
	fn a_rule_of_several_lines_is_added_line_by_line() {
		assert_eq!(
			merged_rules("a\n", &rules(&["a\nb\n\nc"])).as_deref(),
			Some("a\nb\nc\n")
		);
	}

	#[test]
	fn a_rule_given_twice_is_added_once() {
		assert_eq!(
			merged_rules("", &rules(&["a", "a"])).as_deref(),
			Some("a\n")
		);
	}
}
