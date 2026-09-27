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
	fs::archive::{
		extract::ExpansionLimit,
		format::StreamCodec,
		password::ArchivePassword,
		sevenz::write::{SevenZEncryption, SevenZMethod, SevenZWriter},
		worker,
		zip::{
			crypto::AesStrength,
			write::{Encryption, ZipMethod, ZipWriter},
		},
	},
};

const LIMITS: CodecLimits = CodecLimits {
	decoder_memory: 64 << 20,
	max_members: 1000,
	expansion: Some(ExpansionLimit {
		ratio: 1000,
		floor: 256 << 20,
	}),
	max_index_bytes: 32 << 20,
	max_bytes: None,
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
	run_full(archive, name, limits, None)
}

fn run_full(
	archive: &[u8],
	name: &str,
	limits: CodecLimits,
	password: Option<&str>,
) -> (Vec<Seen>, Result<ArchiveEnd, Error>) {
	let (seen, result, _) = run_counting(archive, name, limits, password);
	(seen, result)
}

/// As [`run_full`], and the bytes of the archive the codec counted as read.
fn run_counting(
	archive: &[u8],
	name: &str,
	limits: CodecLimits,
	password: Option<&str>,
) -> (Vec<Seen>, Result<ArchiveEnd, Error>, u64) {
	let job = StreamJob {
		name: name.to_owned(),
		len: archive.len() as u64,
		limits,
		password: password.map(|p| ArchivePassword::new(p.to_owned()).unwrap()),
	};
	let mut link = worker::start(move |port| extract_stream(&port, job)).unwrap();
	let mut seen = Vec::new();
	while let Some(event) = link.events.blocking_recv() {
		match event {
			WorkerEvent::Ask { index, reply, .. } => {
				let start = usize::try_from(index * CHUNK_SIZE_U64).unwrap();
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
			WorkerEvent::Head(_) => panic!("an extracting codec sent a head"),
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
	(seen, result, link.shared.input_bytes())
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
		Seen::Skipped(
			3,
			"docs/hard".into(),
			0,
			ExtractSkipReason::Hardlink {
				target: "docs/a.txt".into(),
			},
		),
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
			unchecked_entries: 0,
			unaccounted_bytes: 0,
			duplicates: None,
		}
	);
}

#[test]
fn a_hard_link_carrying_data_is_extracted_as_a_file() {
	let mut builder = tar::Builder::new(Vec::new());
	append(&mut builder, tar::EntryType::Regular, "a.txt", b"alpha");
	// a hard link in a pax archive may carry the file's data, which is extracted like a file's
	let record = b"15 mtime=17000\n";
	let mut pax = tar::Header::new_ustar();
	pax.set_entry_type(tar::EntryType::XHeader);
	pax.set_size(record.len() as u64);
	builder
		.append_data(&mut pax, "PaxHeader", &record[..])
		.unwrap();
	let mut hard = tar::Header::new_ustar();
	hard.set_entry_type(tar::EntryType::Link);
	hard.set_size(5);
	hard.set_mode(0o644);
	hard.set_link_name("a.txt").unwrap();
	builder
		.append_data(&mut hard, "b.txt", &b"alpha"[..])
		.unwrap();
	let (seen, end) = run(&builder.into_inner().unwrap(), "links.tar");
	assert_eq!(
		seen,
		[
			Seen::Opened(StreamLayout::Tar { codec: None }),
			file(0, "a.txt", b"alpha"),
			file(1, "b.txt", b"alpha"),
		]
	);
	assert_eq!(end.unwrap().unaccounted_bytes, 0);
}

#[test]
fn a_file_is_sent_in_whole_chunks() {
	let data: Vec<u8> = (0..CHUNK_SIZE + 7)
		.map(|i| (i % 251).to_le_bytes()[0])
		.collect();
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
			unchecked_entries: 0,
			unaccounted_bytes: 4,
			duplicates: None,
		}
	);
}

#[test]
fn an_empty_tar_is_an_archive_of_nothing() {
	// what `tar cf e.tar -T /dev/null` writes: the end-of-archive marker, padded to a record
	let empty = vec![0u8; 10 * 1024];
	let nothing = ArchiveEnd {
		unchecked_entries: 0,
		unaccounted_bytes: 0,
		duplicates: None,
	};
	let (seen, end) = run(&empty, "e.tar");
	assert_eq!(seen, [Seen::Opened(StreamLayout::Tar { codec: None })]);
	assert_eq!(end.unwrap(), nothing);
	let (seen, end) = run(&gzip(&empty), "e.tar.gz");
	assert_eq!(
		seen,
		[Seen::Opened(StreamLayout::Tar {
			codec: Some(StreamCodec::Gzip)
		})]
	);
	assert_eq!(end.unwrap(), nothing);
	// without a tar's name, zeros are a file of zeros
	let (seen, _) = run(&gzip(&empty), "zeros.gz");
	assert_eq!(
		seen[0],
		Seen::Opened(StreamLayout::Single {
			codec: StreamCodec::Gzip
		})
	);
}

