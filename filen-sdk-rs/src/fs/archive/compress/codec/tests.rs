//! The codec on its real thread, driven from the test thread, and what it writes read back
//! through the SDK's own decoders and tar reader.

use std::{
	collections::HashMap,
	time::{Duration, Instant},
};

use chrono::TimeZone;

use super::*;
use crate::{
	consts::{CHUNK_SIZE, CHUNK_SIZE_U64},
	fs::archive::{
		decode::open_stream,
		encode::Compression,
		format::StreamCodec,
		tar_iter::{MemberKind, TarReader},
		worker::{self, WorkerLink},
		zip::{
			crypto::AesStrength,
			read::{EntryLimits, ZipEntry, ZipKind, ZipLimits, open_entry, read_index},
			write::ZipMethod,
		},
	},
};

/// What the driver saw: the archive, its chunk sizes, and how many file ends.
struct Written {
	archive: Vec<u8>,
	chunks: Vec<usize>,
	file_ends: usize,
	result: Result<u64, Error>,
}

/// Runs the codec, answering asks from `sources`; `short` sources answer one byte short.
fn run(
	format: CompressFormat,
	entries: Vec<ArchiveEntry>,
	sources: &HashMap<u32, Vec<u8>>,
	short: Option<u32>,
) -> Written {
	run_with(format, entries, sources, short, None)
}

fn run_with(
	format: CompressFormat,
	entries: Vec<ArchiveEntry>,
	sources: &HashMap<u32, Vec<u8>>,
	short: Option<u32>,
	password: Option<&str>,
) -> Written {
	let job = CompressJob {
		format,
		entries,
		password: password.map(|p| ArchivePassword::new(p.to_owned()).unwrap()),
	};
	drive(
		worker::start(move |port| compress(&port, job)).unwrap(),
		sources,
		short,
	)
}

/// Answers the codec's asks from `sources` until it returns; `short` sources answer one byte
/// short.
fn drive(
	mut link: WorkerLink<Result<u64, Error>>,
	sources: &HashMap<u32, Vec<u8>>,
	short: Option<u32>,
) -> Written {
	let mut written = Written {
		archive: Vec::new(),
		chunks: Vec::new(),
		file_ends: 0,
		result: Ok(0),
	};
	while let Some(event) = link.events.blocking_recv() {
		match event {
			WorkerEvent::Ask {
				source,
				index,
				reply,
			} => {
				let data = &sources[&source];
				let start = usize::try_from(index * CHUNK_SIZE_U64).unwrap();
				let mut end = (start + CHUNK_SIZE).min(data.len());
				if short == Some(source) && end == data.len() {
					end -= 1;
				}
				let _ = reply.send(Ok(data[start..end].to_vec()));
			}
			WorkerEvent::Data(chunk) => {
				written.chunks.push(chunk.len());
				written.archive.extend_from_slice(&chunk);
			}
			WorkerEvent::FileEnd => written.file_ends += 1,
			other => panic!("the compressing codec sent {other:?}"),
		}
	}
	let deadline = Instant::now() + Duration::from_secs(10);
	written.result = loop {
		match link.done.try_recv() {
			Ok(result) => break result,
			Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
				assert!(Instant::now() < deadline, "the codec never returned");
				std::thread::sleep(Duration::from_millis(1));
			}
			Err(tokio::sync::oneshot::error::TryRecvError::Closed) => panic!("the codec died"),
		}
	};
	written
}

/// A member read back: kind, path, mtime and data.
#[derive(Debug, PartialEq)]
struct Member {
	dir: bool,
	path: String,
	mtime: i64,
	data: Vec<u8>,
}

fn read_tar(archive: &[u8], codec: Option<StreamCodec>) -> Vec<Member> {
	let reader: Box<dyn Read> = match codec {
		None => Box::new(archive),
		Some(codec) => Box::new(open_stream(codec, archive, 512 << 20).unwrap()),
	};
	let mut tar = TarReader::new(reader, 1000);
	let mut members = Vec::new();
	while let Some(member) = tar.next_member().unwrap() {
		let mut data = Vec::new();
		let mut buf = [0u8; 8192];
		loop {
			let n = tar.read_body(&mut buf).unwrap();
			if n == 0 {
				break;
			}
			data.extend_from_slice(&buf[..n]);
		}
		members.push(Member {
			dir: member.kind == MemberKind::Dir,
			path: member.path,
			mtime: member.modified.map_or(0, |time| time.secs),
			data,
		});
	}
	members
}

