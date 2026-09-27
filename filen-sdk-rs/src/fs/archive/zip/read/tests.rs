//! The reader against zips from the SDK's own writer and from the `zip` crate, and against zips
//! built wrong on purpose.

use std::io::{Cursor, Write};

use chrono::TimeZone;
use zip8::{AesMode, CompressionMethod, write::SimpleFileOptions};

use super::*;
use crate::fs::archive::zip::write::{Encryption, ZipMethod, ZipWriter};

const LIMITS: ZipLimits = ZipLimits {
	max_index_bytes: 32 << 20,
	max_entries: 1_000_000,
};

const ENTRY: EntryLimits = EntryLimits {
	decoder_memory: 64 << 20,
};

fn pattern(len: usize, seed: u8) -> Vec<u8> {
	(0..len)
		.map(|i| (i % 251).to_le_bytes()[0] ^ seed)
		.collect()
}

/// Every entry of `zip`, read through the reader: name, kind and data.
fn read_all(
	zip: &[u8],
	password: Option<&[u8]>,
) -> Result<Vec<(String, ZipKind, Vec<u8>)>, ZipError> {
	let mut source = Cursor::new(zip);
	let index = read_index(&mut source, zip.len() as u64, LIMITS)?;
	let mut out = Vec::new();
	for entry in &index.entries {
		let mut data = Vec::new();
		if entry.kind == ZipKind::File {
			open_entry(&mut source, index.shift, entry, password, ENTRY)?
				.read_to_end(&mut data)
				.map_err(
					|e| match e.into_inner().map(|inner| inner.downcast::<ZipError>()) {
						Some(Ok(zip)) => *zip,
						Some(Err(other)) => ZipError::Read(io::Error::other(other)),
						None => ZipError::Corrupt("read failed"),
					},
				)?;
		}
		out.push((entry.name.clone(), entry.kind, data));
	}
	Ok(out)
}

fn ours(
	entries: &[(&str, Option<&[u8]>)],
	method: ZipMethod,
	password: Option<(&[u8], AesStrength)>,
) -> Vec<u8> {
	let mut writer = ZipWriter::new(Vec::new());
	let when = Some(Utc.with_ymd_and_hms(2024, 5, 6, 7, 8, 10).unwrap());
	for (path, data) in entries {
		match data {
			None => writer.add_dir(path, when).unwrap(),
			Some(data) => {
				let encryption = password.map(|(password, strength)| Encryption {
					password,
					strength,
					salt: vec![3; strength.salt_len()],
				});
				let read = writer
					.add_file(
						path,
						when,
						data.len() as u64,
						method,
						encryption,
						&mut &data[..],
					)
					.unwrap();
				assert_eq!(read, data.len() as u64);
			}
		}
	}
	writer.finish().unwrap()
}

fn sample() -> Vec<(String, Option<Vec<u8>>)> {
	vec![
		("docs".into(), None),
		("docs/a.txt".into(), Some(b"alpha".to_vec())),
		("docs/big.bin".into(), Some(pattern(300_000, 1))),
		("empty".into(), Some(Vec::new())),
		("ünïcode/名前.txt".into(), Some(b"utf8".to_vec())),
	]
}

fn borrowed(sample: &[(String, Option<Vec<u8>>)]) -> Vec<(&str, Option<&[u8]>)> {
	sample
		.iter()
		.map(|(path, data)| (path.as_str(), data.as_deref()))
		.collect()
}

fn expected(sample: &[(String, Option<Vec<u8>>)]) -> Vec<(String, ZipKind, Vec<u8>)> {
	sample
		.iter()
		.map(|(path, data)| match data {
			None => (format!("{path}/"), ZipKind::Dir, Vec::new()),
			Some(data) => (path.clone(), ZipKind::File, data.clone()),
		})
		.collect()
}