#[test]
fn a_zstd_tar_is_read_through_its_frames() {
	let mut encoder = crate::fs::archive::encode::open_encoder(
		crate::fs::archive::encode::Compression {
			codec: StreamCodec::Zstd,
			level: None,
		},
		Vec::new(),
	)
	.unwrap();
	encoder.write_all(&sample_tar()).unwrap();
	// a skippable frame ahead of the data, as some writers put metadata there
	let mut archive = vec![0x50, 0x2A, 0x4D, 0x18, 2, 0, 0, 0, 1, 2];
	archive.extend(encoder.finish().unwrap());
	let (seen, end) = run(&archive, "sample.tar.zst");
	let mut expected = vec![Seen::Opened(StreamLayout::Tar {
		codec: Some(StreamCodec::Zstd),
	})];
	expected.extend(sample_seen());
	assert_eq!(seen, expected);
	assert_eq!(end.unwrap().unaccounted_bytes, 10);
}

#[test]
fn what_a_codec_without_a_checksum_decodes_is_unchecked() {
	let brotli = |data: &[u8]| {
		let mut writer = ::brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
		writer.write_all(data).unwrap();
		writer.into_inner()
	};
	// brotli carries no checksum: its one file, or every file of its tar, is unchecked
	let (_, end) = run(&brotli(b"a note"), "note.txt.br");
	assert_eq!(end.unwrap().unchecked_entries, 1);
	let (_, end) = run(&brotli(&sample_tar()), "sample.tar.br");
	assert_eq!(
		end.unwrap().unchecked_entries,
		1,
		"docs/a.txt is its one file"
	);
	// gzip's CRC-32 checks all of it
	let (_, end) = run(&gzip(&sample_tar()), "sample.tgz");
	assert_eq!(end.unwrap().unchecked_entries, 0);
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
			unchecked_entries: 0,
			unaccounted_bytes: 0,
			duplicates: None,
		}
	);
}

#[test]
fn what_the_codec_cannot_read_is_refused() {
	// zip and 7z magic without the archive behind it
	assert_eq!(
		kind(run(b"PK\x03\x04rest", "a.zip").1),
		ErrorKind::ArchiveCorrupt
	);
	assert_eq!(
		kind(run(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0, 4], "a.7z").1),
		ErrorKind::ArchiveCorrupt
	);
	// a 7z of a format version after 0.x
	let mut future = [0u8; 32];
	future[..6].copy_from_slice(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]);
	future[6] = 1;
	assert_eq!(kind(run(&future, "a.7z").1), ErrorKind::ArchiveUnsupported);
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
					password: None,
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

fn pattern(len: usize, seed: u8) -> Vec<u8> {
	(0..len)
		.map(|i| (i % 251).to_le_bytes()[0] ^ seed)
		.collect()
}

fn zip_of(entries: &[(&str, Option<&[u8]>)], password: Option<&[u8]>) -> Vec<u8> {
	let mut writer = ZipWriter::new(Vec::new());
	for (path, data) in entries {
		match data {
			None => writer.add_dir(path, None).unwrap(),
			Some(data) => {
				let encryption = password.map(|password| Encryption {
					password,
					strength: AesStrength::Aes256,
					salt: vec![9; 16],
				});
				writer
					.add_file(
						path,
						None,
						data.len() as u64,
						ZipMethod::Deflate { level: 6 },
						encryption,
						&mut &data[..],
					)
					.unwrap();
			}
		}
	}
	writer.finish().unwrap()
}

fn zip_sample() -> (Vec<u8>, Vec<Seen>) {
	let big = pattern(CHUNK_SIZE + 99, 4);
	let zip = zip_of(
		&[
			("docs", None),
			("docs/a.txt", Some(b"alpha")),
			("docs/big.bin", Some(&big)),
			("../evil", Some(b"x")),
		],
		None,
	);
	let seen = vec![
		Seen::Opened(StreamLayout::Zip),
		Seen::Dir(0, "docs".into()),
		file(1, "docs/a.txt", b"alpha"),
		file(2, "docs/big.bin", &big),
		Seen::Skipped(3, "../evil".into(), 1, ExtractSkipReason::UnsafePath),
	];
	(zip, seen)
}