fn when(secs: i64) -> Option<DateTime<Utc>> {
	Some(Utc.timestamp_opt(secs, 0).unwrap())
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
	(0..len)
		.map(|i| (i % 253).to_le_bytes()[0] ^ seed)
		.collect()
}

/// A tree with a long path, an empty file and a file over a chunk.
fn sample() -> (Vec<ArchiveEntry>, HashMap<u32, Vec<u8>>, Vec<Member>) {
	let long_dir = "a-directory-name-long-enough".repeat(4);
	let long_file = format!("{long_dir}/{}", "f".repeat(60));
	let sources = HashMap::from([
		(0, b"alpha".to_vec()),
		(1, Vec::new()),
		(2, pattern(CHUNK_SIZE + 1234, 5)),
		(3, b"deep".to_vec()),
	]);
	let entries = vec![
		ArchiveEntry::Dir {
			path: "docs".into(),
			modified: when(1_600_000_000),
		},
		ArchiveEntry::Dir {
			path: long_dir.clone(),
			modified: None,
		},
		ArchiveEntry::File {
			source: 0,
			path: "docs/a.txt".into(),
			size: 5,
			modified: when(1_700_000_000),
		},
		ArchiveEntry::File {
			source: 1,
			path: "empty".into(),
			size: 0,
			modified: when(-5),
		},
		ArchiveEntry::File {
			source: 2,
			path: "big.bin".into(),
			size: CHUNK_SIZE as u64 + 1234,
			modified: None,
		},
		ArchiveEntry::File {
			source: 3,
			path: long_file.clone(),
			size: 4,
			modified: when(1),
		},
	];
	let members = vec![
		Member {
			dir: true,
			path: "docs/".into(),
			mtime: 1_600_000_000,
			data: Vec::new(),
		},
		Member {
			dir: true,
			path: format!("{long_dir}/"),
			mtime: 0,
			data: Vec::new(),
		},
		Member {
			dir: false,
			path: "docs/a.txt".into(),
			mtime: 1_700_000_000,
			data: b"alpha".to_vec(),
		},
		Member {
			dir: false,
			path: "empty".into(),
			// before the epoch, written as it
			mtime: 0,
			data: Vec::new(),
		},
		Member {
			dir: false,
			path: "big.bin".into(),
			mtime: 0,
			data: sources[&2].clone(),
		},
		Member {
			dir: false,
			path: long_file,
			mtime: 1,
			data: b"deep".to_vec(),
		},
	];
	(entries, sources, members)
}

#[test]
fn every_tar_format_reads_back_member_for_member() {
	let codecs = [
		None,
		Some(StreamCodec::Gzip),
		Some(StreamCodec::Bzip2),
		Some(StreamCodec::Xz),
		Some(StreamCodec::Lzma),
		Some(StreamCodec::Lzip),
		Some(StreamCodec::Lz4),
		Some(StreamCodec::Brotli),
	];
	for codec in codecs {
		let (entries, sources, expected) = sample();
		let format = CompressFormat::Tar {
			compression: codec.map(|codec| Compression { codec, level: None }),
		};
		let written = run(format, entries, &sources, None);
		let len = written.result.unwrap();
		assert_eq!(len, written.archive.len() as u64, "{codec:?}");
		assert_eq!(written.file_ends, 4, "{codec:?}");
		let (last, whole) = written.chunks.split_last().unwrap();
		assert!(whole.iter().all(|&n| n == CHUNK_SIZE), "{codec:?}");
		assert!(*last > 0 && *last <= CHUNK_SIZE, "{codec:?}");
		assert_eq!(read_tar(&written.archive, codec), expected, "{codec:?}");
		if codec.is_none() {
			let (entries, ..) = sample();
			assert_eq!(len, tar_size(&entries), "the size gate is exact");
		}
	}
}

#[test]
fn a_single_file_compresses_on_its_own() {
	let data = pattern(3 * CHUNK_SIZE / 2, 9);
	let sources = HashMap::from([(7, data.clone())]);
	let entries = vec![ArchiveEntry::File {
		source: 7,
		path: "ignored".into(),
		size: data.len() as u64,
		modified: None,
	}];
	let format = CompressFormat::Single {
		compression: Compression {
			codec: StreamCodec::Xz,
			level: Some(1),
		},
	};
	let written = run(format, entries, &sources, None);
	assert_eq!(written.result.unwrap(), written.archive.len() as u64);
	assert_eq!(written.file_ends, 1);
	let mut decoded = Vec::new();
	open_stream(StreamCodec::Xz, &written.archive[..], 512 << 20)
		.unwrap()
		.read_to_end(&mut decoded)
		.unwrap();
	assert_eq!(decoded, data);
}

