use super::*;
use crate::fs::archive::{
	format::TAR_CHECKSUM,
	test_support::{damaged_copies, tar_checksummed, tar_members},
};

/// A tar header block of `kind` for `name` with `size`, checksummed.
fn header(name: &[u8], kind: u8, size: u64) -> [u8; TAR_BLOCK] {
	let mut block = [0u8; TAR_BLOCK];
	block[..name.len()].copy_from_slice(name);
	block[100..108].copy_from_slice(b"0000644\0");
	block[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
	block[136..148].copy_from_slice(format!("{:011o}\0", 1_700_000_000u64).as_bytes());
	block[156] = kind;
	block[257..263].copy_from_slice(b"ustar\0");
	block[263..265].copy_from_slice(b"00");
	tar_checksummed(block)
}

/// `header` with the old GNU magic, as GNU tar writes for its own extensions.
fn gnu_header(name: &[u8], kind: u8, size: u64) -> [u8; TAR_BLOCK] {
	let mut block = header(name, kind, size);
	block[257..265].copy_from_slice(b"ustar  \0");
	tar_checksummed(block)
}

fn padded(data: &[u8]) -> Vec<u8> {
	let mut out = data.to_vec();
	out.resize(data.len().div_ceil(TAR_BLOCK) * TAR_BLOCK, 0);
	out
}

fn member(archive: &mut Vec<u8>, name: &[u8], kind: u8, data: &[u8]) {
	archive.extend_from_slice(&header(name, kind, data.len() as u64));
	archive.extend(padded(data));
}

fn pax_record(key: &str, value: &str) -> String {
	let body = format!(" {key}={value}\n");
	let mut len = body.len() + 1;
	while format!("{len}{body}").len() != len {
		len += 1;
	}
	format!("{len}{body}")
}

#[test]
fn members_and_their_data_are_read_in_order() {
	let mut archive = Vec::new();
	member(&mut archive, b"dir/", b'5', b"");
	member(&mut archive, b"dir/a.txt", b'0', b"hello");
	member(&mut archive, b"b.bin", b'0', &[7u8; 1000]);
	archive.extend([0u8; 2 * TAR_BLOCK]);
	let mut reader = TarReader::new(archive.as_slice(), 100);
	let members = tar_members(&mut reader);
	let summary: Vec<(&str, MemberKind, Vec<u8>)> = members
		.iter()
		.map(|(m, d)| (m.path.as_str(), m.kind.clone(), d.clone()))
		.collect();
	assert_eq!(
		summary,
		[
			("dir/", MemberKind::Dir, Vec::new()),
			("dir/a.txt", MemberKind::File, b"hello".to_vec()),
			("b.bin", MemberKind::File, vec![7u8; 1000]),
		]
	);
	assert_eq!(
		members[1].0.modified,
		Some(MemberTime {
			secs: 1_700_000_000,
			nanos: 0
		})
	);
}

#[test]
fn unread_data_is_skipped() {
	let mut archive = Vec::new();
	member(&mut archive, b"big", b'0', &[1u8; 3000]);
	member(&mut archive, b"next", b'0', b"x");
	let mut reader = TarReader::new(archive.as_slice(), 100);
	assert_eq!(reader.next_member().unwrap().unwrap().path, "big");
	assert_eq!(reader.next_member().unwrap().unwrap().path, "next");
	// a stream that ends at a block boundary without the marker ends the archive
	assert_eq!(reader.next_member().unwrap(), None);
}

#[test]
fn gnu_long_names_and_links_apply_to_the_next_member() {
	let long = "d/".repeat(100) + "file.txt";
	let mut archive = Vec::new();
	member(
		&mut archive,
		b"././@LongLink",
		b'L',
		format!("{long}\0").as_bytes(),
	);
	member(&mut archive, b"truncated", b'0', b"data");
	member(&mut archive, b"././@LongLink", b'K', b"target/of/link\0");
	member(&mut archive, b"link", b'2', b"");
	let members = tar_members(&mut TarReader::new(archive.as_slice(), 100));
	assert_eq!(members[0].0.path, long);
	assert_eq!(members[0].1, b"data");
	assert_eq!(
		members[1].0.kind,
		MemberKind::Symlink {
			target: "target/of/link".to_owned()
		}
	);
}

#[test]
fn hard_links_keep_their_target_however_it_is_stored() {
	let long = "t/".repeat(80) + "target.txt";
	let mut archive = Vec::new();
	let mut link = header(b"short", b'1', 0);
	link[157..167].copy_from_slice(b"docs/a.txt");
	archive.extend_from_slice(&tar_checksummed(link));
	member(
		&mut archive,
		b"././@LongLink",
		b'K',
		format!("{long}\0").as_bytes(),
	);
	member(&mut archive, b"long", b'1', b"");
	member(
		&mut archive,
		b"PaxHeader",
		b'x',
		pax_record("linkpath", "p\u{e4}x/target").as_bytes(),
	);
	// POSIX lets a hard link in a pax archive carry the file's data
	member(&mut archive, b"with-data", b'1', b"its own copy");
	let members = tar_members(&mut TarReader::new(archive.as_slice(), 100));
	let links: Vec<(&str, &MemberKind, &[u8])> = members
		.iter()
		.map(|(m, d)| (m.path.as_str(), &m.kind, d.as_slice()))
		.collect();
	let hardlink = |target: &str| MemberKind::Hardlink {
		target: target.to_owned(),
	};
	assert_eq!(
		links,
		[
			("short", &hardlink("docs/a.txt"), &b""[..]),
			("long", &hardlink(&long), &b""[..]),
			(
				"with-data",
				&hardlink("p\u{e4}x/target"),
				&b"its own copy"[..]
			),
		]
	);
}

/// Every member of `archive` with its size, and the error that ended the walk, if one did.
fn walk(archive: &[u8]) -> (Vec<(String, u64)>, Option<String>) {
	let mut reader = TarReader::new(archive, 100);
	let mut members = Vec::new();
	loop {
		match reader.next_member() {
			Ok(Some(member)) => members.push((member.path, member.size)),
			Ok(None) => return (members, None),
			Err(error) => return (members, Some(error.to_string())),
		}
	}
}

fn sized(members: &[(&str, u64)]) -> Vec<(String, u64)> {
	members
		.iter()
		.map(|&(path, size)| (path.to_owned(), size))
		.collect()
}

#[test]
fn a_hard_links_size_counts_only_in_a_pax_archive() {
	// old writers put the target's size on a hard link and no data after it: libarchive and
	// GNU tar read it as 0 on a GNU, pre-POSIX or ustar header, whatever follows
	let mut old = header(b"link", b'1', 2048);
	old[257..265].fill(0);
	for link in [
		gnu_header(b"link", b'1', 2048),
		tar_checksummed(old),
		header(b"link", b'1', 2048),
	] {
		let mut archive = link.to_vec();
		member(&mut archive, b"after", b'0', b"z");
		assert_eq!(walk(&archive), (sized(&[("link", 0), ("after", 1)]), None));
	}
	// so a stated size cannot hide a member from them as the link's data: what follows the
	// link is read as a header, and junk there is damage
	let mut archive = Vec::new();
	member(&mut archive, b"target", b'0', b"t");
	archive.extend_from_slice(&header(b"link", b'1', 4 * TAR_BLOCK_U64));
	archive.extend_from_slice(&[b'j'; TAR_BLOCK]);
	member(&mut archive, b"hidden", b'0', b"hidden");
	member(&mut archive, b"after", b'0', b"a");
	assert_eq!(
		walk(&archive),
		(
			sized(&[("target", 1), ("link", 0)]),
			Some("the tar archive is damaged: a header's checksum does not match".to_owned())
		)
	);
	// nor can the end-of-archive marker right after a link be taken for its data
	let mut archive = header(b"link", b'1', TAR_BLOCK_U64).to_vec();
	archive.extend([0u8; 2 * TAR_BLOCK]);
	assert_eq!(walk(&archive), (sized(&[("link", 0)]), None));
}

#[test]
fn a_hard_link_in_a_pax_archive_keeps_its_size() {
	let pax = |archive: &mut Vec<u8>, record: &str| {
		member(archive, b"PaxHeader", b'x', record.as_bytes());
	};
	// a PAX header makes the archive pax, and ustar headers after it keep it so; GNU long
	// name and link records leave it as it was
	let mut archive = Vec::new();
	pax(&mut archive, &pax_record("mtime", "1"));
	member(&mut archive, b"first", b'0', b"a");
	member(&mut archive, b"././@LongLink", b'K', b"target\0");
	member(&mut archive, b"link", b'1', &[5u8; TAR_BLOCK]);
	member(&mut archive, b"after", b'0', b"z");
	assert_eq!(
		walk(&archive),
		(
			sized(&[("first", 1), ("link", TAR_BLOCK_U64), ("after", 1)]),
			None
		)
	);
	// a GNU header after it makes the archive GNU again
	let mut archive = Vec::new();
	pax(&mut archive, &pax_record("mtime", "1"));
	archive.extend_from_slice(&gnu_header(b"link", b'1', TAR_BLOCK_U64));
	member(&mut archive, b"after", b'0', b"z");
	assert_eq!(walk(&archive), (sized(&[("link", 0), ("after", 1)]), None));
	// but a link's own PAX `size` holds on any header, as libarchive applies it last
	let mut archive = Vec::new();
	pax(&mut archive, &pax_record("size", "3"));
	archive.extend_from_slice(&gnu_header(b"link", b'1', 0));
	archive.extend(padded(b"abc"));
	member(&mut archive, b"after", b'0', b"z");
	assert_eq!(walk(&archive), (sized(&[("link", 3), ("after", 1)]), None));
}

#[test]
fn pax_size_overrides_the_ustar_field() {
	// the ustar field says 0, PAX says 700: the data is read as 700 bytes, so the next header
	// is found where GNU tar and bsdtar find it, and nothing inside the data is parsed as one
	let data = vec![9u8; 700];
	let pax = pax_record("size", "700") + &pax_record("path", "real/name.bin");
	let mut archive = Vec::new();
	member(&mut archive, b"PaxHeader", b'x', pax.as_bytes());
	archive.extend_from_slice(&header(b"shortname", b'0', 0));
	archive.extend(padded(&data));
	member(&mut archive, b"after", b'0', b"z");
	let members = tar_members(&mut TarReader::new(archive.as_slice(), 100));
	assert_eq!(members.len(), 2);
	assert_eq!(members[0].0.path, "real/name.bin");
	assert_eq!(members[0].1, data);
	assert_eq!(members[1].0.path, "after");
}

#[test]
fn a_size_near_u64_max_is_refused_rather_than_wrapped() {
	// its padding would wrap the skip to a single byte, reading the member's data as headers
	let pax = pax_record("size", &u64::MAX.to_string());
	let mut archive = Vec::new();
	member(&mut archive, b"PaxHeader", b'x', pax.as_bytes());
	member(&mut archive, b"huge", b'0', b"");
	member(&mut archive, b"smuggled", b'0', b"x");
	let mut reader = TarReader::new(archive.as_slice(), 100);
	assert_eq!(reader.next_member().unwrap().unwrap().size, u64::MAX);
	assert!(matches!(
		reader.next_member(),
		Err(TarError::Corrupt("a member's size is out of range"))
	));
}

#[test]
fn a_gnu_base_256_size_is_read() {
	// GNU tar stores sizes of 8 GiB and over in base-256, which no octal field holds
	let size = 9u64 << 30;
	let mut header = tar::Header::new_gnu();
	header.set_path("big.img").unwrap();
	header.set_entry_type(EntryType::Regular);
	header.set_size(size);
	header.set_cksum();
	assert_eq!(header.as_old().size[0] & 0x80, 0x80, "written in base-256");
	let mut archive = header.as_bytes().to_vec();
	archive.extend_from_slice(&[5u8; 2 * TAR_BLOCK]);
	let mut reader = TarReader::new(archive.as_slice(), 100);
	assert_eq!(reader.next_member().unwrap().unwrap().size, size);
	// the data it promises is not there
	assert!(matches!(
		reader.next_member(),
		Err(TarError::Corrupt("a member's data ends early"))
	));
}

#[test]
fn pax_times_keep_fractions_and_sign() {
	assert_eq!(
		parse_pax_time(b"1700000000.25"),
		Some(MemberTime {
			secs: 1_700_000_000,
			nanos: 250_000_000
		})
	);
	assert_eq!(
		parse_pax_time(b"-10.5"),
		Some(MemberTime {
			secs: -11,
			nanos: 500_000_000
		})
	);
	assert_eq!(
		parse_pax_time(b"-3"),
		Some(MemberTime { secs: -3, nanos: 0 })
	);
	assert_eq!(parse_pax_time(b"x"), None);
}

#[test]
fn gnu_sparse_members_are_classified_as_sparse() {
	let pax = pax_record("GNU.sparse.major", "1") + &pax_record("GNU.sparse.name", "vm.img");
	let mut archive = Vec::new();
	member(&mut archive, b"PaxHeader", b'x', pax.as_bytes());
	member(&mut archive, b"./GNUSparseFile.1/vm.img", b'0', b"map+data");
	archive.extend_from_slice(&gnu_header(b"old-sparse", b'S', 4));
	archive.extend(padded(b"data"));
	let members = tar_members(&mut TarReader::new(archive.as_slice(), 100));
	assert_eq!(members[0].0.kind, MemberKind::Sparse);
	assert_eq!(members[1].0.kind, MemberKind::Sparse);
}

#[test]
fn records_are_capped_before_they_are_read() {
	let mut archive = Vec::new();
	archive.extend_from_slice(&header(b"PaxHeader", b'x', MAX_PAX + 1));
	let error = TarReader::new(archive.as_slice(), 100)
		.next_member()
		.unwrap_err();
	assert!(matches!(error, TarError::Corrupt(_)), "{error}");
}

#[test]
fn every_header_counts_toward_the_member_limit() {
	let mut archive = Vec::new();
	for _ in 0..3 {
		member(
			&mut archive,
			b"PaxHeader",
			b'g',
			pax_record("comment", "x").as_bytes(),
		);
	}
	member(&mut archive, b"a", b'0', b"");
	let mut reader = TarReader::new(archive.as_slice(), 3);
	assert!(matches!(
		reader.next_member(),
		Err(TarError::TooManyMembers(3))
	));
}

#[test]
fn corruption_is_reported() {
	let mut archive = Vec::new();
	member(&mut archive, b"a", b'0', b"data");
	let mut bad_checksum = archive.clone();
	bad_checksum[0] = b'b';
	assert!(matches!(
		TarReader::new(bad_checksum.as_slice(), 100).next_member(),
		Err(TarError::Corrupt(_))
	));
	// data that ends early
	let truncated = &archive[..520];
	let mut reader = TarReader::new(truncated, 100);
	reader.next_member().unwrap();
	assert!(matches!(reader.next_member(), Err(TarError::Corrupt(_))));
	// a header cut short
	assert!(matches!(
		TarReader::new(&archive[..511], 100).next_member(),
		Err(TarError::Corrupt(_))
	));
	// a PAX record whose length runs past the header
	let mut archive = Vec::new();
	member(&mut archive, b"PaxHeader", b'x', b"99 path=a\n");
	member(&mut archive, b"a", b'0', b"");
	assert!(matches!(
		TarReader::new(archive.as_slice(), 100).next_member(),
		Err(TarError::Corrupt(_))
	));
}

#[test]
fn a_damaged_tar_never_panics() {
	// every kind of record the reader parses: PAX with a size, GNU long names and links,
	// an old GNU sparse member with an extended map, and plain members
	let mut archive = Vec::new();
	let pax = pax_record("size", "5")
		+ &pax_record("path", "p/q.txt")
		+ &pax_record("mtime", "-1.5")
		+ &pax_record("linkpath", "t");
	member(&mut archive, b"PaxHeader", b'x', pax.as_bytes());
	archive.extend_from_slice(&header(b"q", b'0', 0));
	archive.extend(padded(b"hello"));
	member(&mut archive, b"././@LongLink", b'L', b"a/long/name\0");
	member(&mut archive, b"././@LongLink", b'K', b"a/long/target\0");
	member(&mut archive, b"l", b'1', b"");
	let mut sparse = gnu_header(b"s", b'S', 4);
	sparse[482] = 1;
	archive.extend_from_slice(&tar_checksummed(sparse));
	archive.extend_from_slice(&[0u8; TAR_BLOCK]);
	archive.extend(padded(b"data"));
	member(&mut archive, b"dir/", b'5', b"");
	archive.extend([0u8; 2 * TAR_BLOCK]);

	let walk = |archive: &[u8]| {
		let mut reader = TarReader::new(archive, 100);
		let mut buf = [0u8; 64];
		let mut paths = Vec::new();
		while let Ok(Some(member)) = reader.next_member() {
			paths.push(member.path);
			while let Ok(1..) = reader.read_body(&mut buf) {}
		}
		paths
	};
	assert_eq!(walk(&archive), ["p/q.txt", "a/long/name", "s", "dir/"]);
	for (at, bit, mut damaged) in damaged_copies(&archive, 0..archive.len()) {
		let block = at / TAR_BLOCK * TAR_BLOCK;
		// a header whose checksum no longer matches is refused before it is parsed, so
		// the damaged header gets a matching checksum to reach its fields
		let header: &mut [u8; TAR_BLOCK] =
			(&mut damaged[block..block + TAR_BLOCK]).try_into().unwrap();
		if archive[block..].first_chunk().is_some_and(is_tar_header)
			&& !TAR_CHECKSUM.contains(&(at - block))
		{
			*header = tar_checksummed(*header);
		}
		std::panic::catch_unwind(|| walk(&damaged))
			.unwrap_or_else(|_| panic!("a flip of {bit:#x} at {at} panicked"));
	}
}

#[test]
fn non_utf8_paths_decode_as_latin1() {
	let mut archive = Vec::new();
	member(&mut archive, b"caf\xe9.txt", b'0', b"");
	let members = tar_members(&mut TarReader::new(archive.as_slice(), 100));
	assert_eq!(members[0].0.path, "café.txt");
	assert!(members[0].0.path_rewritten);
}