#[test]
fn a_zip_is_sent_entry_by_entry() {
	let (zip, expected) = zip_sample();
	let (seen, end) = run(&zip, "bundle.zip");
	assert_eq!(seen, expected);
	assert_eq!(
		end.unwrap(),
		ArchiveEnd {
			unchecked_entries: 0,
			unaccounted_bytes: 0,
			duplicates: None,
		}
	);
}

#[test]
fn every_byte_of_an_archive_counts_once_toward_the_bytes_read() {
	let (zip, _) = zip_sample();
	let sevenz = sevenz_of(
		&[("big.bin", Some(&pattern(CHUNK_SIZE + 99, 4)))],
		SevenZMethod::Lzma2 { level: 1 },
		false,
		None,
	);
	// a zip or 7z is told by its head, read as a stream's is, then read again from its start
	for (archive, name) in [
		(sample_tar(), "sample.tar"),
		(gzip(&sample_tar()), "sample.tar.gz"),
		(zip, "bundle.zip"),
		(sevenz, "sample.7z"),
	] {
		let (_, end, read) = run_counting(&archive, name, LIMITS, None);
		end.unwrap();
		assert_eq!(read, archive.len() as u64, "{name}");
	}
}

#[test]
fn an_encrypted_zip_needs_the_right_password_before_anything_is_sent() {
	let zip = zip_of(&[("secret.txt", Some(b"secret"))], Some(b"right"));
	let (seen, end) = run_full(&zip, "s.zip", LIMITS, None);
	assert!(seen.is_empty());
	assert_eq!(kind(end), ErrorKind::ArchivePasswordRequired);
	let (seen, end) = run_full(&zip, "s.zip", LIMITS, Some("wrong"));
	assert!(seen.is_empty());
	assert_eq!(kind(end), ErrorKind::ArchiveWrongPassword);
	let (seen, end) = run_full(&zip, "s.zip", LIMITS, Some("right"));
	end.unwrap();
	assert_eq!(
		seen,
		[
			Seen::Opened(StreamLayout::Zip),
			file(0, "secret.txt", b"secret")
		]
	);
}

#[test]
fn zip_duplicates_symlinks_and_bombs() {
	let zip = zip_of(&[("same", Some(b"one")), ("same", Some(b"two"))], None);
	let (seen, end) = run(&zip, "d.zip");
	let end = end.unwrap();
	assert_eq!(
		end.duplicates,
		Some(DuplicateEntries {
			names: vec!["same".into()],
			count: 1
		})
	);
	assert_eq!(seen[1], file(1, "same", b"two"), "the last one listed wins");

	let mut writer = zip8::ZipWriter::new(std::io::Cursor::new(Vec::new()));
	writer
		.add_symlink(
			"link",
			"target/file",
			zip8::write::SimpleFileOptions::default(),
		)
		.unwrap();
	let linked = writer.finish().unwrap().into_inner();
	let (seen, end) = run(&linked, "l.zip");
	end.unwrap();
	assert_eq!(
		seen,
		[
			Seen::Opened(StreamLayout::Zip),
			Seen::Skipped(
				0,
				"link".into(),
				11,
				ExtractSkipReason::Symlink {
					target: "target/file".into()
				}
			),
		]
	);

	let zeros = zip_of(&[("zeros", Some(&vec![0u8; 4 << 20]))], None);
	let bomb = CodecLimits {
		expansion: Some(ExpansionLimit {
			ratio: 10,
			floor: 1 << 20,
		}),
		..LIMITS
	};
	let (seen, end) = run_with(&zeros, "z.zip", bomb);
	assert!(
		seen.is_empty(),
		"refused on its stated sizes, before anything is sent"
	);
	assert_eq!(kind(end), ErrorKind::ArchiveTooLarge);
}

#[test]
fn a_zip_bomb_that_understates_its_size_is_stopped_at_it() {
	const STATED: u32 = 1024;
	let mut zip = zip_of(&[("zeros", Some(&vec![0u8; 4 << 20]))], None);
	// the central record's uncompressed size, which is all the up-front check sees
	let record = zip
		.windows(4)
		.rposition(|w| w == 0x0201_4b50u32.to_le_bytes())
		.unwrap();
	zip[record + 24..record + 28].copy_from_slice(&STATED.to_le_bytes());
	let (seen, end) = run(&zip, "z.zip");
	assert_eq!(kind(end), ErrorKind::ArchiveCorrupt);
	let [
		Seen::Opened(StreamLayout::Zip),
		Seen::File { data, ended, .. },
	] = &seen[..]
	else {
		panic!("{seen:?}");
	};
	assert!(data.len() <= STATED as usize && !ended, "{}", data.len());
}