#[test]
fn a_single_file_is_exactly_one_file() {
	let (entries, sources, _) = sample();
	let format = CompressFormat::Single {
		compression: Compression {
			codec: StreamCodec::Gzip,
			level: None,
		},
	};
	let written = run(format, entries, &sources, None);
	assert_eq!(written.result.unwrap_err().kind(), ErrorKind::InvalidState);
	assert!(written.archive.is_empty(), "nothing is written");
}

#[test]
fn a_source_that_changed_length_fails_the_archive() {
	let (entries, sources, _) = sample();
	let written = run(
		CompressFormat::Tar { compression: None },
		entries,
		&sources,
		Some(2),
	);
	assert_eq!(
		written.result.unwrap_err().kind(),
		ErrorKind::FileChangedDuringSync
	);
}

#[test]
fn an_empty_archive_is_valid() {
	let written = run(
		CompressFormat::Tar {
			compression: Some(Compression {
				codec: StreamCodec::Gzip,
				level: None,
			}),
		},
		Vec::new(),
		&HashMap::new(),
		None,
	);
	written.result.unwrap();
	assert!(read_tar(&written.archive, Some(StreamCodec::Gzip)).is_empty());
}

#[test]
fn the_size_gate_is_exact_at_the_long_name_boundaries() {
	for len in [99, 100, 101, 511, 512, 513] {
		let dir = "d".repeat(len);
		let file = "f".repeat(len);
		let entries = vec![
			ArchiveEntry::Dir {
				path: dir,
				modified: None,
			},
			ArchiveEntry::File {
				source: 0,
				path: file,
				size: 3,
				modified: None,
			},
		];
		let sources = HashMap::from([(0, b"abc".to_vec())]);
		let written = run(
			CompressFormat::Tar { compression: None },
			entries.clone(),
			&sources,
			None,
		);
		assert_eq!(
			written.result.unwrap(),
			tar_size(&entries),
			"paths of {len} bytes"
		);
	}
}

/// Reads a zip back through the zip crate: path (dirs end in `/`) and data per entry.
fn zip_crate_entries(archive: &[u8], password: Option<&[u8]>) -> Vec<(String, Vec<u8>)> {
	let mut zip = zip8::ZipArchive::new(std::io::Cursor::new(archive)).unwrap();
	(0..zip.len())
		.map(|i| {
			let mut file = match password {
				None => zip.by_index(i).unwrap(),
				Some(password) => zip.by_index_decrypt(i, password).unwrap(),
			};
			let mut data = Vec::new();
			file.read_to_end(&mut data).unwrap();
			(file.name().to_owned(), data)
		})
		.collect()
}

/// Reads a zip back through our own reader, which checks every CRC and AES code.
fn our_entries(archive: &[u8], password: Option<&[u8]>) -> Vec<(String, Vec<u8>)> {
	let mut source = std::io::Cursor::new(archive);
	let index = read_index(
		&mut source,
		archive.len() as u64,
		ZipLimits {
			max_index_bytes: 32 << 20,
			max_entries: 1000,
		},
	)
	.unwrap();
	assert!(index.overlapping.is_empty() && index.duplicate_count == 0);
	assert_eq!(index.prefix_bytes, 0);
	index
		.entries
		.iter()
		.map(|entry| {
			let mut data = Vec::new();
			if entry.kind == ZipKind::File {
				open_entry(
					&mut source,
					index.shift,
					entry,
					password,
					EntryLimits {
						decoder_memory: 64 << 20,
					},
				)
				.unwrap()
				.read_to_end(&mut data)
				.unwrap();
			}
			(entry.name.clone(), data)
		})
		.collect()
}

fn members_as_entries(members: &[Member]) -> Vec<(String, Vec<u8>)> {
	members
		.iter()
		.map(|member| (member.path.clone(), member.data.clone()))
		.collect()
}

