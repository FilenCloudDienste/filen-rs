//! The codec on its real thread, driven from the test thread: asks answered from an in-memory
//! archive, events collected in order.

use std::{
	io::Write,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::{Duration, Instant},
};

use super::*;
use crate::{
	consts::{CHUNK_SIZE, CHUNK_SIZE_U64},
	fs::archive::{extract::ExpansionLimit, format::StreamCodec, worker},
};

const LIMITS: CodecLimits = CodecLimits {
	decoder_memory: 64 << 20,
	max_members: 1000,
	expansion: Some(ExpansionLimit {
		ratio: 1000,
		floor: 256 << 20,
	}),
};

/// What the driver saw, with a file's data joined up.
#[derive(Debug, PartialEq)]
enum Seen {
	Opened(StreamLayout),
	Dir(u64, String),
	File {
		ordinal: u64,
		path: String,
		size: Option<u64>,
		chunks: Vec<usize>,
		data: Vec<u8>,
		ended: bool,
	},
	Skipped(u64, String, u64, ExtractSkipReason),
}

fn path_of(head: &EntryHead) -> String {
	head.path
		.segments
		.iter()
		.map(AsRef::as_ref)
		.collect::<Vec<&str>>()
		.join("/")
}

fn run_with(
	archive: &[u8],
	name: &str,
	limits: CodecLimits,
) -> (Vec<Seen>, Result<ArchiveEnd, Error>) {
	let job = StreamJob {
		name: name.to_owned(),
		len: archive.len() as u64,
		limits,
	};
	let mut link = worker::start(move |port| extract_stream(&port, job)).unwrap();
	let mut seen = Vec::new();
	while let Some(event) = link.events.blocking_recv() {
		match event {
			WorkerEvent::Ask { index, reply, .. } => {
				let start = (index * CHUNK_SIZE_U64) as usize;
				let end = (start + CHUNK_SIZE).min(archive.len());
				let _ = reply.send(Ok(archive[start..end].to_vec()));
			}
			WorkerEvent::Opened(layout) => seen.push(Seen::Opened(layout)),
			WorkerEvent::Entry(head) => seen.push(match head.kind {
				EntryKind::Dir => Seen::Dir(head.ordinal, path_of(&head)),
				EntryKind::File { size } => Seen::File {
					ordinal: head.ordinal,
					path: path_of(&head),
					size,
					chunks: Vec::new(),
					data: Vec::new(),
					ended: false,
				},
			}),
			WorkerEvent::Data(data) => {
				let Some(Seen::File {
					chunks, data: all, ..
				}) = seen.last_mut()
				else {
					panic!("data outside a file");
				};
				chunks.push(data.len());
				all.extend_from_slice(&data);
			}
			WorkerEvent::FileEnd => {
				let Some(Seen::File { ended, .. }) = seen.last_mut() else {
					panic!("a file end outside a file");
				};
				*ended = true;
			}
			WorkerEvent::Skipped(member) => seen.push(Seen::Skipped(
				member.ordinal,
				member.path,
				member.bytes,
				member.reason,
			)),
		}
	}
	// the result follows the events closing, once the codec's thread hands it over
	let deadline = Instant::now() + Duration::from_secs(10);
	let result = loop {
		match link.done.try_recv() {
			Ok(result) => break result,
			Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
				assert!(Instant::now() < deadline, "the codec never returned");
				std::thread::sleep(Duration::from_millis(1));
			}
			Err(tokio::sync::oneshot::error::TryRecvError::Closed) => panic!("the codec died"),
		}
	};
	(seen, result)
}

fn run(archive: &[u8], name: &str) -> (Vec<Seen>, Result<ArchiveEnd, Error>) {
	run_with(archive, name, LIMITS)
}