#[test]
fn bytes_after_a_zips_end_record_are_unaccounted() {
	let (mut zip, expected) = zip_sample();
	zip.extend_from_slice(&[0; 100]);
	let (seen, end) = run(&zip, "padded.zip");
	assert_eq!(seen, expected);
	assert_eq!(end.unwrap().unaccounted_bytes, 100);
}

/// A 7z of `entries` (a `None` is a directory), with keys cheap to derive.
fn sevenz_of(
	entries: &[(&str, Option<&[u8]>)],
	method: SevenZMethod,
	solid: bool,
	encryption: Option<(SevenZEncryption, &str)>,
) -> Vec<u8> {
	let password: Option<Vec<u8>> = encryption
		.map(|(_, password)| password.encode_utf16().flat_map(u16::to_le_bytes).collect());
	let mut writer = SevenZWriter::with_cycles_power(
		Vec::new(),
		method,
		solid,
		encryption.map(|(what, _)| (what, &password.as_ref().unwrap()[..])),
		4,
	)
	.unwrap();
	for (path, data) in entries {
		match data {
			None => writer.add_dir(path, None),
			Some(data) => {
				writer
					.add_file(path, None, data.len() as u64, &mut &data[..])
					.unwrap();
			}
		}
	}
	let (mut archive, start) = writer.finish().unwrap();
	archive[..32].copy_from_slice(&start);
	archive
}

#[test]
fn a_7z_is_sent_entry_by_entry() {
	let big = pattern(CHUNK_SIZE + 99, 4);
	for solid in [false, true] {
		let archive = sevenz_of(
			&[
				("docs", None),
				("docs/a.txt", Some(b"alpha")),
				("empty", Some(b"")),
				("big.bin", Some(&big)),
			],
			SevenZMethod::Lzma2 { level: 1 },
			solid,
			None,
		);
		let (seen, end) = run(&archive, "sample.7z");
		// files with data come first, then directories and empty files
		assert_eq!(
			seen,
			vec![
				Seen::Opened(StreamLayout::SevenZ),
				file(0, "docs/a.txt", b"alpha"),
				Seen::File {
					ordinal: 1,
					path: "big.bin".into(),
					size: Some(big.len() as u64),
					chunks: vec![CHUNK_SIZE, 99],
					data: big.clone(),
					ended: true,
				},
				Seen::Dir(2, "docs".into()),
				file(3, "empty", b""),
			],
			"solid {solid}"
		);
		assert_eq!(
			end.unwrap(),
			ArchiveEnd {
				unchecked_entries: 0,
				unaccounted_bytes: 0,
				duplicates: None,
			}
		);
	}
}

#[test]
fn an_encrypted_7z_needs_the_right_password_before_anything_is_sent() {
	for what in [
		SevenZEncryption::Entries,
		SevenZEncryption::EntriesAndHeaders,
	] {
		let archive = sevenz_of(
			&[("secret.txt", Some(b"secret"))],
			SevenZMethod::Lzma2 { level: 1 },
			true,
			Some((what, "right")),
		);
		let (seen, end) = run_full(&archive, "s.7z", LIMITS, None);
		assert_eq!(kind(end), ErrorKind::ArchivePasswordRequired, "{what:?}");
		assert!(seen.is_empty());
		let (seen, end) = run_full(&archive, "s.7z", LIMITS, Some("wrong"));
		assert_eq!(kind(end), ErrorKind::ArchiveWrongPassword, "{what:?}");
		assert!(
			seen.is_empty(),
			"the password is checked before anything is sent"
		);
		let (seen, end) = run_full(&archive, "s.7z", LIMITS, Some("right"));
		end.unwrap();
		assert_eq!(
			seen,
			vec![
				Seen::Opened(StreamLayout::SevenZ),
				file(0, "secret.txt", b"secret"),
			]
		);
	}
}

