use std::path::PathBuf;

use super::*;

fn path(text: &str) -> RootPath {
	RootPath::parse(text.split('\\').filter(|part| !part.is_empty())).unwrap()
}

#[test]
fn paths_differing_only_in_case_share_a_key() {
	assert_eq!(
		path("Docs\\Report.TXT").case_key(),
		path("docs\\report.txt").case_key()
	);
	assert_ne!(path("docs\\a").case_key(), path("docs\\b").case_key());
	// The separator keeps `a\bc` and `ab\c` apart.
	assert_ne!(path("a\\bc").case_key(), path("ab\\c").case_key());
}

#[test]
fn parent_name_and_join_walk_the_components() {
	let file = path("a\\b\\c.txt");
	assert_eq!(file.name().map(PlaceholderName::as_str), Some("c.txt"));
	assert_eq!(file.parent(), Some(path("a\\b")));
	assert_eq!(file.depth(), 3);
	assert_eq!(RootPath::root().parent(), None);
	assert!(RootPath::root().is_root());
	assert_eq!(
		path("a").join(PlaceholderName::parse("b").unwrap()),
		path("a\\b")
	);
	assert_eq!(
		file.with_name(PlaceholderName::parse("d.txt").unwrap()),
		Some(path("a\\b\\d.txt"))
	);
	assert_eq!(file.to_string(), "a\\b\\c.txt");
	assert_eq!(file.slash_joined(), "a/b/c.txt");
}

#[test]
fn starts_with_compares_whole_components_in_case_folding() {
	let file = path("Dir\\sub\\f.txt");
	assert!(file.starts_with(&path("dir")));
	assert!(file.starts_with(&path("DIR\\SUB")));
	assert!(file.starts_with(&file));
	assert!(file.starts_with(&RootPath::root()));
	assert!(!file.starts_with(&path("Di")));
	assert!(!path("dir").starts_with(&file));
}

#[test]
fn rebase_moves_a_subtree() {
	let from = path("old");
	let to = path("new\\place");
	assert_eq!(
		path("OLD\\a\\b").rebase(&from, &to),
		Some(path("new\\place\\a\\b"))
	);
	assert_eq!(path("old").rebase(&from, &to), Some(to.clone()));
	assert_eq!(path("other\\a").rebase(&from, &to), None);
}

#[test]
fn a_component_windows_cannot_hold_is_refused() {
	assert!(matches!(
		RootPath::parse(["ok", "bad."]),
		Err(RootPathError::Name { component, .. }) if component == "bad."
	));
	assert_eq!(
		RootPath::from_relative(&PathBuf::from("a/../b")),
		Err(RootPathError::NotAName)
	);
	assert_eq!(
		RootPath::from_relative(&PathBuf::from("a/b")),
		Ok(path("a\\b"))
	);
}