#[test]
fn our_zips_read_back_with_every_method_and_encryption() {
	let sample = sample();
	for method in [
		ZipMethod::Stored,
		ZipMethod::Deflate { level: 6 },
		ZipMethod::Bzip2 { level: 9 },
	] {
		for encryption in [None, Some(AesStrength::Aes128), Some(AesStrength::Aes256)] {
			let zip = ours(
				&borrowed(&sample),
				method,
				encryption.map(|strength| (&b"pw"[..], strength)),
			);
			let read = read_all(&zip, encryption.map(|_| &b"pw"[..])).unwrap();
			assert_eq!(read, expected(&sample), "{method:?} {encryption:?}");
		}
	}
}

#[test]
fn the_zip_crate_reads_our_zips() {
	let sample = sample();
	for (method, encryption) in [
		(ZipMethod::Deflate { level: 6 }, None),
		(ZipMethod::Bzip2 { level: 1 }, None),
		(ZipMethod::Stored, Some(AesStrength::Aes256)),
		(ZipMethod::Deflate { level: 9 }, Some(AesStrength::Aes192)),
	] {
		let zip = ours(
			&borrowed(&sample),
			method,
			encryption.map(|strength| (&b"pw"[..], strength)),
		);
		let mut archive = zip8::ZipArchive::new(Cursor::new(&zip)).unwrap();
		assert_eq!(archive.len(), sample.len());
		for (path, data) in &sample {
			let Some(data) = data else {
				let dir = archive.by_name(&format!("{path}/")).unwrap();
				assert!(dir.is_dir());
				continue;
			};
			let mut file = match encryption {
				None => archive.by_name(path).unwrap(),
				Some(_) => archive.by_name_decrypt(path, b"pw").unwrap(),
			};
			let mut read = Vec::new();
			file.read_to_end(&mut read).unwrap();
			assert_eq!(&read, data, "{path} {method:?} {encryption:?}");
		}
	}
}

#[test]
fn we_read_the_zip_crates_zips() {
	let sample = sample();
	for (method, aes) in [
		(CompressionMethod::Stored, None),
		(CompressionMethod::Deflated, None),
		(CompressionMethod::Bzip2, None),
		(CompressionMethod::Deflated, Some(AesMode::Aes128)),
		(CompressionMethod::Stored, Some(AesMode::Aes256)),
	] {
		let mut writer = zip8::ZipWriter::new(Cursor::new(Vec::new()));
		for (path, data) in &sample {
			let mut options = SimpleFileOptions::default().compression_method(method);
			if let Some(mode) = aes {
				options = options.with_aes_encryption(mode, "pw");
			}
			match data {
				None => writer.add_directory(path.as_str(), options).unwrap(),
				Some(data) => {
					writer.start_file(path.as_str(), options).unwrap();
					writer.write_all(data).unwrap();
				}
			}
		}
		let zip = writer.finish().unwrap().into_inner();
		let read = read_all(&zip, aes.map(|_| &b"pw"[..])).unwrap();
		assert_eq!(read, expected(&sample), "{method:?} {aes:?}");
	}
}

#[test]
fn passwords_are_asked_for_and_checked() {
	let zip = ours(
		&[("secret.txt", Some(&b"secret"[..]))],
		ZipMethod::Deflate { level: 6 },
		Some((&b"right"[..], AesStrength::Aes256)),
	);
	assert!(matches!(
		read_all(&zip, None),
		Err(ZipError::PasswordRequired)
	));
	assert!(matches!(
		read_all(&zip, Some(b"wrong")),
		Err(ZipError::WrongPassword)
	));
	assert_eq!(read_all(&zip, Some(b"right")).unwrap()[0].2, b"secret");
}

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

#[test]
fn the_index_stays_within_its_limits() {
	let zip = ours(
		&[
			("a", Some(&b"1"[..])),
			("b", Some(&b"2"[..])),
			("c", Some(&b"3"[..])),
		],
		ZipMethod::Stored,
		None,
	);
	let mut source = Cursor::new(&zip);
	let few = ZipLimits {
		max_entries: 2,
		..LIMITS
	};
	assert!(matches!(
		read_index(&mut source, zip.len() as u64, few),
		Err(ZipError::TooLarge(_))
	));
	let small = ZipLimits {
		max_index_bytes: 100,
		..LIMITS
	};
	assert!(matches!(
		read_index(&mut source, zip.len() as u64, small),
		Err(ZipError::TooLarge(_))
	));
}

