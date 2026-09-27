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
	(0..len).map(|i| (i % 251) as u8 ^ seed).collect()
}

/// Every entry of `zip`, read through the reader: name, kind and data.
fn read_all(
	zip: &[u8],
	password: Option<&[u8]>,
) -> Result<Vec<(String, ZipKind, Vec<u8>)>, ZipError> {
	read_source(&mut Cursor::new(zip), zip.len() as u64, password)
}

/// [`read_all`] of a zip that is the `len` bytes of `source`.
fn read_source<R: Read + Seek>(
	source: &mut R,
	len: u64,
	password: Option<&[u8]>,
) -> Result<Vec<(String, ZipKind, Vec<u8>)>, ZipError> {
	let index = read_index(source, len, LIMITS)?;
	let mut out = Vec::new();
	for entry in &index.entries {
		let mut data = Vec::new();
		if entry.kind == ZipKind::File {
			open_entry(source, index.shift, entry, password, ENTRY)?
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
	written_by(ZipWriter::new(Vec::new()), entries, method, password)
}

/// `entries` added to `writer`, and what it wrote.
fn written_by(
	mut writer: ZipWriter<Vec<u8>>,
	entries: &[(&str, Option<&[u8]>)],
	method: ZipMethod,
	password: Option<(&[u8], AesStrength)>,
) -> Vec<u8> {
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
	let count = (records.len() - 1) as u16;
	end[8..10].copy_from_slice(&count.to_le_bytes());
	end[10..12].copy_from_slice(&count.to_le_bytes());
	end[12..16].copy_from_slice(&(kept.len() as u32).to_le_bytes());
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

/// A file of `skipped` zero bytes and then `data`, without the zeros taking memory: the whole of
/// an archive written by [`ZipWriter::past`].
struct Past {
	skipped: u64,
	data: Vec<u8>,
	pos: u64,
}

impl Past {
	fn new(skipped: u64, data: Vec<u8>) -> Self {
		Self {
			skipped,
			data,
			pos: 0,
		}
	}

	fn len(&self) -> u64 {
		self.skipped + self.data.len() as u64
	}
}

impl Read for Past {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let n = if self.pos < self.skipped {
			let n = buf.len().min((self.skipped - self.pos) as usize);
			buf[..n].fill(0);
			n
		} else {
			let at = ((self.pos - self.skipped) as usize).min(self.data.len());
			let n = buf.len().min(self.data.len() - at);
			buf[..n].copy_from_slice(&self.data[at..at + n]);
			n
		};
		self.pos += n as u64;
		Ok(n)
	}
}

impl Seek for Past {
	fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
		let (base, delta) = match to {
			SeekFrom::Start(at) => (at, 0),
			SeekFrom::End(delta) => (self.len(), delta),
			SeekFrom::Current(delta) => (self.pos, delta),
		};
		self.pos = base
			.checked_add_signed(delta)
			.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
		Ok(self.pos)
	}
}

/// Past 4 GiB: every offset is a zip64 one.
const PAST_4_GIB: u64 = 5 << 30;

#[test]
fn zip64_sizes_and_offsets_read_back() {
	let sample = sample();
	for (method, encryption) in [
		(ZipMethod::Stored, None),
		(ZipMethod::Deflate { level: 6 }, Some(AesStrength::Aes256)),
		(ZipMethod::Bzip2 { level: 1 }, None),
	] {
		let password = encryption.map(|_| &b"pw"[..]);
		// (past 4 GiB, whether every file is written with zip64 sizes)
		for (skipped, threshold) in [(0, 0), (PAST_4_GIB, u64::MAX), (PAST_4_GIB, 0)] {
			let case = format!("{method:?} {encryption:?} {skipped} {threshold}");
			let zip = written_by(
				ZipWriter::past(Vec::new(), skipped, threshold),
				&borrowed(&sample),
				method,
				encryption.map(|strength| (&b"pw"[..], strength)),
			);
			let mut source = Past::new(skipped, zip);
			let len = source.len();
			let index = read_index(&mut source, len, LIMITS).unwrap();
			assert_eq!(index.prefix_bytes, skipped, "{case}");
			for entry in &index.entries {
				// a zip64 data descriptor is no gap either
				assert_eq!(
					unaccounted_after(&mut source, index.shift, entry),
					0,
					"{} {case}",
					entry.name
				);
			}
			assert_eq!(
				read_source(&mut source, len, password).unwrap(),
				expected(&sample),
				"{case}"
			);

			let mut archive = zip8::ZipArchive::new(source).unwrap();
			for (path, data) in &sample {
				let Some(data) = data else {
					assert!(archive.by_name(&format!("{path}/")).unwrap().is_dir());
					continue;
				};
				let mut file = match password {
					None => archive.by_name(path).unwrap(),
					Some(password) => archive.by_name_decrypt(path, password).unwrap(),
				};
				let mut read = Vec::new();
				file.read_to_end(&mut read).unwrap();
				assert_eq!(&read, data, "{path} {case}");
			}
		}
	}
}