#[test]
fn sevenz_symlinks_and_anti_items_are_skipped() {
	use sevenz_rust2::{ArchiveEntry as SevenZEntry, ArchiveWriter};
	let mut writer = ArchiveWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
	let mut link = SevenZEntry::new_file("link");
	link.has_windows_attributes = true;
	link.windows_attributes = 0x8000 | (0o120_777 << 16);
	writer
		.push_archive_entry(link, Some(&b"target/file"[..]))
		.unwrap();
	let mut anti = SevenZEntry::new_file("gone");
	anti.is_anti_item = true;
	anti.has_stream = false;
	writer.push_archive_entry::<&[u8]>(anti, None).unwrap();
	writer
		.push_archive_entry(SevenZEntry::new_file("kept"), Some(&b"kept"[..]))
		.unwrap();
	let archive = writer.finish().unwrap().into_inner();
	let (seen, end) = run(&archive, "links.7z");
	end.unwrap();
	assert_eq!(
		seen,
		vec![
			Seen::Opened(StreamLayout::SevenZ),
			Seen::Skipped(
				0,
				"link".into(),
				11,
				ExtractSkipReason::Symlink {
					target: "target/file".into()
				}
			),
			Seen::Skipped(1, "gone".into(), 0, ExtractSkipReason::AntiItem),
			file(2, "kept", b"kept"),
		]
	);
}

/// A Windows REPARSE_DATA_BUFFER for a symlink (`tag` 0xA000000C, with its flags field) or a
/// mount point (0xA0000003), as 7-Zip stores one with `-snl`.
fn reparse_data(tag: u32, substitute: &str, print: &str) -> Vec<u8> {
	let utf16 =
		|text: &str| -> Vec<u8> { text.encode_utf16().flat_map(u16::to_le_bytes).collect() };
	let (substitute, print) = (utf16(substitute), utf16(print));
	let mut body = Vec::new();
	for field in [0, substitute.len(), substitute.len(), print.len()] {
		body.extend_from_slice(&u16::try_from(field).unwrap().to_le_bytes());
	}
	if tag == 0xA000_000C {
		body.extend_from_slice(&1u32.to_le_bytes());
	}
	body.extend(substitute);
	body.extend(print);
	let mut data = tag.to_le_bytes().to_vec();
	data.extend_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
	data.extend_from_slice(&[0, 0]);
	data.extend(body);
	data
}

#[test]
fn a_7z_reparse_point_is_a_link_only_when_its_data_says_so() {
	use sevenz_rust2::{ArchiveEntry as SevenZEntry, ArchiveWriter};
	const REPARSE_POINT: u32 = 0x400;
	let mut writer = ArchiveWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
	let mut push = |name: &str, data: Option<&[u8]>| {
		let mut entry = SevenZEntry::new_file(name);
		entry.has_windows_attributes = true;
		entry.windows_attributes = REPARSE_POINT | 0x20;
		entry.has_stream = data.is_some();
		writer.push_archive_entry(entry, data).unwrap();
	};
	let symlink = reparse_data(0xA000_000C, r"\??\C:\data\file.txt", r"C:\data\file.txt");
	let junction = reparse_data(0xA000_0003, r"\??\C:\data", "");
	let placeholder = pattern(5000, 3);
	push("link", Some(&symlink));
	push("junction", Some(&junction));
	// a file behind a reparse point (a cloud placeholder, say) carries its content
	push("placeholder.bin", Some(&placeholder));
	push("odd.txt", Some(b"not reparse data"));
	push("empty", None);
	let archive = writer.finish().unwrap().into_inner();
	let (seen, end) = run(&archive, "links.7z");
	end.unwrap();
	let link = |ordinal, path: &str, bytes: &[u8], target: &str| {
		Seen::Skipped(
			ordinal,
			path.into(),
			bytes.len() as u64,
			ExtractSkipReason::Symlink {
				target: target.into(),
			},
		)
	};
	assert_eq!(
		seen,
		vec![
			Seen::Opened(StreamLayout::SevenZ),
			link(0, "link", &symlink, r"C:\data\file.txt"),
			// a junction without a print name shows its substitute name, less the NT prefix
			link(1, "junction", &junction, r"C:\data"),
			file(2, "placeholder.bin", &placeholder),
			file(3, "odd.txt", b"not reparse data"),
			file(4, "empty", b""),
		]
	);
}

#[test]
fn damaged_data_is_reported_as_a_damaged_archive() {
	// the data of an unencrypted entry: the decoder, not the source, fails
	let data = pattern(200_000, 1);
	let mut archive = sevenz_of(
		&[("a.bin", Some(&data))],
		SevenZMethod::Deflate { level: 6 },
		false,
		None,
	);
	archive[32 + 50] ^= 0xFF;
	let (_, end) = run(&archive, "a.7z");
	assert_eq!(kind(end), ErrorKind::ArchiveCorrupt);

	let mut zip = zip_of(&[("a.bin", Some(&data))], None);
	// a byte inside the deflate stream, past the local header and name
	zip[30 + 5 + 100] ^= 0xFF;
	let (_, end) = run(&zip, "a.zip");
	assert_eq!(
		kind(end),
		ErrorKind::ArchiveCorrupt,
		"a zip's decoder errors too"
	);
}

