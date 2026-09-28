//! Zips built wrong on purpose: damaged data and records, entries stating the wrong size, and
//! bytes that belong to no entry.

use super::{
	fixtures::fixture,
	zip64::{PAST_4_GIB, Sparse},
	*,
};

#[test]
fn damage_is_caught() {
	let zip = ours(
		&[("a.txt", Some(&pattern(10_000, 2)[..]))],
		ZipMethod::Stored,
		None,
	);
	// a flipped data byte fails the CRC-32
	// past the 30-byte header, the name and the 9-byte timestamp field
	let mut flipped = zip.clone();
	flipped[30 + 5 + 9 + 100] ^= 1;
	assert!(matches!(
		read_all(&flipped, None),
		Err(ZipError::Corrupt("an entry's CRC-32 does not match"))
	));
	// a flipped byte of AES data fails the authentication code
	let zip = ours(
		&[("a.txt", Some(&pattern(10_000, 2)[..]))],
		ZipMethod::Stored,
		Some((&b"pw"[..], AesStrength::Aes128)),
	);
	let mut flipped = zip.clone();
	flipped[200] ^= 1;
	assert!(read_all(&flipped, Some(b"pw")).is_err());
	// no end record
	assert!(matches!(
		read_all(&zip[..zip.len() - 10], None),
		Err(ZipError::Corrupt(_))
	));
	assert!(matches!(read_all(b"PK", None), Err(ZipError::Corrupt(_))));
}

/// `zip` with its `drop`-th central directory record left out, as an in-place edit that
/// forgot an entry leaves it: the entry's local header and data stay where they were.
fn without_central_record(zip: &[u8], drop: usize) -> Vec<u8> {
	let eocd = zip.len() - 22;
	assert_eq!(u32_at(zip, eocd), EOCD_SIG, "no zip64 or comment here");
	let cd_size = u32_at(zip, eocd + 12) as usize;
	let cd_start = u32_at(zip, eocd + 16) as usize;
	let mut records = Vec::new();
	let mut at = cd_start;
	while at < cd_start + cd_size {
		let len = 46
			+ usize::from(u16_at(zip, at + 28))
			+ usize::from(u16_at(zip, at + 30))
			+ usize::from(u16_at(zip, at + 32));
		records.push(&zip[at..at + len]);
		at += len;
	}
	let kept: Vec<u8> = records
		.iter()
		.enumerate()
		.filter(|&(i, _)| i != drop)
		.flat_map(|(_, record)| record.iter().copied())
		.collect();
	let mut out = zip[..cd_start].to_vec();
	out.extend_from_slice(&kept);
	let mut end = zip[eocd..].to_vec();
	let count = u16::try_from(records.len() - 1).unwrap();
	end[8..10].copy_from_slice(&count.to_le_bytes());
	end[10..12].copy_from_slice(&count.to_le_bytes());
	end[12..16].copy_from_slice(&(u32::try_from(kept.len()).unwrap()).to_le_bytes());
	out.extend_from_slice(&end);
	out
}

#[test]
fn bytes_between_entries_belong_to_nothing() {
	let entries = [
		("a.txt", Some(&b"alpha"[..])),
		("hidden.bin", Some(&[7u8; 300][..])),
		("c.txt", Some(&b"gamma"[..])),
	];
	let zip = ours(&entries, ZipMethod::Stored, None);
	let unaccounted = |zip: &[u8]| {
		let mut source = Cursor::new(zip);
		let index = read_index(&mut source, zip.len() as u64, LIMITS).unwrap();
		index
			.entries
			.iter()
			.map(|entry| unaccounted_after(&mut source, index.shift, entry))
			.sum::<u64>()
	};
	// our entries end in data descriptors, which are no gap
	assert_eq!(unaccounted(&zip), 0);
	let hidden = without_central_record(&zip, 1);
	let gap = unaccounted(&hidden);
	// the forgotten entry's local header, name, data and descriptor
	assert!(gap >= 30 + 10 + 300, "{gap}");
	assert_eq!(
		read_all(&hidden, None)
			.unwrap()
			.into_iter()
			.map(|(name, ..)| name)
			.collect::<Vec<_>>(),
		["a.txt", "c.txt"]
	);
}

#[test]
fn an_end_record_signature_after_the_comment_is_not_taken_for_one() {
	let zip = ours(&[("a.txt", Some(&b"alpha"[..]))], ZipMethod::Stored, None);
	// trailing bytes holding what reads as an end record of one entry, whose directory would
	// start in the real end record
	let fake = [
		&EOCD_SIG.to_le_bytes()[..],
		&[0; 4],
		&1u16.to_le_bytes(),
		&1u16.to_le_bytes(),
		&u32::try_from(CENTRAL_HEADER_LEN).unwrap().to_le_bytes(),
		&0u32.to_le_bytes(),
		&0u16.to_le_bytes(),
		b"and more padding",
	]
	.concat();
	let mut padded = zip.clone();
	padded.extend_from_slice(&fake);
	let mut source = Cursor::new(&padded);
	let index = read_index(&mut source, padded.len() as u64, LIMITS).unwrap();
	assert_eq!(index.trailing_bytes, fake.len() as u64);
	assert_eq!(read_all(&padded, None).unwrap()[0].2, b"alpha");
}

