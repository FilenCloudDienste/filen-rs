use std::fs;

use super::*;

const NFC: &str = "caf\u{e9}";
const NFD: &str = "cafe\u{301}";

#[test]
fn windows_refuses_a_trailing_dot_or_space() {
	assert_eq!(
		windows_rejection("foo."),
		Some(DeviceRejection::TrailingDotOrSpace)
	);
	assert_eq!(
		windows_rejection("foo "),
		Some(DeviceRejection::TrailingDotOrSpace)
	);
	assert_eq!(windows_rejection("foo.txt"), None);
	assert_eq!(windows_rejection(".hidden"), None);
}

#[test]
fn windows_refuses_its_forbidden_characters_and_control_characters() {
	for ch in WINDOWS_FORBIDDEN {
		let name = format!("a{ch}b");
		assert_eq!(
			windows_rejection(&name),
			Some(DeviceRejection::ForbiddenChar(ch)),
			"{name:?}"
		);
	}
	assert_eq!(
		windows_rejection("a\u{1}b"),
		Some(DeviceRejection::ForbiddenChar('\u{1}'))
	);
	// DEL and a fullwidth colon are fine on Windows, whatever the SDK's validator says.
	assert_eq!(windows_rejection("a\u{7f}b"), None);
	assert_eq!(windows_rejection("a\u{ff1a}b"), None);
}

#[test]
fn windows_reserves_device_names_with_and_without_an_extension() {
	for name in [
		"CON",
		"con",
		"Nul",
		"PRN",
		"aux",
		"COM1",
		"lpt9",
		"COM0",
		"COM\u{b9}",
		"LPT\u{b3}",
		"CONIN$",
		"conout$",
		"CON.txt",
		"nul.tar.gz",
		"Aux .txt",
	] {
		assert_eq!(
			windows_rejection(name),
			Some(DeviceRejection::ReservedName),
			"{name:?}"
		);
	}
	for name in [
		"CONSOLE",
		"CON1",
		"COM10",
		"LPT",
		"xCON",
		"NULL.txt",
		"COM\u{b4}",
		"con_.txt",
	] {
		assert_eq!(windows_rejection(name), None, "{name:?}");
	}
}

#[test]
fn windows_counts_a_names_length_in_utf16_units() {
	// 255 three-byte characters: 765 bytes, 255 units.
	let at_limit = "\u{20ac}".repeat(255);
	assert_eq!(windows_rejection(&at_limit), None);
	let over = "\u{20ac}".repeat(256);
	assert_eq!(
		windows_rejection(&over),
		Some(DeviceRejection::TooLong { len: 256, max: 255 })
	);
	// A character outside the BMP takes two units.
	let astral = "\u{1f600}".repeat(128);
	assert_eq!(
		windows_rejection(&astral),
		Some(DeviceRejection::TooLong { len: 256, max: 255 })
	);
}

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
