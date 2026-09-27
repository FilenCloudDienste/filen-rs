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
		worker,
		zip::{
			crypto::AesStrength,
			read::{EntryLimits, ZipKind, ZipLimits, open_entry, read_index},
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
	let mut link = worker::start(move |port| compress(&port, job)).unwrap();
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
				let start = (index * CHUNK_SIZE_U64) as usize;
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
	(0..len).map(|i| (i % 253) as u8 ^ seed).collect()
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
			let at = entry.header_offset as usize;
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
