use filen_mobile_native_cache::CacheError;

use super::*;
use crate::test_support::root_path;

#[test]
fn a_tag_is_eight_hex_digits_and_shows_nothing_of_the_path() {
	let tag = path_tag(&root_path("Private Folder\\secret.txt")).to_string();
	assert_eq!(tag.len(), 8);
	assert!(tag.chars().all(|c| c.is_ascii_hexdigit()), "{tag}");
}

#[test]
fn every_spelling_of_one_item_shares_its_tag() {
	assert_eq!(
		path_tag(&root_path("Dir\\File.txt")),
		path_tag(&root_path("dir\\FILE.TXT"))
	);
}

#[test]
fn two_items_have_two_tags() {
	assert_ne!(path_tag(&root_path("a.txt")), path_tag(&root_path("b.txt")));
}

#[test]
fn a_cache_error_shows_its_kind_alone() {
	let failed = CacheError::remote("Path secret-folder\\secret.txt no longer resolves");
	assert_eq!(error(&failed).to_string(), "the cache failed (remote)");
}
