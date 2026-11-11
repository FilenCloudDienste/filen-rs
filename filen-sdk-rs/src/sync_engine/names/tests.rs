use std::fs;

use super::*;

const NFC: &str = "caf\u{e9}";
const NFD: &str = "cafe\u{301}";

#[test]
fn posix_refuses_only_a_name_longer_than_255_bytes() {
	assert_eq!(posix_rejection(&"a".repeat(255)), None);
	assert_eq!(
		posix_rejection(&"a".repeat(256)),
		Some(DeviceRejection::TooLong { len: 256, max: 255 })
	);
	for name in ["foo.", "foo ", " foo", "a:b", "CON", "nul.txt", "a\u{1}b"] {
		assert_eq!(posix_rejection(name), None, "{name:?}");
	}
}

#[test]
fn the_device_check_is_the_hosts_own() {
	let expected = if cfg!(windows) {
		Some(DeviceRejection::TrailingDotOrSpace)
	} else {
		None
	};
	assert_eq!(device_rejection("foo."), expected);
}

#[test]
fn an_ascii_path_is_joined_as_spelled() {
	let root = Path::new("/nowhere");
	assert_eq!(
		on_disk(root, "a/b.txt", Leaf::Existing),
		root.join("a").join("b.txt")
	);
}

/// The NFD spelling of a directory and a file under it is found from their NFC keys, and the path
/// found opens the file.
#[test]
fn an_nfd_entry_is_found_from_its_nfc_key() {
	let dir = tempfile::tempdir().unwrap();
	fs::create_dir(dir.path().join(NFD)).unwrap();
	fs::write(dir.path().join(NFD).join(NFD), b"nfd").unwrap();
	let key = format!("{NFC}/{NFC}");
	for leaf in [Leaf::Existing, Leaf::Target] {
		let found = respelled(dir.path(), &key, leaf);
		assert_eq!(fs::read(&found).unwrap(), b"nfd");
		#[cfg(not(target_vendor = "apple"))]
		assert_eq!(found, dir.path().join(NFD).join(NFD));
	}
}

/// A new entry under an NFD directory lands IN that directory, under its key's spelling, rather
/// than beside it under an NFC twin of the directory.
#[test]
fn a_target_under_an_nfd_directory_lands_inside_it() {
	let dir = tempfile::tempdir().unwrap();
	fs::create_dir(dir.path().join(NFD)).unwrap();
	let found = respelled(dir.path(), &format!("{NFC}/new-{NFC}.txt"), Leaf::Target);
	fs::write(&found, b"new").unwrap();
	let listed: Vec<String> = fs::read_dir(dir.path())
		.unwrap()
		.map(|entry| entry.unwrap().file_name().into_string().unwrap())
		.collect();
	assert_eq!(listed.len(), 1, "{listed:?}");
	#[cfg(not(target_vendor = "apple"))]
	assert_eq!(found, dir.path().join(NFD).join(format!("new-{NFC}.txt")));
}

/// A spelling that is neither the NFC nor the NFD form (a precomposed character beside a decomposed
/// one) is found by listing the directory — for an existing entry only.
#[test]
fn a_mixed_spelling_is_found_by_listing_for_an_existing_entry_only() {
	let mixed = "\u{e9}e\u{301}";
	let key: String = mixed.nfc().collect();
	let dir = tempfile::tempdir().unwrap();
	fs::write(dir.path().join(mixed), b"mixed").unwrap();
	let existing = respelled(dir.path(), &key, Leaf::Existing);
	assert_eq!(fs::read(&existing).unwrap(), b"mixed");
	#[cfg(not(target_vendor = "apple"))]
	{
		assert_eq!(existing, dir.path().join(mixed));
		assert_eq!(
			respelled(dir.path(), &key, Leaf::Target),
			dir.path().join(&key)
		);
	}
}

/// Two entries that normalize alike are a collision: the listing names neither, and the key's own
/// spelling is kept. Only a file system that tells the two forms apart can hold them.
#[cfg(not(target_vendor = "apple"))]
#[test]
fn two_spellings_of_one_key_leave_the_key_as_spelled() {
	let mixed = "\u{e9}e\u{301}";
	let other = "e\u{301}\u{e9}";
	let key: String = mixed.nfc().collect();
	assert_eq!(other.nfc().collect::<String>(), key);
	let dir = tempfile::tempdir().unwrap();
	fs::write(dir.path().join(mixed), b"one").unwrap();
	fs::write(dir.path().join(other), b"two").unwrap();
	assert_eq!(
		respelled(dir.path(), &key, Leaf::Existing),
		dir.path().join(&key)
	);
}