#[test]
fn bytes_after_the_end_record_are_counted() {
	let zip = ours(&[("a.txt", Some(&b"alpha"[..]))], ZipMethod::Stored, None);
	let mut padded = zip.clone();
	padded.extend_from_slice(&[0; 100]);
	// a comment length that falls short of the comment leaves bytes after it too
	let mut short_comment = zip.clone();
	let comment = b"a comment, less its last 7 bytes";
	short_comment[zip.len() - 2..]
		.copy_from_slice(&u16::try_from(comment.len() - 7).unwrap().to_le_bytes());
	short_comment.extend_from_slice(comment);
	for (zip, trailing) in [(&zip, 0), (&padded, 100), (&short_comment, 7)] {
		let mut source = Cursor::new(zip);
		let index = read_index(&mut source, zip.len() as u64, LIMITS).unwrap();
		assert_eq!(index.trailing_bytes, trailing);
		assert_eq!(read_all(zip, None).unwrap()[0].2, b"alpha");
	}
}

/// `zip`, whose one entry's central record states `size` as its uncompressed size.
fn stating_size(zip: &[u8], size: u32) -> Vec<u8> {
	let record = zip
		.windows(4)
		.rposition(|w| w == CENTRAL_HEADER_SIG.to_le_bytes())
		.unwrap();
	let mut stating = zip.to_vec();
	stating[record + 24..record + 28].copy_from_slice(&size.to_le_bytes());
	stating
}

#[test]
fn an_entry_is_held_to_its_stated_size() {
	let data = pattern(10_000, 3);
	let zip = ours(
		&[("a.bin", Some(&data[..]))],
		ZipMethod::Deflate { level: 6 },
		None,
	);
	assert!(matches!(
		read_all(&stating_size(&zip, 5_000), None),
		Err(ZipError::Corrupt("an entry holds more data than it states"))
	));
	assert!(matches!(
		read_all(&stating_size(&zip, 20_000), None),
		Err(ZipError::Corrupt("an entry holds less data than it states"))
	));
}

#[test]
fn an_understated_bomb_stops_at_its_stated_size() {
	const STATED: u32 = 1024;
	let zeros = vec![0u8; 4 << 20];
	let zip = stating_size(
		&ours(
			&[("bomb.bin", Some(&zeros[..]))],
			ZipMethod::Deflate { level: 9 },
			None,
		),
		STATED,
	);
	let mut source = Cursor::new(&zip);
	let index = read_index(&mut source, zip.len() as u64, LIMITS).unwrap();
	let mut entry = open_entry(&mut source, index.shift, &index.entries[0], None, ENTRY).unwrap();
	let mut buf = [0u8; 4096];
	let mut delivered = 0;
	let error = loop {
		match entry.read(&mut buf) {
			Ok(0) => panic!("the bomb read to its end"),
			Ok(n) => delivered += n,
			Err(error) => break error,
		}
	};
	assert!(delivered <= STATED as usize, "{delivered}");
	assert!(matches!(
		error.get_ref().and_then(|e| e.downcast_ref()),
		Some(ZipError::Corrupt("an entry holds more data than it states"))
	));
}

#[test]
fn a_damaged_byte_never_panics() {
	let sample = sample();
	let small = &borrowed(&sample)[..2];
	// (bytes before the zip, never held in memory; the zip)
	let archives = [
		(0, ours(small, ZipMethod::Deflate { level: 6 }, None)),
		(
			0,
			ours(
				&small[1..],
				ZipMethod::Bzip2 { level: 1 },
				Some((&b"pw"[..], AesStrength::Aes128)),
			),
		),
		(
			PAST_4_GIB,
			written_by(
				ZipWriter::past(Vec::new(), PAST_4_GIB, 0),
				small,
				ZipMethod::Stored,
				None,
			),
		),
		(0, fixture!("finder-ditto.zip").1.to_vec()),
		(0, fixture!("zip64-infozip.zip").1.to_vec()),
		(0, fixture!("zipcrypto-infozip.zip").1.to_vec()),
		// the LZMA entries' own headers (version, properties, dictionary size) are damaged too
		(0, fixture!("lzma.zip").1.to_vec()),
		(0, fixture!("lzma-no-eos.zip").1.to_vec()),
		(0, fixture!("xz.zip").1.to_vec()),
	];
	for (skipped, zip) in archives {
		// every header, record, length and offset, and every entry's data, in turn
		// a byte set to 0xFF as well as flipped: a length or an offset at its largest
		let set = (0..zip.len()).map(|at| {
			let mut damaged = zip.clone();
			damaged[at] = 0xFF;
			(at, 0xFF, damaged)
		});
		for (at, _, damaged) in damaged_copies(&zip, 0..zip.len()).chain(set) {
			let mut source = Sparse::past(skipped, damaged.into());
			let len = source.len();
			let _ = std::panic::catch_unwind(move || read_source(&mut source, len, Some(b"pw")))
				.unwrap_or_else(|_| panic!("damage at {at} of a {len}-byte zip panicked"));
		}
	}
}
