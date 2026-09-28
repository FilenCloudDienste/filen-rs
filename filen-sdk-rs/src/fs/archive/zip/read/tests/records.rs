//! Central directory records built by hand: how their names, zip64 fields and Unix modes are
//! read.

use super::*;

/// A central directory record for `name` and nothing else of note, made on `host`.
fn central_record(host: u16, flags: u16, name: &[u8], extra: &[u8], unix_mode: u32) -> Vec<u8> {
	[
		&CENTRAL_HEADER_SIG.to_le_bytes()[..],
		&((host << 8) | 20).to_le_bytes(),
		&20u16.to_le_bytes(),
		&flags.to_le_bytes(),
		// method, time, date, CRC-32, sizes
		&[0; 18],
		&u16::try_from(name.len()).unwrap().to_le_bytes(),
		&u16::try_from(extra.len()).unwrap().to_le_bytes(),
		// comment length, disk, internal attributes
		&[0; 6],
		&(unix_mode << 16).to_le_bytes(),
		&0u32.to_le_bytes(),
		name,
		extra,
	]
	.concat()
}

/// An Info-ZIP Unicode path field holding `name`, for a record whose raw name has `crc`.
fn unicode_path(crc: u32, name: &str) -> Vec<u8> {
	[
		&0x7075u16.to_le_bytes()[..],
		&u16::try_from(5 + name.len()).unwrap().to_le_bytes(),
		&[1],
		&crc.to_le_bytes(),
		name.as_bytes(),
	]
	.concat()
}

#[test]
fn names_are_decoded_as_their_writers_meant() {
	let utf8 = "Café/Résumé.txt".as_bytes();
	let cp437: &[u8] = b"Caf\x82/R\x82sum\x82.txt";
	// (host, flags, raw name, extra field, name, rewritten)
	let cases = [
		(
			HOST_DOS,
			FLAG_UTF8,
			utf8,
			Vec::new(),
			"Café/Résumé.txt",
			false,
		),
		// flagged, but not UTF-8: read as CP437, and said to be rewritten
		(
			HOST_DOS,
			FLAG_UTF8,
			cp437,
			Vec::new(),
			"Caf\u{E9}/R\u{E9}sum\u{E9}.txt",
			true,
		),
		(HOST_DOS, 0, cp437, Vec::new(), "Café/Résumé.txt", false),
		// macOS Archive Utility, ditto and Info-ZIP on Unix store UTF-8 without saying so
		(HOST_UNIX, 0, utf8, Vec::new(), "Café/Résumé.txt", false),
		(HOST_OS_X, 0, utf8, Vec::new(), "Café/Résumé.txt", false),
		// decomposed, as macOS may store it, and passed on as it is
		(
			HOST_OS_X,
			0,
			"Cafe\u{301}.txt".as_bytes(),
			Vec::new(),
			"Cafe\u{301}.txt",
			false,
		),
		// a Unix name that is not UTF-8 is in some local character set: CP437 is a guess, so
		// the name is said to be rewritten, as a tar's is
		(HOST_UNIX, 0, cp437, Vec::new(), "Café/Résumé.txt", true),
		// a DOS name that happens to be valid UTF-8 stays CP437 ("├⌐" is 0xC3 0xA9)
		(HOST_DOS, 0, utf8, Vec::new(), "Caf├⌐/R├⌐sum├⌐.txt", false),
		// the Unicode path field wins while it matches the raw name
		(
			HOST_DOS,
			0,
			cp437,
			unicode_path(crc32fast::hash(cp437), "Unicode/Näme.txt"),
			"Unicode/Näme.txt",
			false,
		),
		// once the raw name changed (an old tool renamed the entry), it is stale
		(
			HOST_DOS,
			0,
			cp437,
			unicode_path(crc32fast::hash(b"old name"), "Unicode/Näme.txt"),
			"Café/Résumé.txt",
			false,
		),
	];
	for (host, flags, raw, extra, name, rewritten) in cases {
		let record = central_record(host, flags, raw, &extra, 0o100_644);
		let (entry, _) = parse_central_header(&record, 0, 0).unwrap();
		assert_eq!(
			(entry.name.as_str(), entry.name_rewritten),
			(name, rewritten),
			"{host} {flags:#x} {raw:?}"
		);
	}
}

#[test]
fn the_zip64_field_holds_what_the_fixed_fields_leave_out_in_order() {
	const FULL: u32 = u32::MAX;
	let zip64 = |values: &[u64]| {
		[
			&0x0001u16.to_le_bytes()[..],
			&u16::try_from(values.len() * 8).unwrap().to_le_bytes(),
			&values
				.iter()
				.flat_map(|value| value.to_le_bytes())
				.collect::<Vec<_>>(),
		]
		.concat()
	};
	// (compressed size, size, offset as the fixed fields hold them, the zip64 values, and what
	// they come to as (size, compressed size, offset))
	for (fixed, values, read) in [
		(
			(FULL, FULL, FULL),
			vec![5 << 30, 6 << 30, 7 << 30],
			(5 << 30, 6 << 30, 7 << 30),
		),
		((FULL, 10, 20), vec![6 << 30], (10, 6 << 30, 20)),
		((10, FULL, 20), vec![5 << 30], (5 << 30, 10, 20)),
		((10, 20, FULL), vec![7 << 30], (20, 10, 7 << 30)),
	] {
		let mut record = central_record(HOST_UNIX, 0, b"big", &zip64(&values), 0o100_644);
		let (compressed_size, size, offset) = fixed;
		record[20..24].copy_from_slice(&compressed_size.to_le_bytes());
		record[24..28].copy_from_slice(&size.to_le_bytes());
		record[42..46].copy_from_slice(&offset.to_le_bytes());
		let (entry, _) = parse_central_header(&record, 0, 0).unwrap();
		assert_eq!(
			(entry.size, entry.compressed_size, entry.header_offset),
			read,
			"{fixed:?}"
		);
	}
}

#[test]
fn symlinks_are_recognised_from_unix_and_os_x() {
	for (host, kind) in [
		(HOST_UNIX, ZipKind::Symlink),
		(HOST_OS_X, ZipKind::Symlink),
		// a DOS host's high attribute bits are no Unix mode
		(HOST_DOS, ZipKind::File),
	] {
		let record = central_record(host, 0, b"link", &[], 0o120_755);
		let (entry, _) = parse_central_header(&record, 0, 0).unwrap();
		assert_eq!(entry.kind, kind, "{host}");
	}
}
