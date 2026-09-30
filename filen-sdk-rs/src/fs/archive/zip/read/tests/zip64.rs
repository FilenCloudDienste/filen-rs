//! Zip64: archives past 4 GiB, entries of more than 4 GiB, and more than 65,535 entries.

use super::*;

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
		::zip::ZipArchive::new(Cursor::new(&zip)).unwrap().len(),
		70_000
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
pub(super) struct Sparse {
	runs: Vec<Run>,
	pos: u64,
}

impl Sparse {
	/// `skipped` zeros and then `data`: the whole of an archive written by [`ZipWriter::past`].
	pub(super) fn past(skipped: u64, data: Sparse) -> Self {
		Self {
			runs: [Run::Zeros(skipped)].into_iter().chain(data.runs).collect(),
			pos: 0,
		}
	}

	pub(super) fn len(&self) -> u64 {
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
				let at = usize::try_from(self.pos - start).unwrap();
				let n = buf
					.len()
					.min(usize::try_from(end - self.pos).unwrap_or(usize::MAX));
				match run {
					Run::Zeros(_) => buf[..n].fill(0),
					Run::Bytes(bytes) => {
						buf[..n].copy_from_slice(&bytes[at..at + n]);
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
pub(super) const PAST_4_GIB: u64 = 5 << 30;

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
	let header = read_at(source, shift + entry.header_offset, LOCAL_HEADER_LEN).unwrap();
	let (name_len, extra_len) = (
		usize::from(u16_at(&header, 26)),
		usize::from(u16_at(&header, 28)),
	);
	let extra = read_at(
		source,
		shift + entry.header_offset + LOCAL_HEADER_LEN_U64 + name_len as u64,
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
			+ LOCAL_HEADER_LEN_U64
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

			let mut archive = ::zip::ZipArchive::new(source).unwrap();
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

	let mut archive = ::zip::ZipArchive::new(source).unwrap();
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