#[test]
fn a_filtered_7z_spanning_chunks_decodes() {
	use sevenz_rust2::{ArchiveEntry as SevenZEntry, ArchiveWriter, EncoderMethod};
	// the x86 branch filter over LZMA2, across several of the source's chunks
	let data = pattern(3 * CHUNK_SIZE, 9);
	let mut writer = ArchiveWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
	writer.set_content_methods(vec![
		EncoderMethod::BCJ_X86_FILTER.into(),
		EncoderMethod::LZMA2.into(),
	]);
	writer
		.push_archive_entry(SevenZEntry::new_file("program.exe"), Some(&data[..]))
		.unwrap();
	let archive = writer.finish().unwrap().into_inner();
	let (seen, end) = run(&archive, "program.7z");
	end.unwrap();
	let Some(Seen::File { data: read, .. }) = seen.get(1) else {
		panic!("{seen:?}");
	};
	assert_eq!(*read, data);
}

#[test]
fn data_under_a_tar_directory_is_unaccounted() {
	let mut builder = tar::Builder::new(Vec::new());
	append(
		&mut builder,
		tar::EntryType::Directory,
		"docs/",
		b"not a directory's",
	);
	append(
		&mut builder,
		tar::EntryType::Regular,
		"docs/a.txt",
		b"alpha",
	);
	let tar = builder.into_inner().unwrap();
	let (seen, end) = run(&tar, "sample.tar");
	assert_eq!(seen[1], Seen::Dir(0, "docs".into()));
	assert_eq!(end.unwrap().unaccounted_bytes, 17);
}

/// A zip written by the `zip` crate, directories stored as "files" named with a trailing slash,
/// deflated, as `java.util.zip` and Python write them.
fn zip_with_deflated_dirs(dir_data: &[u8]) -> Vec<u8> {
	use zip8::{CompressionMethod, write::SimpleFileOptions};
	let mut writer = zip8::ZipWriter::new(std::io::Cursor::new(Vec::new()));
	let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
	writer.start_file("docs/", options).unwrap();
	writer.write_all(dir_data).unwrap();
	writer.start_file("docs/a.txt", options).unwrap();
	writer.write_all(b"alpha").unwrap();
	writer.finish().unwrap().into_inner()
}

#[test]
fn an_empty_deflated_directory_is_no_hidden_data() {
	let zip = zip_with_deflated_dirs(b"");
	let mut source = std::io::Cursor::new(&zip[..]);
	let index = crate::fs::archive::zip::read::read_index(
		&mut source,
		zip.len() as u64,
		crate::fs::archive::zip::read::ZipLimits {
			max_index_bytes: 1 << 20,
			max_entries: 10,
		},
	)
	.unwrap();
	assert!(
		index.entries[0].compressed_size > 0,
		"the directory is stored as a deflate stream, as Java writes it"
	);
	let (seen, end) = run(&zip, "java.zip");
	assert_eq!(end.unwrap().unaccounted_bytes, 0, "{seen:?}");
	// a "directory" that stores data hides it
	let (_, end) = run(&zip_with_deflated_dirs(b"not a directory's"), "java.zip");
	assert!(end.unwrap().unaccounted_bytes > 0);
}

#[test]
fn a_wrong_password_on_lzma_entries_reads_as_one() {
	let archive = sevenz_of(
		&[("a.txt", Some(&pattern(10_000, 1)[..]))],
		SevenZMethod::Lzma { level: 1 },
		false,
		Some((SevenZEncryption::Entries, "right")),
	);
	for attempt in 0..16 {
		let wrong = format!("wrong {attempt}");
		let (seen, end) = run_full(&archive, "a.7z", LIMITS, Some(&wrong));
		assert_eq!(kind(end), ErrorKind::ArchiveWrongPassword, "{wrong}");
		assert!(seen.is_empty());
	}
}

