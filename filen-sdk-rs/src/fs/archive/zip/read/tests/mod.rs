//! The reader against zips from the SDK's own writer and from the `zip` crate, and against zips
//! built wrong on purpose.

mod damage;
mod fixtures;
mod records;
mod zip64;

use std::{
	collections::BTreeMap,
	io::{Cursor, Write},
};

use ::zip::{
	AesMode, CompressionMethod, unstable::write::FileOptionsExt, write::SimpleFileOptions,
};
use chrono::TimeZone;

use super::*;
use crate::fs::archive::{
	test_support::{READ_BACK_ZIP, READ_BACK_ZIP_ENTRY, archive_password, damaged_copies, pattern},
	zip::write::{Encryption, ZipMethod, ZipWriter},
};

/// Every entry of `zip`, read through the reader: name, kind and data.
fn read_all(
	zip: &[u8],
	password: Option<&str>,
) -> Result<Vec<(String, ZipKind, Vec<u8>)>, ZipError> {
	read_source(&mut Cursor::new(zip), zip.len() as u64, password)
}

/// [`read_all`] of a zip that is the `len` bytes of `source`.
fn read_source<R: Read + Seek>(
	source: &mut R,
	len: u64,
	password: Option<&str>,
) -> Result<Vec<(String, ZipKind, Vec<u8>)>, ZipError> {
	let password = password.map(archive_password);
	let index = read_index(source, len, READ_BACK_ZIP)?;
	let mut out = Vec::new();
	for entry in &index.entries {
		let mut data = Vec::new();
		if entry.kind == ZipKind::File {
			open_entry(
				source,
				index.shift,
				entry,
				password.as_ref(),
				READ_BACK_ZIP_ENTRY,
			)?
			.read_to_end(&mut data)
			.map_err(|error| error.downcast().unwrap_or_else(ZipError::Read))?;
		}
		out.push((entry.name.clone(), entry.kind, data));
	}
	Ok(out)
}

fn ours(
	entries: &[(&str, Option<&[u8]>)],
	method: ZipMethod,
	password: Option<(&str, AesStrength)>,
) -> Vec<u8> {
	written_by(ZipWriter::new(Vec::new()), entries, method, password)
}

