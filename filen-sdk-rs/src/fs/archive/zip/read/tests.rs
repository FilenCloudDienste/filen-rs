//! The reader against zips from the SDK's own writer and from the `zip` crate, and against zips
//! built wrong on purpose.

use std::{
	collections::BTreeMap,
	io::{Cursor, Write},
};

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
fn we_read_the_zip_crates_zip_crypto() {
	use zip8::unstable::write::FileOptionsExt;
	// a second implementation of the key schedule: a mistake the reader shares with the tests'
	// own encryptor would cancel out
	let sample = sample();
	for method in [CompressionMethod::Stored, CompressionMethod::Deflated] {
		let mut writer = zip8::ZipWriter::new(Cursor::new(Vec::new()));
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
		let index = read_index(&mut source, zip.len() as u64, LIMITS).unwrap();
		assert!(
			index
				.entries
				.iter()
				.filter(|entry| entry.kind == ZipKind::File)
				.all(|entry| matches!(entry.encryption, ZipEncryption::ZipCrypto { .. })),
			"{method:?}"
		);
		assert_eq!(
			read_all(&zip, Some(b"pw")).unwrap(),
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

/// A part of a [`Sparse`] file.
enum Run {
	Zeros(u64),
	Bytes(Vec<u8>),
}

impl Run {
	fn len(&self) -> u64 {
		match self {
			Self::Zeros(len) => *len,
			Self::Bytes(bytes) => bytes.len() as u64,
		}
	}
}

/// A file whose runs of zeros take no memory, written front to back and read anywhere: an
/// archive past 4 GiB, or one holding an entry of more than 4 GiB of zeros.
#[derive(Default)]
struct Sparse {
	runs: Vec<Run>,
	pos: u64,
}

impl Sparse {
	/// `skipped` zeros and then `data`: the whole of an archive written by [`ZipWriter::past`].
	fn past(skipped: u64, data: Sparse) -> Self {
		Self {
			runs: [Run::Zeros(skipped)].into_iter().chain(data.runs).collect(),
			pos: 0,
		}
	}

	fn len(&self) -> u64 {
		self.runs.iter().map(Run::len).sum()
	}
}

impl From<Vec<u8>> for Sparse {
	fn from(data: Vec<u8>) -> Self {
		Self {
			runs: vec![Run::Bytes(data)],
			pos: 0,
		}
	}
}

impl Write for Sparse {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		match (self.runs.last_mut(), buf.iter().all(|&b| b == 0)) {
			(Some(Run::Zeros(len)), true) => *len += buf.len() as u64,
			(Some(Run::Bytes(bytes)), false) => bytes.extend_from_slice(buf),
			(_, true) => self.runs.push(Run::Zeros(buf.len() as u64)),
			(_, false) => self.runs.push(Run::Bytes(buf.to_vec())),
		}
		Ok(buf.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

impl Read for Sparse {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let mut start = 0;
		for run in &self.runs {
			let end = start + run.len();
			if self.pos < end {
				let at = self.pos - start;
				let n = buf.len().min((end - self.pos) as usize);
				match run {
					Run::Zeros(_) => buf[..n].fill(0),
					Run::Bytes(bytes) => {
						buf[..n].copy_from_slice(&bytes[at as usize..at as usize + n]);
					}
				}
				self.pos += n as u64;
				return Ok(n);
			}
			start = end;
		}
		Ok(0)
	}
}

impl Seek for Sparse {
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

/// What `entry`'s local header and data descriptor say of it, read from the bytes.
#[derive(Debug, PartialEq, Eq)]
struct LocalRecords {
	/// The local header's compressed and uncompressed sizes.
	sizes: (u32, u32),
	/// The values of its zip64 extra field, if it has one.
	zip64: Option<Vec<u64>>,
	/// The descriptor's compressed and uncompressed sizes, 8 bytes each with zip64, else 4.
	descriptor: (u64, u64),
}

fn local_records<R: Read + Seek>(source: &mut R, shift: u64, entry: &ZipEntry) -> LocalRecords {
	let header = read_at(
		source,
		shift + entry.header_offset,
		LOCAL_HEADER_LEN as usize,
	)
	.unwrap();
	let (name_len, extra_len) = (
		usize::from(u16_at(&header, 26)),
		usize::from(u16_at(&header, 28)),
	);
	let extra = read_at(
		source,
		shift + entry.header_offset + LOCAL_HEADER_LEN + name_len as u64,
		extra_len,
	)
	.unwrap();
	let mut zip64 = None;
	let mut fields = &extra[..];
	while fields.len() >= 4 {
		let len = usize::from(u16_at(fields, 2));
		if u16_at(fields, 0) == 0x0001 {
			zip64 = Some(
				fields[4..4 + len]
					.chunks_exact(8)
					.map(|value| u64_at(value, 0))
					.collect(),
			);
		}
		fields = &fields[4 + len..];
	}
	let wide = zip64.is_some();
	let descriptor = read_at(
		source,
		shift
			+ entry.header_offset
			+ LOCAL_HEADER_LEN
			+ (name_len + extra_len) as u64
			+ entry.compressed_size,
		if wide { 24 } else { 16 },
	)
	.unwrap();
	assert_eq!(u32_at(&descriptor, 0), 0x0807_4b50, "{}", entry.name);
	assert_eq!(u32_at(&descriptor, 4), entry.crc, "{}", entry.name);
	LocalRecords {
		sizes: (u32_at(&header, 18), u32_at(&header, 22)),
		zip64,
		descriptor: if wide {
			(u64_at(&descriptor, 8), u64_at(&descriptor, 16))
		} else {
			(
				u64::from(u32_at(&descriptor, 8)),
				u64::from(u32_at(&descriptor, 12)),
			)
		},
	}
}

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
			let mut source = Sparse::past(skipped, zip.into());
			let len = source.len();
			let index = read_index(&mut source, len, LIMITS).unwrap();
			assert_eq!(index.prefix_bytes, skipped, "{case}");
			for entry in index.entries.iter().filter(|e| e.kind == ZipKind::File) {
				// the local header leaves the sizes to the descriptor: zip64 ones when it has
				// the zip64 field, whose two values it cannot know yet
				let zip64 = threshold == 0;
				assert_eq!(
					local_records(&mut source, index.shift, entry),
					LocalRecords {
						sizes: if zip64 { (u32::MAX, u32::MAX) } else { (0, 0) },
						zip64: zip64.then(|| vec![0, 0]),
						descriptor: (entry.compressed_size, entry.size),
					},
					"{} {case}",
					entry.name
				);
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

#[test]
fn an_entry_of_over_4_gib_reads_back() {
	use crate::fs::archive::zip::write::ZIP64_ENTRY_THRESHOLD;
	const BIG: u64 = (4 << 30) + 1;
	// past 4 GiB, so the central record's zip64 field holds all three values
	let mut writer = ZipWriter::past(Sparse::default(), PAST_4_GIB, ZIP64_ENTRY_THRESHOLD);
	let when = Some(Utc.with_ymd_and_hms(2024, 5, 6, 7, 8, 10).unwrap());
	for (path, size) in [("before.txt", 6), ("big.bin", BIG), ("after.txt", 5)] {
		let mut data = io::repeat(b'z')
			.take(size.min(6))
			.chain(io::repeat(0).take(size.saturating_sub(6)));
		writer
			.add_file(path, when, size, ZipMethod::Stored, None, &mut data)
			.unwrap();
	}
	let mut source = Sparse::past(PAST_4_GIB, writer.finish().unwrap());
	let len = source.len();
	let index = read_index(&mut source, len, LIMITS).unwrap();
	let big = &index.entries[1];
	assert_eq!(
		(big.name.as_str(), big.size, big.compressed_size),
		("big.bin", BIG, BIG)
	);
	assert!(big.header_offset > PAST_4_GIB);
	assert_eq!(
		local_records(&mut source, index.shift, big),
		LocalRecords {
			sizes: (u32::MAX, u32::MAX),
			zip64: Some(vec![0, 0]),
			descriptor: (BIG, BIG),
		}
	);
	let mut read = 0u64;
	let mut buf = vec![0u8; 1 << 20];
	let mut entry = open_entry(&mut source, index.shift, big, None, ENTRY).unwrap();
	loop {
		let n = entry.read(&mut buf).unwrap();
		if n == 0 {
			break;
		}
		read += n as u64;
	}
	drop(entry);
	assert_eq!(read, BIG);

	let mut archive = zip8::ZipArchive::new(source).unwrap();
	let file = archive.by_name("big.bin").unwrap();
	assert_eq!(
		(file.size(), file.compressed_size(), file.header_start()),
		(BIG, BIG, big.header_offset)
	);
	drop(file);
	let mut after = Vec::new();
	archive
		.by_name("after.txt")
		.unwrap()
		.read_to_end(&mut after)
		.unwrap();
	assert_eq!(after, b"zzzzz");
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
		&(CENTRAL_HEADER_LEN as u32).to_le_bytes(),
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
	short_comment[zip.len() - 2..].copy_from_slice(&((comment.len() - 7) as u16).to_le_bytes());
	short_comment.extend_from_slice(comment);
	for (zip, trailing) in [(&zip, 0), (&padded, 100), (&short_comment, 7)] {
		let mut source = Cursor::new(zip);
		let index = read_index(&mut source, zip.len() as u64, LIMITS).unwrap();
		assert_eq!(index.trailing_bytes, trailing);
		assert_eq!(read_all(zip, None).unwrap()[0].2, b"alpha");
	}
}

/// The MS-DOS version-made-by host, which the reader has no use for.
const HOST_DOS: u16 = 0;

/// A central directory record for `name` and nothing else of note, made on `host`.
fn central_record(host: u16, flags: u16, name: &[u8], extra: &[u8], unix_mode: u32) -> Vec<u8> {
	[
		&CENTRAL_HEADER_SIG.to_le_bytes()[..],
		&((host << 8) | 20).to_le_bytes(),
		&20u16.to_le_bytes(),
		&flags.to_le_bytes(),
		// method, time, date, CRC-32, sizes
		&[0; 18],
		&(name.len() as u16).to_le_bytes(),
		&(extra.len() as u16).to_le_bytes(),
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
		&(5 + name.len() as u16).to_le_bytes(),
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
			&(values.len() as u16 * 8).to_le_bytes(),
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

/// A zip from `tests/fixtures/archives/zip`, made by a real tool (see its README).
macro_rules! fixture {
	($name:literal) => {
		(
			$name,
			&include_bytes!(concat!(
				env!("CARGO_MANIFEST_DIR"),
				"/tests/fixtures/archives/zip/",
				$name
			))[..],
		)
	};
}

/// The README's `noise`: bytes from a linear congruential generator.
fn noise(mut seed: u32, len: usize) -> Vec<u8> {
	(0..len)
		.map(|_| {
			seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0x7FFF_FFFF;
			(seed >> 16) as u8
		})
		.collect()
}

/// The files of the README's `src/` tree, which most fixtures hold.
fn fixture_tree() -> BTreeMap<String, Vec<u8>> {
	let far = [noise(1, 1000), vec![0; 34_000], noise(1, 1000)].concat();
	let lines = (0..500).map(|i| format!("line {i}\n")).collect::<String>();
	BTreeMap::from([
		(
			"hello.txt".to_owned(),
			b"hello from a real zip tool\n".to_vec(),
		),
		("sub/far.bin".to_owned(), far),
		("sub/lines.txt".to_owned(), lines.into_bytes()),
	])
}

/// The files of a zip of the README's `src/` tree, which holds no directory but `sub/`.
fn files_of_tree(read: Vec<(String, ZipKind, Vec<u8>)>) -> BTreeMap<String, Vec<u8>> {
	read.into_iter()
		.filter_map(|(name, kind, data)| match kind {
			ZipKind::File => Some((name, data)),
			ZipKind::Dir => {
				assert_eq!(name, "sub/");
				None
			}
			ZipKind::Symlink => panic!("a symlink {name}"),
		})
		.collect()
}

#[test]
fn real_tools_zips_read_back() {
	for (name, zip) in [
		fixture!("deflate64.zip"),
		fixture!("bzip2.zip"),
		fixture!("lzma.zip"),
		fixture!("lzma-no-eos.zip"),
		fixture!("xz.zip"),
		fixture!("descriptor-infozip.zip"),
	] {
		let read = read_all(zip, None).unwrap_or_else(|e| panic!("{name}: {e}"));
		assert_eq!(files_of_tree(read), fixture_tree(), "{name}");
	}
}

/// How a fixture's files are encrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protection {
	/// ZipCrypto; with a data descriptor, the check byte comes from the time, not the CRC-32.
	ZipCrypto { descriptor: bool },
	/// WinZip AES; AE-2 stores no CRC-32.
	Aes { strength: AesStrength, ae2: bool },
}

impl Protection {
	fn of(entry: &ZipEntry) -> Option<Self> {
		match entry.encryption {
			ZipEncryption::None => None,
			ZipEncryption::ZipCrypto { .. } => Some(Self::ZipCrypto {
				descriptor: entry.has_descriptor,
			}),
			ZipEncryption::Aes {
				strength,
				authenticated_only,
			} => Some(Self::Aes {
				strength,
				ae2: authenticated_only,
			}),
		}
	}
}

#[test]
fn real_tools_encrypted_zips_read_back() {
	use AesStrength::{Aes128, Aes256};
	for ((name, zip), protection) in [
		(
			fixture!("zipcrypto-infozip.zip"),
			Protection::ZipCrypto { descriptor: true },
		),
		(
			fixture!("zipcrypto-bsdtar.zip"),
			Protection::ZipCrypto { descriptor: true },
		),
		(
			fixture!("zipcrypto-7zip.zip"),
			Protection::ZipCrypto { descriptor: false },
		),
		(
			fixture!("aes128-bsdtar.zip"),
			Protection::Aes {
				strength: Aes128,
				ae2: false,
			},
		),
		(
			fixture!("aes256-bsdtar.zip"),
			Protection::Aes {
				strength: Aes256,
				ae2: false,
			},
		),
		(
			fixture!("aes128-7zip.zip"),
			Protection::Aes {
				strength: Aes128,
				ae2: true,
			},
		),
		(
			fixture!("aes256-7zip.zip"),
			Protection::Aes {
				strength: Aes256,
				ae2: true,
			},
		),
	] {
		let mut source = Cursor::new(zip);
		let index = read_index(&mut source, zip.len() as u64, LIMITS).unwrap();
		for entry in index.entries.iter().filter(|e| e.kind == ZipKind::File) {
			assert_eq!(
				Protection::of(entry),
				Some(protection),
				"{name} {}",
				entry.name
			);
		}
		let read = read_all(zip, Some(b"pw")).unwrap_or_else(|e| panic!("{name}: {e}"));
		assert_eq!(files_of_tree(read), fixture_tree(), "{name}");
		assert!(
			matches!(read_all(zip, None), Err(ZipError::PasswordRequired)),
			"{name}"
		);
		assert!(
			matches!(read_all(zip, Some(b"nope")), Err(ZipError::WrongPassword)),
			"{name}"
		);
	}
}

#[test]
fn a_ppmd_entry_is_unsupported() {
	let (_, zip) = fixture!("ppmd.zip");
	assert!(matches!(
		read_all(zip, None),
		Err(ZipError::Unsupported("a compression method"))
	));
}

#[test]
fn a_finder_zip_keeps_its_unflagged_utf8_names() {
	let (_, zip) = fixture!("finder-ditto.zip");
	// in the order of their local headers, AppleDouble files left out
	let read: Vec<_> = read_all(zip, None)
		.unwrap()
		.into_iter()
		.filter(|(name, ..)| !name.starts_with("__MACOSX/"))
		.collect();
	let file = |name: &str, data: &[u8]| (name.to_owned(), ZipKind::File, data.to_vec());
	let other = |name: &str, kind| (name.to_owned(), kind, Vec::new());
	assert_eq!(
		read,
		[
			other("finder/", ZipKind::Dir),
			file("finder/naïve.txt", b"naive\n"),
			other("finder/link", ZipKind::Symlink),
			other("finder/Café/", ZipKind::Dir),
			file("finder/Café/Résumé.txt", b"bonjour\n"),
		]
	);
}

#[test]
fn info_zips_zip64_sizes_read_back() {
	let (_, zip) = fixture!("zip64-infozip.zip");
	let lines = fixture_tree().remove("sub/lines.txt").unwrap();
	// read from stdin, as "-": its size is only in the zip64 fields
	assert_eq!(
		read_all(zip, None).unwrap(),
		[("-".to_owned(), ZipKind::File, lines)]
	);
}

#[test]
fn a_comment_with_bytes_after_it_is_read_past() {
	let (_, zip) = fixture!("comment-trailing.zip");
	let mut source = Cursor::new(zip);
	let index = read_index(&mut source, zip.len() as u64, LIMITS).unwrap();
	assert_eq!(
		index.trailing_bytes,
		b"trailing bytes after the comment\n".len() as u64
	);
	assert_eq!(
		read_all(zip, None).unwrap(),
		[(
			"hello.txt".to_owned(),
			ZipKind::File,
			b"hello from a real zip tool\n".to_vec()
		)]
	);
}

#[test]
fn real_tools_symlinks_are_recognised() {
	let (_, infozip) = fixture!("symlink-infozip.zip");
	let (_, sevenzip) = fixture!("symlink-7zip.zip");
	// the same zip, as made on each version-made-by host
	let on_host = |host: u16| {
		let mut zip = infozip.to_vec();
		let mut at = 0;
		while let Some(found) = zip[at..]
			.windows(4)
			.position(|w| w == CENTRAL_HEADER_SIG.to_le_bytes())
		{
			at += found;
			zip[at + 5] = host as u8;
			at += 4;
		}
		zip
	};
	for (zip, link) in [
		(infozip.to_vec(), ZipKind::Symlink),
		(sevenzip.to_vec(), ZipKind::Symlink),
		(on_host(HOST_OS_X), ZipKind::Symlink),
		(on_host(HOST_DOS), ZipKind::File),
	] {
		let kinds: Vec<_> = read_all(&zip, None)
			.unwrap()
			.into_iter()
			.map(|(name, kind, _)| (name, kind))
			.collect();
		assert_eq!(
			kinds,
			[
				("hello.txt".to_owned(), ZipKind::File),
				("link".to_owned(), link)
			]
		);
	}
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
		for at in 0..zip.len() {
			for damage in [|b: u8| b ^ 0x01, |b: u8| b ^ 0x80, |_| 0xFF] {
				let mut damaged = zip.clone();
				damaged[at] = damage(damaged[at]);
				let mut source = Sparse::past(skipped, damaged.into());
				let len = source.len();
				let _ =
					std::panic::catch_unwind(move || read_source(&mut source, len, Some(b"pw")))
						.unwrap_or_else(|_| panic!("damage at {at} of a {len}-byte zip panicked"));
			}
		}
	}
}