#[test]
fn every_zip_method_and_encryption_reads_back_entry_for_entry() {
	let methods = [
		ZipMethod::Stored,
		ZipMethod::Deflate { level: 1 },
		ZipMethod::Deflate { level: 9 },
		ZipMethod::Bzip2 { level: 1 },
	];
	for method in methods {
		for encryption in [None, Some(AesStrength::Aes128), Some(AesStrength::Aes256)] {
			let (entries, sources, members) = sample();
			let password = encryption.map(|_| "correct horse");
			let written = run_with(
				CompressFormat::Zip { method, encryption },
				entries,
				&sources,
				None,
				password,
			);
			let case = format!("{method:?} {encryption:?}");
			assert_eq!(
				written.result.unwrap(),
				written.archive.len() as u64,
				"{case}"
			);
			assert_eq!(written.file_ends, 4, "{case}");
			let (_, whole) = written.chunks.split_last().unwrap();
			assert!(whole.iter().all(|&n| n == CHUNK_SIZE), "{case}");
			let expected = members_as_entries(&members);
			let password = password.map(str::as_bytes);
			assert_eq!(our_entries(&written.archive, password), expected, "{case}");
			assert_eq!(
				zip_crate_entries(&written.archive, password),
				expected,
				"{case}"
			);
		}
	}
}

/// A zip entry's records around its data, as its local header and data descriptor state them.
#[derive(Debug, PartialEq)]
struct EntryRecords {
	/// The local header's compressed size and size fields.
	local_sizes: (u32, u32),
	/// The data of the local header's zip64 extra field, if it has one.
	local_zip64: Option<Vec<u8>>,
	/// The data descriptor's compressed size and size, and whether they are 8 bytes each.
	descriptor: (u64, u64, bool),
}

/// The records of the file `entry` of `archive`, reading its descriptor as zip64 when `zip64`.
fn entry_records(archive: &[u8], entry: &ZipEntry, zip64: bool) -> EntryRecords {
	const ZIP64_EXTRA_ID: u16 = 0x0001;
	const DATA_DESCRIPTOR_SIG: u32 = 0x0807_4b50;
	let u16_at = |at: usize| u16::from_le_bytes(archive[at..at + 2].try_into().unwrap());
	let u32_at = |at: usize| u32::from_le_bytes(archive[at..at + 4].try_into().unwrap());
	let u64_at = |at: usize| u64::from_le_bytes(archive[at..at + 8].try_into().unwrap());
	let at = usize::try_from(entry.header_offset).unwrap();
	let name_len = usize::from(u16_at(at + 26));
	let extra_len = usize::from(u16_at(at + 28));
	let mut extra = &archive[at + 30 + name_len..at + 30 + name_len + extra_len];
	let mut local_zip64 = None;
	while !extra.is_empty() {
		let (id, len) = (
			u16::from_le_bytes([extra[0], extra[1]]),
			usize::from(u16::from_le_bytes([extra[2], extra[3]])),
		);
		if id == ZIP64_EXTRA_ID {
			local_zip64 = Some(extra[4..4 + len].to_vec());
		}
		extra = &extra[4 + len..];
	}
	let descriptor =
		at + 30 + name_len + extra_len + usize::try_from(entry.compressed_size).unwrap();
	assert_eq!(u32_at(descriptor), DATA_DESCRIPTOR_SIG, "{}", entry.name);
	EntryRecords {
		local_sizes: (u32_at(at + 18), u32_at(at + 22)),
		local_zip64,
		descriptor: if zip64 {
			(u64_at(descriptor + 8), u64_at(descriptor + 16), true)
		} else {
			(
				u64::from(u32_at(descriptor + 8)),
				u64::from(u32_at(descriptor + 12)),
				false,
			)
		},
	}
}