#[test]
fn an_empty_7z_entry_proves_no_password() {
	use sevenz_rust2::{
		ArchiveEntry as SevenZEntry, ArchiveWriter, EncoderMethod,
		encoder_options::AesEncoderOptions,
	};
	// sevenz-rust2 gives an empty file added with a reader a data stream of its own; decoding
	// nothing matches its CRC-32 under any key, and LZMA2 reads nothing before its first output
	let data = pattern(64 << 10, 5);
	let mut writer = ArchiveWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
	writer.set_content_methods(vec![
		AesEncoderOptions::new("right".into()).into(),
		EncoderMethod::LZMA2.into(),
	]);
	writer.set_encrypt_header(false);
	writer
		.push_archive_entry(SevenZEntry::new_file("empty.txt"), Some(&b""[..]))
		.unwrap();
	writer
		.push_archive_entry(SevenZEntry::new_file("data.bin"), Some(&data[..]))
		.unwrap();
	let archive = writer.finish().unwrap().into_inner();
	for wrong in ["wrong", "also wrong"] {
		let (seen, end) = run_full(&archive, "empty.7z", LIMITS, Some(wrong));
		assert_eq!(
			end.map(|_| ()).map_err(|error| error.kind()),
			Err(ErrorKind::ArchiveWrongPassword),
			"{seen:?}"
		);
		assert!(seen.is_empty(), "nothing is created: {seen:?}");
	}
	let (seen, end) = run_full(&archive, "empty.7z", LIMITS, Some("right"));
	end.unwrap();
	assert!(
		seen.iter()
			.any(|seen| matches!(seen, Seen::File { data: read, .. } if *read == data)),
		"{seen:?}"
	);
}

/// A zip of stored, ZipCrypto-encrypted files (no data descriptors, so each check byte is the
/// high byte of its CRC-32), built by hand: no writer the tests have writes ZipCrypto.
fn zip_crypto_zip(files: &[(&str, &[u8])], password: &[u8]) -> Vec<u8> {
	use crate::fs::archive::zip::crypto::test_support::zip_crypto_encrypt;
	let mut zip = Vec::new();
	let mut central = Vec::new();
	for (name, data) in files {
		let crc = crc32fast::hash(data);
		let stored = zip_crypto_encrypt(password, (crc >> 24) as u8, data);
		let offset = u32::try_from(zip.len()).unwrap();
		// version needed, flags (encrypted), method (stored), time, date
		let common = [
			&20u16.to_le_bytes()[..],
			&1u16.to_le_bytes(),
			&0u16.to_le_bytes(),
			&0u16.to_le_bytes(),
			&0x21u16.to_le_bytes(),
			&crc.to_le_bytes(),
			&u32::try_from(stored.len()).unwrap().to_le_bytes(),
			&u32::try_from(data.len()).unwrap().to_le_bytes(),
			&u16::try_from(name.len()).unwrap().to_le_bytes(),
			&0u16.to_le_bytes(),
		]
		.concat();
		zip.extend(0x0403_4b50u32.to_le_bytes());
		zip.extend(&common);
		zip.extend(name.as_bytes());
		zip.extend(&stored);
		central.extend(0x0201_4b50u32.to_le_bytes());
		central.extend(20u16.to_le_bytes());
		central.extend(&common);
		// comment length, disk, internal and external attributes, local header offset
		central.extend([0u8; 10]);
		central.extend(offset.to_le_bytes());
		central.extend(name.as_bytes());
	}
	let central_at = u32::try_from(zip.len()).unwrap();
	let count = u16::try_from(files.len()).unwrap().to_le_bytes();
	zip.extend(&central);
	zip.extend(0x0605_4b50u32.to_le_bytes());
	zip.extend([0u8; 4]);
	zip.extend(count);
	zip.extend(count);
	zip.extend(u32::try_from(central.len()).unwrap().to_le_bytes());
	zip.extend(central_at.to_le_bytes());
	zip.extend([0u8; 2]);
	zip
}

#[test]
fn an_empty_zip_crypto_entry_proves_no_password() {
	use crate::fs::archive::zip::crypto::{ZipCryptoReader, test_support::zip_crypto_encrypt};
	// the one file with data is too large to probe, so the entries themselves decide
	let data = pattern((16 << 20) + 1, 7);
	let zip = zip_crypto_zip(&[("empty.txt", b""), ("data.bin", &data)], b"right");
	// a wrong password that both check bytes let through (1 in 65536)
	let passes = |password: &[u8], data: &[u8]| {
		let check = (crc32fast::hash(data) >> 24) as u8;
		let header = zip_crypto_encrypt(b"right", check, &[]);
		ZipCryptoReader::new(&header[..], password, check).is_ok()
	};
	let wrong = (0u32..)
		.map(|attempt| format!("wrong{attempt}"))
		.find(|password| passes(password.as_bytes(), b"") && passes(password.as_bytes(), &data))
		.unwrap();
	let (seen, end) = run_full(&zip, "crypto.zip", LIMITS, Some(&wrong));
	assert_eq!(
		end.map(|_| ()).map_err(|error| error.kind()),
		Err(ErrorKind::ArchiveWrongPassword),
		"{:?}",
		seen.len()
	);
	let (_, end) = run_full(&zip, "crypto.zip", LIMITS, Some("right"));
	end.unwrap();
}