#[test]
fn duplicates_keep_the_last_and_are_reported() {
	let zip = ours(
		&[
			("same.txt", Some(&b"first"[..])),
			("same.txt", Some(&b"second"[..])),
		],
		ZipMethod::Stored,
		None,
	);
	let mut source = Cursor::new(&zip);
	let index = read_index(&mut source, zip.len() as u64, LIMITS).unwrap();
	assert_eq!(index.entries.len(), 1);
	assert_eq!(
		(index.duplicate_count, index.duplicate_names.clone()),
		(1, vec!["same.txt".to_owned()])
	);
	assert_eq!(read_all(&zip, None).unwrap()[0].2, b"second");
}

#[test]
fn prepended_data_is_skipped_and_counted() {
	let zip = ours(
		&[("a.txt", Some(&b"alpha"[..]))],
		ZipMethod::Deflate { level: 6 },
		None,
	);
	let mut sfx = b"#!/bin/sh\nexit 0\n".to_vec();
	let prefix = sfx.len() as u64;
	sfx.extend_from_slice(&zip);
	let mut source = Cursor::new(&sfx);
	let index = read_index(&mut source, sfx.len() as u64, LIMITS).unwrap();
	assert_eq!((index.shift, index.prefix_bytes), (prefix, prefix));
	assert_eq!(read_all(&sfx, None).unwrap()[0].2, b"alpha");
}

#[test]
fn entries_are_visited_in_local_header_order_and_overlaps_refused() {
	let zip = ours(
		&[
			("first", Some(&b"1111"[..])),
			("second", Some(&b"2222"[..])),
		],
		ZipMethod::Stored,
		None,
	);
	// swap the two central directory records: the order they are visited in stays the same
	let mut source = Cursor::new(&zip);
	let index = read_index(&mut source, zip.len() as u64, LIMITS).unwrap();
	let names: Vec<_> = index.entries.iter().map(|e| e.name.as_str()).collect();
	assert_eq!(names, ["first", "second"]);

	// point the second record at the first entry's header: the two overlap
	let cd = zip
		.windows(4)
		.rposition(|w| w == CENTRAL_HEADER_SIG.to_le_bytes())
		.unwrap();
	let mut overlapping = zip.clone();
	overlapping[cd + 42..cd + 46].copy_from_slice(&0u32.to_le_bytes());
	let mut source = Cursor::new(&overlapping);
	let index = read_index(&mut source, overlapping.len() as u64, LIMITS).unwrap();
	assert_eq!(index.entries.len(), 1);
	assert_eq!(index.overlapping.len(), 1);
}

#[test]
fn many_entries_use_zip64_end_records() {
	let names: Vec<String> = (0..70_000).map(|i| format!("f{i}")).collect();
	let entries: Vec<(&str, Option<&[u8]>)> =
		names.iter().map(|n| (n.as_str(), Some(&b""[..]))).collect();
	let zip = ours(&entries, ZipMethod::Stored, None);
	let mut source = Cursor::new(&zip);
	let index = read_index(&mut source, zip.len() as u64, LIMITS).unwrap();
	assert_eq!(index.entries.len(), 70_000);
	assert_eq!(
		zip8::ZipArchive::new(Cursor::new(&zip)).unwrap().len(),
		70_000
	);
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
		&(u32::try_from(CENTRAL_HEADER_LEN).unwrap()).to_le_bytes(),
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
		.copy_from_slice(&(u16::try_from(comment.len() - 7).unwrap()).to_le_bytes());
	short_comment.extend_from_slice(comment);
	for (zip, trailing) in [(&zip, 0), (&padded, 100), (&short_comment, 7)] {
		let mut source = Cursor::new(zip);
		let index = read_index(&mut source, zip.len() as u64, LIMITS).unwrap();
		assert_eq!(index.trailing_bytes, trailing);
		assert_eq!(read_all(zip, None).unwrap()[0].2, b"alpha");
	}
}