#[test]
fn a_missing_path_keeps_its_keys_spelling() {
	let dir = tempfile::tempdir().unwrap();
	let key = format!("{NFC}/x/{NFC}");
	assert_eq!(
		respelled(dir.path(), &key, Leaf::Existing),
		joined(dir.path(), &key)
	);
}

#[test]
fn only_a_name_with_a_decomposable_character_has_another_form() {
	assert!(has_other_form(NFC));
	assert!(has_other_form("\u{c5}ngstr\u{f6}m"));
	for name in [
		"cafe",
		"\u{6771}\u{4eac}",
		"\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}",
		"\u{3a9}",
		"",
	] {
		assert!(!has_other_form(name), "{name:?}");
	}
}

/// A listing keeps only the entries whose name is not NFC: an NFC entry is found by its key's own
/// `lstat`, and two spellings of one key name neither.
#[test]
fn a_kept_listing_holds_only_the_names_spelled_otherwise() {
	let dir = tempfile::tempdir().unwrap();
	fs::write(dir.path().join("plain.txt"), b"x").unwrap();
	fs::write(dir.path().join(NFC), b"x").unwrap();
	fs::write(dir.path().join(format!("{NFD}.txt")), b"x").unwrap();
	let listed = list_respelled(dir.path());
	assert_eq!(
		listed,
		HashMap::from([(format!("{NFC}.txt"), Some(format!("{NFD}.txt")))])
	);
}

/// Paths an observation asks about that are gone list their directory once between them, however
/// many there are, and a mixed spelling is still found through that one listing.
#[test]
fn gone_paths_list_their_directory_once() {
	let mixed = "\u{e9}e\u{301}";
	let key: String = mixed.nfc().collect();
	let dir = tempfile::tempdir().unwrap();
	fs::create_dir(dir.path().join("d")).unwrap();
	fs::write(dir.path().join("d").join(mixed), b"mixed").unwrap();
	let mut listings = Listings::default();
	for gone in ["\u{e9}1", "\u{e9}2", "\u{e9}3"] {
		let rel_path = format!("d/{gone}");
		assert_eq!(
			respelled_missing(dir.path(), &rel_path, Leaf::Existing, Some(&mut listings)),
			joined(dir.path(), &rel_path)
		);
	}
	assert_eq!(listings.dirs.len(), 1);
	let found = respelled_missing(
		dir.path(),
		&format!("d/{key}"),
		Leaf::Existing,
		Some(&mut listings),
	);
	assert_eq!(fs::read(&found).unwrap(), b"mixed");
	assert_eq!(listings.dirs.len(), 1);
}

/// `stat_on_disk` answers for a path as `stat` does, through the spelling on disk: an entry spelled
/// otherwise is found and stat'ed, and a gone one reads `NotFound` at its key's spelling.
#[test]
fn stat_on_disk_reads_the_entry_under_its_spelling() {
	let dir = tempfile::tempdir().unwrap();
	fs::create_dir(dir.path().join(NFD)).unwrap();
	fs::write(dir.path().join(NFD).join("f.txt"), b"abc").unwrap();
	let mut listings = Listings::default();
	let (path, stat) = stat_on_disk(dir.path(), &format!("{NFC}/f.txt"), &mut listings);
	assert_eq!(stat.unwrap().len(), 3);
	assert_eq!(fs::read(&path).unwrap(), b"abc");
	let gone = format!("{NFC}/g\u{e9}.txt");
	let (path, stat) = stat_on_disk(dir.path(), &gone, &mut listings);
	assert_eq!(stat.unwrap_err().kind(), ErrorKind::NotFound);
	#[cfg(not(target_vendor = "apple"))]
	assert_eq!(path, dir.path().join(NFD).join("g\u{e9}.txt"));
	#[cfg(target_vendor = "apple")]
	assert_eq!(path, joined(dir.path(), &gone));
}