/// A zip whose directory entry is encrypted with AES, as the `zip` crate writes one started as a
/// file named with a trailing slash.
fn zip_with_encrypted_dir(method: zip8::CompressionMethod, dir_data: &[u8]) -> Vec<u8> {
	use zip8::{AesMode, write::SimpleFileOptions};
	let mut writer = zip8::ZipWriter::new(std::io::Cursor::new(Vec::new()));
	let options = SimpleFileOptions::default()
		.compression_method(method)
		.with_aes_encryption(AesMode::Aes256, "pw");
	writer.start_file("docs/", options).unwrap();
	writer.write_all(dir_data).unwrap();
	writer.start_file("docs/a.txt", options).unwrap();
	writer.write_all(b"alpha").unwrap();
	writer.finish().unwrap().into_inner()
}

#[test]
fn an_encrypted_directory_is_judged_by_its_length() {
	use zip8::CompressionMethod;
	for method in [
		CompressionMethod::Stored,
		CompressionMethod::Deflated,
		CompressionMethod::Bzip2,
	] {
		let zip = zip_with_encrypted_dir(method, b"");
		let (seen, end) = run_full(&zip, "aes.zip", LIMITS, Some("pw"));
		assert_eq!(end.unwrap().unaccounted_bytes, 0, "{method:?}: {seen:?}");
		let zip = zip_with_encrypted_dir(method, b"not a directory's");
		let (_, end) = run_full(&zip, "aes.zip", LIMITS, Some("pw"));
		assert!(end.unwrap().unaccounted_bytes > 0, "{method:?}");
	}
}

/// The password of every fixture whose name starts with `encrypted`.
const FIXTURE_PASSWORD: &str = "fixture password";

/// Extracts every archive in `tests/fixtures/archives/<dir>`, made by real tools (see the README
/// there), and checks what the codec sends, the bytes that belong to no entry and the entries no
/// checksum covered against the directory's `manifest.tsv`, which was written from the inputs
/// rather than from what the SDK reads.
fn check_fixtures(dir: &str) {
	let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures/archives")
		.join(dir);
	let manifest = std::fs::read_to_string(root.join("manifest.tsv")).unwrap();
	let mut expected = std::collections::BTreeMap::<&str, Vec<String>>::new();
	for line in manifest.lines().filter(|line| !line.starts_with('#')) {
		let (archive, row) = line.split_once('\t').unwrap();
		expected.entry(archive).or_default().push(row.to_owned());
	}
	for (archive, mut rows) in expected {
		let bytes = std::fs::read(root.join(archive)).unwrap();
		let password = archive.starts_with("encrypted").then_some(FIXTURE_PASSWORD);
		let (seen, end) = run_full(&bytes, archive, LIMITS, password);
		let end = end.unwrap_or_else(|error| panic!("{archive}: {error}"));
		let mut found: Vec<String> = seen
			.into_iter()
			.filter_map(|seen| match seen {
				Seen::Opened(_) => None,
				Seen::Dir(_, path) => Some(format!("dir\t{path}")),
				Seen::File {
					path, data, ended, ..
				} => {
					assert!(ended, "{archive}: {path} did not end");
					Some(format!(
						"file\t{path}\t{}\t{:08x}",
						data.len(),
						crc32fast::hash(&data)
					))
				}
				Seen::Skipped(_, path, _, ExtractSkipReason::Symlink { target }) => {
					Some(format!("symlink\t{path}\t{target}"))
				}
				Seen::Skipped(_, path, _, ExtractSkipReason::Hardlink { target }) => {
					Some(format!("hardlink\t{path}\t{target}"))
				}
				Seen::Skipped(_, path, _, reason) => Some(format!("skip\t{path}\t{reason:?}")),
			})
			.collect();
		if end.unaccounted_bytes > 0 {
			found.push(format!("unaccounted\t\t{}", end.unaccounted_bytes));
		}
		if end.unchecked_entries > 0 {
			found.push(format!("unchecked\t\t{}", end.unchecked_entries));
		}
		rows.sort();
		found.sort();
		assert_eq!(found, rows, "{archive}");
	}
}

#[test]
fn tar_fixtures_extract_to_their_manifest() {
	check_fixtures("tar");
}

#[test]
fn sevenz_fixtures_extract_to_their_manifest() {
	check_fixtures("7z");
}

#[test]
fn stream_fixtures_extract_to_their_manifest() {
	check_fixtures("streams");
}