#[test]
fn a_zips_entries_from_the_zip64_threshold_up_read_back_entry_for_entry() {
	// lowered from 4 GiB, so the sample's file over a chunk is written as an entry that large
	// is, and its small files as they are
	const ZIP64_FROM: u64 = CHUNK_SIZE_U64;
	for (method, encryption) in [
		(ZipMethod::Stored, None),
		(ZipMethod::Deflate { level: 6 }, Some(AesStrength::Aes256)),
		(ZipMethod::Bzip2 { level: 1 }, None),
	] {
		let (entries, sources, members) = sample();
		let password = encryption.map(|_| ArchivePassword::new("zip64".to_owned()).unwrap());
		let job = CompressJob {
			format: CompressFormat::Zip { method, encryption },
			entries,
			password: password.clone(),
		};
		let written = drive(
			worker::start(move |port| {
				compress_with(&port, job, |sink| ZipWriter::past(sink, 0, ZIP64_FROM))
			})
			.unwrap(),
			&sources,
			None,
		);
		let case = format!("{method:?} {encryption:?}");
		assert_eq!(
			written.result.unwrap(),
			written.archive.len() as u64,
			"{case}"
		);
		assert_eq!(written.file_ends, 4, "{case}");

		// a zip64 entry's local header leaves its sizes to its zip64 data descriptor, and
		// carries the zip64 field that says so; every other entry's leaves them at 0
		let mut source = std::io::Cursor::new(&written.archive[..]);
		let index = read_index(
			&mut source,
			written.archive.len() as u64,
			ZipLimits {
				max_index_bytes: 1 << 20,
				max_entries: 100,
			},
		)
		.unwrap();
		let mut zip64_entries = Vec::new();
		for entry in index.entries.iter().filter(|e| e.kind == ZipKind::File) {
			let zip64 = entry.size >= ZIP64_FROM;
			if zip64 {
				zip64_entries.push(entry.name.as_str());
			}
			assert_eq!(
				entry_records(&written.archive, entry, zip64),
				EntryRecords {
					local_sizes: if zip64 { (u32::MAX, u32::MAX) } else { (0, 0) },
					local_zip64: zip64.then(|| vec![0; 16]),
					descriptor: (entry.compressed_size, entry.size, zip64),
				},
				"{} {case}",
				entry.name
			);
		}
		assert_eq!(zip64_entries, ["big.bin"], "{case}");

		let expected = members_as_entries(&members);
		let password = password.as_ref().map(ArchivePassword::as_bytes);
		assert_eq!(our_entries(&written.archive, password), expected, "{case}");
		assert_eq!(
			zip_crate_entries(&written.archive, password),
			expected,
			"{case}"
		);
	}
}

#[test]
fn every_zip_entry_gets_its_own_salt() {
	let (entries, sources, _) = sample();
	let written = run_with(
		CompressFormat::Zip {
			method: ZipMethod::Stored,
			encryption: Some(AesStrength::Aes256),
		},
		entries,
		&sources,
		None,
		Some("pw"),
	);
	written.result.unwrap();
	let mut source = std::io::Cursor::new(&written.archive[..]);
	let index = read_index(
		&mut source,
		written.archive.len() as u64,
		ZipLimits {
			max_index_bytes: 1 << 20,
			max_entries: 100,
		},
	)
	.unwrap();
	// a stored AES entry's data starts with its 16-byte salt
	let salts: std::collections::HashSet<Vec<u8>> = index
		.entries
		.iter()
		.filter(|entry| entry.kind == ZipKind::File)
		.map(|entry| {
			let at = usize::try_from(entry.header_offset).unwrap();
			let name_len = u16::from_le_bytes([written.archive[at + 26], written.archive[at + 27]]);
			let extra_len =
				u16::from_le_bytes([written.archive[at + 28], written.archive[at + 29]]);
			let data = at + 30 + usize::from(name_len) + usize::from(extra_len);
			written.archive[data..data + 16].to_vec()
		})
		.collect();
	assert_eq!(salts.len(), 4);
}

#[test]
fn an_encrypted_zip_without_a_password_writes_nothing() {
	let (entries, sources, _) = sample();
	let written = run(
		CompressFormat::Zip {
			method: ZipMethod::Stored,
			encryption: Some(AesStrength::Aes256),
		},
		entries,
		&sources,
		None,
	);
	assert_eq!(
		written.result.unwrap_err().kind(),
		ErrorKind::ArchivePasswordRequired
	);
	assert!(written.archive.is_empty());
}

#[test]
fn a_zip_source_that_changed_length_fails_the_archive() {
	let (entries, sources, _) = sample();
	let written = run(
		CompressFormat::Zip {
			method: ZipMethod::Deflate { level: 6 },
			encryption: None,
		},
		entries,
		&sources,
		Some(2),
	);
	assert_eq!(
		written.result.unwrap_err().kind(),
		ErrorKind::FileChangedDuringSync
	);
}

#[test]
fn an_empty_zip_is_valid() {
	let written = run(
		CompressFormat::Zip {
			method: ZipMethod::Deflate { level: 6 },
			encryption: None,
		},
		Vec::new(),
		&HashMap::new(),
		None,
	);
	assert_eq!(written.result.unwrap(), 22, "just the end record");
	assert!(zip_crate_entries(&written.archive, None).is_empty());
	assert!(our_entries(&written.archive, None).is_empty());
}