fn file(ordinal: u64, path: &str, data: &[u8]) -> Seen {
	Seen::File {
		ordinal,
		path: path.to_owned(),
		size: Some(data.len() as u64),
		chunks: data.chunks(CHUNK_SIZE).map(<[u8]>::len).collect(),
		data: data.to_vec(),
		ended: true,
	}
}

fn header(kind: tar::EntryType, size: u64) -> tar::Header {
	let mut header = tar::Header::new_gnu();
	header.set_entry_type(kind);
	header.set_size(size);
	header.set_mode(0o644);
	header.set_mtime(1_700_000_000);
	header
}

fn append(builder: &mut tar::Builder<Vec<u8>>, kind: tar::EntryType, path: &str, data: &[u8]) {
	let mut header = header(kind, data.len() as u64);
	builder.append_data(&mut header, path, data).unwrap();
}

/// A tar of every kind of member the codec treats differently.
fn sample_tar() -> Vec<u8> {
	let mut builder = tar::Builder::new(Vec::new());
	append(&mut builder, tar::EntryType::Directory, "docs/", b"");
	append(
		&mut builder,
		tar::EntryType::Regular,
		"docs/a.txt",
		b"alpha",
	);
	let mut link = header(tar::EntryType::Symlink, 0);
	builder
		.append_link(&mut link, "docs/link", "a.txt")
		.unwrap();
	let mut hard = header(tar::EntryType::Link, 0);
	builder
		.append_link(&mut hard, "docs/hard", "docs/a.txt")
		.unwrap();
	// a path that climbs out, which the builder refuses to write, so written by hand
	let mut evil = header(tar::EntryType::Regular, 4);
	evil.as_old_mut().name[..7].copy_from_slice(b"../evil");
	evil.set_cksum();
	builder.append(&evil, &b"evil"[..]).unwrap();
	append(&mut builder, tar::EntryType::Directory, "./", b"");
	builder.into_inner().unwrap()
}

fn sample_seen() -> Vec<Seen> {
	vec![
		Seen::Dir(0, "docs".into()),
		file(1, "docs/a.txt", b"alpha"),
		Seen::Skipped(
			2,
			"docs/link".into(),
			0,
			ExtractSkipReason::Symlink {
				target: "a.txt".into(),
			},
		),
		Seen::Skipped(3, "docs/hard".into(), 0, ExtractSkipReason::Hardlink),
		Seen::Skipped(4, "../evil".into(), 4, ExtractSkipReason::UnsafePath),
	]
}

fn gzip(data: &[u8]) -> Vec<u8> {
	let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
	encoder.write_all(data).unwrap();
	encoder.finish().unwrap()
}

fn kind(result: Result<ArchiveEnd, Error>) -> ErrorKind {
	result.expect_err("the codec should fail").kind()
}

#[test]
fn a_bare_tar_is_sent_member_by_member() {
	let (seen, end) = run(&sample_tar(), "sample.tar");
	let mut expected = vec![Seen::Opened(StreamLayout::Tar { codec: None })];
	expected.extend(sample_seen());
	assert_eq!(seen, expected);
	assert_eq!(
		end.unwrap(),
		ArchiveEnd {
			unaccounted_bytes: 0
		}
	);
}

#[test]
fn a_file_is_sent_in_whole_chunks() {
	let data: Vec<u8> = (0..CHUNK_SIZE + 7).map(|i| (i % 251) as u8).collect();
	let mut builder = tar::Builder::new(Vec::new());
	append(&mut builder, tar::EntryType::Regular, "big.bin", &data);
	let (seen, _) = run(&builder.into_inner().unwrap(), "big.tar");
	assert_eq!(
		seen,
		[
			Seen::Opened(StreamLayout::Tar { codec: None }),
			file(0, "big.bin", &data),
		]
	);
	let Seen::File { chunks, .. } = &seen[1] else {
		unreachable!()
	};
	assert_eq!(chunks, &[CHUNK_SIZE, 7]);
}