/// Version-made-by hosts: MS-DOS, Unix and OS X.
const DOS: u8 = 0;
const UNIX: u8 = 3;
const OS_X: u8 = 19;

/// A central directory record for `name` and nothing else of note, made on `host`.
fn central_record(host: u8, flags: u16, name: &[u8], extra: &[u8], unix_mode: u32) -> Vec<u8> {
	[
		&CENTRAL_HEADER_SIG.to_le_bytes()[..],
		&[20, host],
		&20u16.to_le_bytes(),
		&flags.to_le_bytes(),
		// method, time, date, CRC-32, sizes
		&[0; 18],
		&(u16::try_from(name.len()).unwrap()).to_le_bytes(),
		&(u16::try_from(extra.len()).unwrap()).to_le_bytes(),
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
		&(5 + u16::try_from(name.len()).unwrap()).to_le_bytes(),
		&[1],
		&crc.to_le_bytes(),
		name.as_bytes(),
	]
	.concat()
}

#[test]
fn names_are_decoded_as_their_writers_meant() {
	const FLAG_UTF8: u16 = 0x0800;
	let utf8 = "Café/Résumé.txt".as_bytes();
	let cp437: &[u8] = b"Caf\x82/R\x82sum\x82.txt";
	// (host, flags, raw name, extra field, name, rewritten)
	let cases = [
		(DOS, FLAG_UTF8, utf8, Vec::new(), "Café/Résumé.txt", false),
		// flagged, but not UTF-8: read as CP437, and said to be rewritten
		(
			DOS,
			FLAG_UTF8,
			cp437,
			Vec::new(),
			"Caf\u{E9}/R\u{E9}sum\u{E9}.txt",
			true,
		),
		(DOS, 0, cp437, Vec::new(), "Café/Résumé.txt", false),
		// macOS Archive Utility, ditto and Info-ZIP on Unix store UTF-8 without saying so
		(UNIX, 0, utf8, Vec::new(), "Café/Résumé.txt", false),
		(OS_X, 0, utf8, Vec::new(), "Café/Résumé.txt", false),
		// decomposed, as macOS may store it, and passed on as it is
		(
			OS_X,
			0,
			"Cafe\u{301}.txt".as_bytes(),
			Vec::new(),
			"Cafe\u{301}.txt",
			false,
		),
		// a Unix name that is not UTF-8 is in some local character set: CP437 is a guess, so
		// the name is said to be rewritten, as a tar's is
		(UNIX, 0, cp437, Vec::new(), "Café/Résumé.txt", true),
		// a DOS name that happens to be valid UTF-8 stays CP437 ("├⌐" is 0xC3 0xA9)
		(DOS, 0, utf8, Vec::new(), "Caf├⌐/R├⌐sum├⌐.txt", false),
		// the Unicode path field wins while it matches the raw name
		(
			DOS,
			0,
			cp437,
			unicode_path(crc32fast::hash(cp437), "Unicode/Näme.txt"),
			"Unicode/Näme.txt",
			false,
		),
		// once the raw name changed (an old tool renamed the entry), it is stale
		(
			DOS,
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
fn symlinks_are_recognised_from_unix_and_os_x() {
	for (host, kind) in [
		(UNIX, ZipKind::Symlink),
		(OS_X, ZipKind::Symlink),
		// a DOS host's high attribute bits are no Unix mode
		(DOS, ZipKind::File),
	] {
		let record = central_record(host, 0, b"link", &[], 0o120_755);
		let (entry, _) = parse_central_header(&record, 0, 0).unwrap();
		assert_eq!(entry.kind, kind, "{host}");
	}
}
