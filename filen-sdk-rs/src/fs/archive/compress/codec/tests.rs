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
	let job = CompressJob { format, entries };
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