#[test]
fn a_compressed_tar_reports_the_data_behind_it() {
	let mut archive = gzip(&sample_tar());
	archive.extend_from_slice(b"junk");
	let (seen, end) = run(&archive, "sample.tgz");
	let mut expected = vec![Seen::Opened(StreamLayout::Tar {
		codec: Some(StreamCodec::Gzip),
	})];
	expected.extend(sample_seen());
	assert_eq!(seen, expected);
	assert_eq!(
		end.unwrap(),
		ArchiveEnd {
			unaccounted_bytes: 4
		}
	);
}

#[test]
fn a_single_compressed_file_is_named_after_the_archive() {
	let data = b"a single file, compressed on its own".repeat(100);
	let (seen, end) = run(&gzip(&data), "notes.txt.gz");
	assert_eq!(
		seen,
		[
			Seen::Opened(StreamLayout::Single {
				codec: StreamCodec::Gzip
			}),
			Seen::File {
				ordinal: 0,
				path: "notes.txt".into(),
				size: None,
				chunks: vec![data.len()],
				data: data.clone(),
				ended: true,
			},
		]
	);
	assert_eq!(
		end.unwrap(),
		ArchiveEnd {
			unaccounted_bytes: 0
		}
	);
}

#[test]
fn what_the_codec_cannot_read_is_refused() {
	assert_eq!(
		kind(run(b"PK\x03\x04rest", "a.zip").1),
		ErrorKind::ArchiveUnsupported
	);
	assert_eq!(
		kind(run(b"just some text", "a.txt").1),
		ErrorKind::ArchiveUnsupported
	);

	let archive = gzip(&sample_tar());
	let (_, end) = run(&archive[..archive.len() - 10], "sample.tgz");
	assert_eq!(kind(end), ErrorKind::ArchiveCorrupt);
}

#[test]
fn archives_beyond_the_limits_are_refused() {
	let zeros = gzip(&vec![0u8; 4 << 20]);
	let bomb = CodecLimits {
		expansion: Some(ExpansionLimit {
			ratio: 10,
			floor: 1 << 20,
		}),
		..LIMITS
	};
	let (_, end) = run_with(&zeros, "zeros.gz", bomb);
	assert_eq!(kind(end), ErrorKind::ArchiveTooLarge);

	let few = CodecLimits {
		max_members: 2,
		..LIMITS
	};
	let (seen, end) = run_with(&sample_tar(), "sample.tar", few);
	assert_eq!(kind(end), ErrorKind::ArchiveTooLarge);
	assert_eq!(seen.len(), 3, "the members before the limit were sent");

	let small_memory = CodecLimits {
		decoder_memory: 128 << 10,
		..LIMITS
	};
	let bzip2 = {
		let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
		encoder.write_all(b"data").unwrap();
		encoder.finish().unwrap()
	};
	let (_, end) = run_with(&bzip2, "a.bz2", small_memory);
	assert_eq!(kind(end), ErrorKind::ArchiveTooLarge);
}

#[test]
fn the_codec_stops_once_its_driver_is_gone() {
	let archive = sample_tar();
	let len = archive.len() as u64;
	let exited = Arc::new(AtomicBool::new(false));
	let mut link = worker::start({
		let exited = Arc::clone(&exited);
		move |port| {
			let result = extract_stream(
				&port,
				StreamJob {
					name: "sample.tar".into(),
					len,
					limits: LIMITS,
				},
			);
			exited.store(true, Ordering::SeqCst);
			result
		}
	})
	.unwrap();
	let Some(WorkerEvent::Ask { reply, .. }) = link.events.blocking_recv() else {
		panic!("the codec asks for the archive first");
	};
	reply.send(Ok(archive)).unwrap();
	drop(link);
	let deadline = Instant::now() + Duration::from_secs(10);
	while !exited.load(Ordering::SeqCst) {
		assert!(Instant::now() < deadline, "the codec thread kept running");
		std::thread::sleep(Duration::from_millis(5));
	}
}
