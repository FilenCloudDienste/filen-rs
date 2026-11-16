use super::*;

#[test]
fn each_windows_rejection_reaches_the_caller() {
	for (name, rejection) in [
		("a:b", DeviceRejection::ForbiddenChar(':')),
		("x.", DeviceRejection::TrailingDotOrSpace),
		("x ", DeviceRejection::TrailingDotOrSpace),
		("CON.txt", DeviceRejection::ReservedName),
		("COM\u{b9}", DeviceRejection::ReservedName),
		("\u{1}", DeviceRejection::ForbiddenChar('\u{1}')),
	] {
		assert_eq!(
			PlaceholderName::parse(name),
			Err(NameRejection::Windows(rejection)),
			"{name:?}"
		);
	}
}

#[test]
fn an_empty_name_is_refused() {
	assert_eq!(PlaceholderName::parse(""), Err(NameRejection::Empty));
}

#[test]
fn dot_and_dot_dot_are_refused() {
	for name in [".", ".."] {
		assert_eq!(
			PlaceholderName::parse(name),
			Err(NameRejection::Windows(DeviceRejection::TrailingDotOrSpace)),
			"{name:?}"
		);
	}
}

#[test]
fn a_name_of_255_units_is_accepted_and_256_refused() {
	let at_limit = "a".repeat(255);
	assert_eq!(
		PlaceholderName::parse(&at_limit).unwrap().as_str(),
		at_limit
	);
	assert_eq!(
		PlaceholderName::parse(&"a".repeat(256)),
		Err(NameRejection::Windows(DeviceRejection::TooLong {
			len: 256,
			max: 255
		}))
	);
}

#[test]
fn an_astral_character_counts_two_units_at_the_limit() {
	// 253 units of ASCII and one emoji: 255 units.
	let at_limit = format!("{}\u{1f600}", "a".repeat(253));
	let name = PlaceholderName::parse(&at_limit).unwrap();
	assert_eq!(name.as_wide_nul().len(), 256);
	let over = format!("{}\u{1f600}", "a".repeat(254));
	assert_eq!(
		PlaceholderName::parse(&over),
		Err(NameRejection::Windows(DeviceRejection::TooLong {
			len: 256,
			max: 255
		}))
	);
}

#[test]
fn the_wide_form_ends_in_one_nul() {
	let name = PlaceholderName::parse("caf\u{e9}.txt").unwrap();
	assert_eq!(
		name.as_wide_nul(),
		&[0x63, 0x61, 0x66, 0xe9, 0x2e, 0x74, 0x78, 0x74, 0][..]
	);
	assert_eq!(format!("{name:?}"), "\"caf\u{e9}.txt\"");
}

#[test]
fn case_keys_fold_simple_case_only() {
	let key = |name| PlaceholderName::parse(name).unwrap().case_key();
	assert_eq!(key("A.txt"), key("a.txt"));
	assert_eq!(key("a.txt"), "A.TXT");
	// `ß` uppercases to two characters, which NTFS's one-to-one table does not do.
	assert_eq!(key("straße"), "STRAßE");
	// Composed and decomposed `é` stay apart: NTFS normalizes nothing.
	assert_ne!(key("\u{e9}"), key("e\u{301}"));
	assert_eq!(key("e\u{301}"), "E\u{301}");
}