/// `entries` added to `writer`, and what it wrote.
fn written_by(
	mut writer: ZipWriter<Vec<u8>>,
	entries: &[(&str, Option<&[u8]>)],
	method: ZipMethod,
	password: Option<(&str, AesStrength)>,
) -> Vec<u8> {
	let when = Some(Utc.with_ymd_and_hms(2024, 5, 6, 7, 8, 10).unwrap());
	let password = password.map(|(password, strength)| (archive_password(password), strength));
	for (path, data) in entries {
		match data {
			None => writer.add_dir(path, when).unwrap(),
			Some(data) => {
				let encryption = password.as_ref().map(|(password, strength)| Encryption {
					password,
					strength: *strength,
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
				encryption.map(|strength| ("pw", strength)),
			);
			let read = read_all(&zip, encryption.map(|_| "pw")).unwrap();
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
			encryption.map(|strength| ("pw", strength)),
		);
		let mut archive = ::zip::ZipArchive::new(Cursor::new(&zip)).unwrap();
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
		let mut writer = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
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
		let read = read_all(&zip, aes.map(|_| "pw")).unwrap();
		assert_eq!(read, expected(&sample), "{method:?} {aes:?}");
	}
}

#[test]
fn we_read_the_zip_crates_zip_crypto() {
	// a second implementation of the key schedule: a mistake the reader shares with the tests'
	// own encryptor would cancel out
	let sample = sample();
	for method in [CompressionMethod::Stored, CompressionMethod::Deflated] {
		let mut writer = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
		for (path, data) in &sample {
			let options = SimpleFileOptions::default().compression_method(method);
			match data {
				None => writer.add_directory(path.as_str(), options).unwrap(),
				Some(data) => {
					let options = options.with_deprecated_encryption(b"pw").unwrap();
					writer.start_file(path.as_str(), options).unwrap();
					writer.write_all(data).unwrap();
				}
			}
		}
		let zip = writer.finish().unwrap().into_inner();
		let mut source = Cursor::new(&zip);
		let index = read_index(&mut source, zip.len() as u64, READ_BACK_ZIP).unwrap();
		assert!(
			index
				.entries
				.iter()
				.filter(|entry| entry.kind == ZipKind::File)
				.all(|entry| matches!(entry.encryption, ZipEncryption::ZipCrypto { .. })),
			"{method:?}"
		);
		assert_eq!(
			read_all(&zip, Some("pw")).unwrap(),
			expected(&sample),
			"{method:?}"
		);
	}
}

#[test]
fn passwords_are_asked_for_and_checked() {
	let zip = ours(
		&[("secret.txt", Some(&b"secret"[..]))],
		ZipMethod::Deflate { level: 6 },
		Some(("right", AesStrength::Aes256)),
	);
	assert!(matches!(
		read_all(&zip, None),
		Err(ZipError::PasswordRequired)
	));
	assert!(matches!(
		read_all(&zip, Some("wrong")),
		Err(ZipError::WrongPassword)
	));
	assert_eq!(read_all(&zip, Some("right")).unwrap()[0].2, b"secret");
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
	let exactly = ZipLimits {
		max_entries: 3,
		..READ_BACK_ZIP
	};
	assert_eq!(
		read_index(&mut source, zip.len() as u64, exactly)
			.unwrap()
			.entries
			.len(),
		3
	);
	let few = ZipLimits {
		max_entries: 2,
		..READ_BACK_ZIP
	};
	assert!(matches!(
		read_index(&mut source, zip.len() as u64, few),
		Err(ZipError::TooLarge(_))
	));
	let small = ZipLimits {
		max_index_bytes: 100,
		..READ_BACK_ZIP
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
	let index = read_index(&mut source, zip.len() as u64, READ_BACK_ZIP).unwrap();
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
	let index = read_index(&mut source, sfx.len() as u64, READ_BACK_ZIP).unwrap();
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
	let (at, records, end) = central_records(&zip);
	let swapped = with_central_records(&zip[..at], &[records[1], records[0]], end);
	assert_eq!(
		&swapped[at + CENTRAL_HEADER_LEN..][..6],
		b"second",
		"the central directory lists the second entry first"
	);
	let mut source = Cursor::new(&swapped);
	let index = read_index(&mut source, swapped.len() as u64, READ_BACK_ZIP).unwrap();
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
	let index = read_index(&mut source, overlapping.len() as u64, READ_BACK_ZIP).unwrap();
	assert_eq!(index.entries.len(), 1);
	assert_eq!(index.overlapping.len(), 1);
}

/// Where `zip`'s central directory starts, its records, and its end record. The zip has no
/// zip64 records or comment.
fn central_records(zip: &[u8]) -> (usize, Vec<&[u8]>, &[u8]) {
	let eocd = zip.len() - EOCD_LEN;
	assert_eq!(u32_at(zip, eocd), EOCD_SIG, "no zip64 or comment here");
	let cd_size = u32_at(zip, eocd + 12) as usize;
	let cd_start = u32_at(zip, eocd + 16) as usize;
	let mut records = Vec::new();
	let mut at = cd_start;
	while at < cd_start + cd_size {
		let len = CENTRAL_HEADER_LEN
			+ usize::from(u16_at(zip, at + 28))
			+ usize::from(u16_at(zip, at + 30))
			+ usize::from(u16_at(zip, at + 32));
		records.push(&zip[at..at + len]);
		at += len;
	}
	(cd_start, records, &zip[eocd..])
}

/// The zip whose entries are `entries`, with `records` as its central directory and `end` as
/// its end record, counts and size set to match.
fn with_central_records(entries: &[u8], records: &[&[u8]], end: &[u8]) -> Vec<u8> {
	let mut out = entries.to_vec();
	let size: usize = records.iter().map(|record| record.len()).sum();
	for record in records {
		out.extend_from_slice(record);
	}
	let mut end = end.to_vec();
	let count = u16::try_from(records.len()).unwrap();
	end[8..10].copy_from_slice(&count.to_le_bytes());
	end[10..12].copy_from_slice(&count.to_le_bytes());
	end[12..16].copy_from_slice(&u32::try_from(size).unwrap().to_le_bytes());
	out.extend_from_slice(&end);
	out
}

/// The MS-DOS version-made-by host, which the reader has no use for.
const HOST_DOS: u16 = 0;
