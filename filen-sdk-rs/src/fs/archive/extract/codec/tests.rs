//! The codec on its real thread, driven from the test thread: asks answered from an in-memory
//! archive, events collected in order.

use std::{
	io::{Cursor, Write},
	sync::mpsc,
	time::Duration,
};

// the crate, which the codec's own `tar` would shadow through `super::*`
use ::tar;
use ::zip::{AesMode, CompressionMethod, write::SimpleFileOptions};
use chrono::DateTime;
use filen_types::fs::Uuid;
use lz4_flex::frame::{BlockMode, BlockSize, FrameEncoder, FrameInfo};
use sevenz_rust2::{
	ArchiveEntry as SevenZEntry, ArchiveWriter, EncoderMethod, SourceReader,
	encoder_options::AesEncoderOptions,
};

use super::*;
use crate::{
	consts::{CHUNK_SIZE, CHUNK_SIZE_U64},
	fs::{
		archive::{
			decode::{CodecError, SKIPPABLE_FRAME_MAGIC},
			encode::{Compression, open_encoder},
			extract::{
				ArchiveEntry, ArchiveEntryId, ArchiveEntryKind, ExpansionLimit, ListedPath,
				ListedSkipReason, PasswordCheck,
			},
			format::StreamCodec,
			sevenz::{
				SevenZError,
				header::ATTRIBUTE_REPARSE_POINT,
				write::{SevenZEncryption, SevenZMethod},
			},
			tar_iter::TarError,
			test_support::{
				APPLE_DOUBLE, READ_BACK_ZIP, TarMember, archive_password, gzip, incompressible,
				pattern, sevenz_of, skippable_frame, tar_of, tar_with, zip_of, zstd_raw_frame,
			},
			worker,
			zip::{
				crypto::{ZipCryptoReader, test_support::zip_crypto_encrypt},
				read::{ZipError, read_index as zip_read_index},
			},
		},
		name::ValidatedName,
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
	Opened(ArchiveFormat),
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
	/// A hard link at a path, to the file at another.
	Link(u64, String, String),
	Listed(ArchiveEntry),
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
	run_job(
		archive,
		StreamJob {
			name: name.to_owned(),
			len: archive.len() as u64,
			limits,
			password: password.map(archive_password),
			skip_mac_metadata: false,
			task: Task::Extract(None),
		},
	)
}

/// `archive` through the codec running `job`: what it sent, how it ended, and the bytes of the
/// archive it counted as read.
fn run_job(archive: &[u8], job: StreamJob) -> (Vec<Seen>, Result<ArchiveEnd, Error>, u64) {
	run_job_answering(job, |index| {
		let start = usize::try_from(index * CHUNK_SIZE_U64).unwrap();
		let end = (start + CHUNK_SIZE).min(archive.len());
		Some(archive[start..end].to_vec())
	})
}

/// As [`run_job`], each chunk the codec asks for answered by `answer`; `None` drops the ask
/// unanswered, as a driver does when the fetch failed and stopped the job.
fn run_job_answering(
	job: StreamJob,
	mut answer: impl FnMut(u64) -> Option<Vec<u8>>,
) -> (Vec<Seen>, Result<ArchiveEnd, Error>, u64) {
	let mut link = worker::start(move |port| extract_stream(&port, job)).unwrap();
	let mut seen = Vec::new();
	while let Some(event) = link.events.blocking_recv() {
		match event {
			WorkerEvent::Ask { index, reply, .. } => {
				if let Some(chunk) = answer(index) {
					let _ = reply.send(chunk);
				}
			}
			WorkerEvent::Opened(layout) => seen.push(Seen::Opened(layout)),
			WorkerEvent::Entry(head) => seen.push(match head.kind {
				EntryKind::Dir => Seen::Dir(head.ordinal, head.path.joined()),
				EntryKind::File { size } => Seen::File {
					ordinal: head.ordinal,
					path: head.path.joined(),
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
			WorkerEvent::Link(link) => seen.push(Seen::Link(
				link.ordinal(),
				link.path.joined(),
				link.target.joined(),
			)),
			WorkerEvent::Listed(entry) => seen.push(Seen::Listed(*entry)),
			WorkerEvent::Head(_) => panic!("an extracting codec sent a head"),
		}
	}
	// the result follows the events closing, once the codec's thread hands it over
	let result = futures::executor::block_on(&mut link.done).expect("the codec died");
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
		// the driver copies the file it names
		Seen::Link(3, "docs/hard".into(), "docs/a.txt".into()),
		Seen::Skipped(4, "../evil".into(), 4, ExtractSkipReason::UnsafePath),
	]
}

fn kind(result: Result<ArchiveEnd, Error>) -> ErrorKind {
	result.expect_err("the codec should fail").kind()
}

#[test]
fn a_bare_tar_is_sent_member_by_member() {
	let (seen, end) = run(&sample_tar(), "sample.tar");
	let mut expected = vec![Seen::Opened(ArchiveFormat::Tar { codec: None })];
	expected.extend(sample_seen());
	assert_eq!(seen, expected);
	assert_eq!(
		end.unwrap(),
		ArchiveEnd {
			unchecked_entries: 0,
			unaccounted_bytes: 0,
			duplicates: None,
			password: PasswordCheck::NotNeeded,
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
			Seen::Opened(ArchiveFormat::Tar { codec: None }),
			file(0, "a.txt", b"alpha"),
			file(1, "b.txt", b"alpha"),
		]
	);
	assert_eq!(end.unwrap().unaccounted_bytes, 0);
}

#[test]
fn a_file_is_sent_in_whole_chunks() {
	let data = pattern(CHUNK_SIZE + 7, 0);
	let mut builder = tar::Builder::new(Vec::new());
	append(&mut builder, tar::EntryType::Regular, "big.bin", &data);
	let (seen, _) = run(&builder.into_inner().unwrap(), "big.tar");
	assert_eq!(
		seen,
		[
			Seen::Opened(ArchiveFormat::Tar { codec: None }),
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
	let mut expected = vec![Seen::Opened(ArchiveFormat::Tar {
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
			password: PasswordCheck::NotNeeded,
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
		password: PasswordCheck::NotNeeded,
	};
	let (seen, end) = run(&empty, "e.tar");
	assert_eq!(seen, [Seen::Opened(ArchiveFormat::Tar { codec: None })]);
	assert_eq!(end.unwrap(), nothing);
	let (seen, end) = run(&gzip(&empty), "e.tar.gz");
	assert_eq!(
		seen,
		[Seen::Opened(ArchiveFormat::Tar {
			codec: Some(StreamCodec::Gzip)
		})]
	);
	assert_eq!(end.unwrap(), nothing);
}

#[test]
fn zeros_without_a_tars_name_are_a_file_of_zeros() {
	let (seen, _) = run(&gzip(&[0u8; 10 * 1024]), "zeros.gz");
	assert_eq!(
		seen[0],
		Seen::Opened(ArchiveFormat::Single {
			codec: StreamCodec::Gzip
		})
	);
}

#[test]
fn a_zstd_tar_is_read_through_its_frames() {
	let mut encoder = open_encoder(
		Compression {
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
	let mut expected = vec![Seen::Opened(ArchiveFormat::Tar {
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
			Seen::Opened(ArchiveFormat::Single {
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
			password: PasswordCheck::NotNeeded,
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

/// Limits under which 4 MiB of zeros decode past what their archive may expand to.
const BOMB: CodecLimits = CodecLimits {
	expansion: Some(ExpansionLimit {
		ratio: 10,
		floor: 1 << 20,
	}),
	..LIMITS
};

/// Limits under which [`sample_tar`] has too many members.
const FEW_MEMBERS: CodecLimits = CodecLimits {
	max_members: 2,
	..LIMITS
};

#[test]
fn a_stream_decoding_past_its_expansion_limit_is_refused() {
	let zeros = gzip(&vec![0u8; 4 << 20]);
	let (_, end) = run_with(&zeros, "zeros.gz", BOMB);
	assert_eq!(kind(end), ErrorKind::ArchiveTooLarge);
}

#[test]
fn a_tar_of_more_members_than_the_limit_is_refused() {
	let (seen, end) = run_with(&sample_tar(), "sample.tar", FEW_MEMBERS);
	assert_eq!(kind(end), ErrorKind::ArchiveTooLarge);
	assert_eq!(seen.len(), 3, "the members before the limit were sent");
}

#[test]
fn a_decoder_needing_more_memory_than_the_limit_is_refused() {
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

/// Whether the codec failed with a `T` inside its error.
fn failed_with<T: std::error::Error + Send + Sync + 'static>(
	result: Result<ArchiveEnd, Error>,
) -> bool {
	result
		.expect_err("the codec should fail")
		.downcast_ref::<T>()
		.is_some()
}

#[test]
fn a_refusal_keeps_the_readers_own_error() {
	assert!(failed_with::<ZipError>(run(b"PK\x03\x04rest", "a.zip").1));
	assert!(failed_with::<SevenZError>(
		run(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0, 4], "a.7z").1
	));
	let archive = gzip(&sample_tar());
	assert!(failed_with::<CodecError>(
		run(&archive[..archive.len() - 10], "sample.tgz").1
	));
	assert!(failed_with::<TarError>(
		run_with(&sample_tar(), "sample.tar", FEW_MEMBERS).1
	));
	assert!(failed_with::<ExpansionExceeded>(
		run_with(&gzip(&vec![0u8; 4 << 20]), "zeros.gz", BOMB).1
	));
}

#[test]
fn the_codec_stops_once_its_driver_is_gone() {
	let archive = sample_tar();
	let len = archive.len() as u64;
	let (exited, has_exited) = mpsc::channel();
	let mut link = worker::start(move |port| {
		let result = extract_stream(
			&port,
			StreamJob {
				name: "sample.tar".into(),
				len,
				limits: LIMITS,
				password: None,
				skip_mac_metadata: false,
				task: Task::Extract(None),
			},
		);
		let _ = exited.send(());
		result
	})
	.unwrap();
	let Some(WorkerEvent::Ask { reply, .. }) = link.events.blocking_recv() else {
		panic!("the codec asks for the archive first");
	};
	reply.send(archive).unwrap();
	drop(link);
	has_exited
		.recv_timeout(Duration::from_secs(10))
		.expect("the codec thread kept running");
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
		Seen::Opened(ArchiveFormat::Zip),
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
			password: PasswordCheck::NotNeeded,
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
	let zip = zip_of(&[("secret.txt", Some(b"secret"))], Some("right"));
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
			Seen::Opened(ArchiveFormat::Zip),
			file(0, "secret.txt", b"secret")
		]
	);
}

#[test]
fn the_last_of_duplicate_zip_entries_wins() {
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
}

#[test]
fn a_zip_symlink_is_skipped_with_its_target() {
	let mut writer = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
	writer
		.add_symlink(
			"link",
			"target/file",
			::zip::write::SimpleFileOptions::default(),
		)
		.unwrap();
	let linked = writer.finish().unwrap().into_inner();
	let (seen, end) = run(&linked, "l.zip");
	end.unwrap();
	assert_eq!(
		seen,
		[
			Seen::Opened(ArchiveFormat::Zip),
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
}

#[test]
fn a_zip_stating_sizes_past_its_expansion_limit_is_refused_up_front() {
	let zeros = zip_of(&[("zeros", Some(&vec![0u8; 4 << 20]))], None);
	let (seen, end) = run_with(&zeros, "z.zip", BOMB);
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
		Seen::Opened(ArchiveFormat::Zip),
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
				Seen::Opened(ArchiveFormat::SevenZ),
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
				password: PasswordCheck::NotNeeded,
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
				Seen::Opened(ArchiveFormat::SevenZ),
				file(0, "secret.txt", b"secret"),
			]
		);
	}
}

#[test]
fn sevenz_symlinks_and_anti_items_are_skipped() {
	let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
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
			Seen::Opened(ArchiveFormat::SevenZ),
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
	let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
	let mut push = |name: &str, data: Option<&[u8]>| {
		let mut entry = SevenZEntry::new_file(name);
		entry.has_windows_attributes = true;
		entry.windows_attributes = ATTRIBUTE_REPARSE_POINT | 0x20;
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
			Seen::Opened(ArchiveFormat::SevenZ),
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
	// the x86 branch filter over LZMA2, across several of the source's chunks
	let data = pattern(3 * CHUNK_SIZE, 9);
	let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
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
	let mut writer = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
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
	let mut source = Cursor::new(&zip[..]);
	let index = zip_read_index(&mut source, zip.len() as u64, READ_BACK_ZIP).unwrap();
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
	// sevenz-rust2 gives an empty file added with a reader a data stream of its own; decoding
	// nothing matches its CRC-32 under any key, and LZMA2 reads nothing before its first output
	let data = pattern(64 << 10, 5);
	let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
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
	// the one file with data is too large to probe, so the entries themselves decide
	let data = pattern(usize::try_from(PASSWORD_PROBE_BYTES).unwrap() + 1, 7);
	let zip = zip_crypto_zip(&[("empty.txt", b""), ("data.bin", &data)], b"right");
	// a wrong password that both check bytes let through (1 in 65536)
	let passes = |password: &str, data: &[u8]| {
		let check = (crc32fast::hash(data) >> 24) as u8;
		let header = zip_crypto_encrypt(b"right", check, &[]);
		ZipCryptoReader::new(&header[..], &archive_password(password), check).is_ok()
	};
	let wrong = (0u32..)
		.map(|attempt| format!("wrong{attempt}"))
		.find(|password| passes(password, b"") && passes(password, &data))
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
fn zip_with_encrypted_dir(method: ::zip::CompressionMethod, dir_data: &[u8]) -> Vec<u8> {
	let mut writer = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
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
	let manifest = String::from_utf8(fixture(dir, "manifest.tsv")).unwrap();
	let mut expected = std::collections::BTreeMap::<&str, Vec<String>>::new();
	for line in manifest.lines().filter(|line| !line.starts_with('#')) {
		let (archive, row) = line.split_once('\t').unwrap();
		expected.entry(archive).or_default().push(row.to_owned());
	}
	for (archive, rows) in expected {
		check_archive(archive, &fixture(dir, archive), rows);
	}
}

/// Extracts `bytes`, the archive `archive`, and checks what the codec sends, the bytes that belong
/// to no entry and the entries no checksum covered against `rows`, written as a `manifest.tsv`
/// row is, less the archive's name.
fn check_archive(archive: &str, bytes: &[u8], mut rows: Vec<String>) {
	let password = archive.starts_with("encrypted").then_some(FIXTURE_PASSWORD);
	let (seen, end) = run_full(bytes, archive, LIMITS, password);
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
			// the driver extracts it as a copy of its target
			Seen::Link(_, path, target) => Some(format!("hardlink\t{path}\t{target}")),
			Seen::Listed(entry) => panic!("{archive}: an extraction listed {entry:?}"),
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

#[test]
fn tar_fixtures_extract_to_their_manifest() {
	check_fixtures("tar");
}

#[test]
fn sevenz_fixtures_extract_to_their_manifest() {
	check_fixtures("7z");
}

#[test]
fn a_zip_of_zstd_entries_is_extracted() {
	let zip = fixture("zip", "zstd-python.zip");
	let (seen, end) = run(&zip, "zstd-python.zip");
	assert_eq!(end.unwrap().unaccounted_bytes, 0);
	let files: Vec<(String, usize, bool)> = seen
		.into_iter()
		.filter_map(|seen| match seen {
			Seen::File {
				path, data, ended, ..
			} => Some((path, data.len(), ended)),
			_ => None,
		})
		.collect();
	assert_eq!(
		files,
		[
			("hello.txt".to_owned(), 27, true),
			("sub/far.bin".to_owned(), 36_000, true),
			("sub/lines.txt".to_owned(), 4390, true),
		]
	);
}

#[test]
fn stream_fixtures_extract_to_their_manifest() {
	check_fixtures("streams");
}

/// A stream codec's output at `level`, from the crates the SDK decodes it with.
fn gzip_at(data: &[u8], level: u32) -> Vec<u8> {
	let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level));
	encoder.write_all(data).unwrap();
	encoder.finish().unwrap()
}

fn bzip2_at(data: &[u8], level: u32) -> Vec<u8> {
	let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(level));
	encoder.write_all(data).unwrap();
	encoder.finish().unwrap()
}

fn lz4_frame(data: &[u8], info: FrameInfo) -> Vec<u8> {
	let mut encoder = FrameEncoder::with_frame_info(info, Vec::new());
	encoder.write_all(data).unwrap();
	encoder.finish().unwrap()
}

/// The magic numbers of the skippable frames the streams below carry: the first and the last of
/// the range lz4 and zstd share.
const LZ4_SKIPPABLE: u32 = *SKIPPABLE_FRAME_MAGIC.start();
const ZSTD_SKIPPABLE: u32 = *SKIPPABLE_FRAME_MAGIC.end();

#[test]
fn streams_of_several_members_or_frames_extract_to_one_file() {
	let (bin, text) = (incompressible(4096, 7), pattern(2110, 0));
	let both = [&bin[..], &text].concat();
	let file = |name: &str| {
		format!(
			"file\t{name}\t{}\t{:08x}",
			both.len(),
			crc32fast::hash(&both)
		)
	};
	let lz4_metadata = skippable_frame(LZ4_SKIPPABLE, b"lz4 metadata");
	let zstd_metadata = skippable_frame(ZSTD_SKIPPABLE, b"zstd metadata");
	let linked = FrameInfo::new()
		.block_size(BlockSize::Max64KB)
		.block_mode(BlockMode::Linked)
		.content_checksum(true)
		.content_size(Some(bin.len() as u64));
	for (name, archive, rows) in [
		// two members at different levels
		(
			"two-members.bin.gz",
			[gzip_at(&bin, 6), gzip_at(&text, 9)].concat(),
			vec![file("two-members.bin")],
		),
		(
			"two-members.bin.bz2",
			[bzip2_at(&bin, 9), bzip2_at(&text, 1)].concat(),
			vec![file("two-members.bin")],
		),
		// a skippable frame, a frame of linked blocks stating its size, then one without a
		// content checksum: the skippable frame's 20 bytes belong to no file, which one frame
		// leaves unchecked
		(
			"skippable-frames.bin.lz4",
			[
				lz4_metadata,
				lz4_frame(&bin, linked),
				lz4_frame(&text, FrameInfo::new()),
			]
			.concat(),
			vec![
				file("skippable-frames.bin"),
				"unaccounted\t\t20".to_owned(),
				"unchecked\t\t1".to_owned(),
			],
		),
		// skippable frames of 21 bytes around a frame with a checksum and one without
		(
			"frames.bin.zst",
			[
				&zstd_metadata[..],
				&ruzstd::encoding::compress_to_vec(
					&bin[..],
					ruzstd::encoding::CompressionLevel::Fastest,
				),
				&zstd_metadata,
				&zstd_raw_frame(&text, 17, true, None),
			]
			.concat(),
			vec![
				file("frames.bin"),
				"unaccounted\t\t42".to_owned(),
				"unchecked\t\t1".to_owned(),
			],
		),
	] {
		check_archive(name, &archive, rows);
	}
}

/// A `task` over `archive`, named `name`, leaving macOS metadata out when `skip_mac_metadata`.
fn job_of(archive: &[u8], name: &str, skip_mac_metadata: bool, task: Task) -> StreamJob {
	StreamJob {
		name: name.to_owned(),
		len: archive.len() as u64,
		limits: LIMITS,
		password: None,
		skip_mac_metadata,
		task,
	}
}

/// What the codec sent, one line each: what the driver creates or skips, and the links it copies.
fn outline(seen: &[Seen]) -> Vec<String> {
	seen.iter()
		.filter_map(|seen| match seen {
			Seen::Opened(_) => None,
			Seen::Dir(_, path) => Some(format!("dir {path}")),
			Seen::File { path, data, .. } => Some(format!("file {path} {}", data.len())),
			Seen::Skipped(_, path, _, reason) => Some(format!("skip {path} {reason:?}")),
			Seen::Link(_, path, target) => Some(format!("link {path} -> {target}")),
			Seen::Listed(entry) => panic!("an extraction listed {entry:?}"),
		})
		.collect()
}

fn fixture(dir: &str, name: &str) -> Vec<u8> {
	std::fs::read(
		std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
			.join("tests/fixtures/archives")
			.join(dir)
			.join(name),
	)
	.unwrap()
}

/// An AppleDouble file's first bytes, then whatever it holds.
fn apple_double_data() -> Vec<u8> {
	[&APPLE_DOUBLE[..], b"Mac OS X        "].concat()
}

#[test]
fn bsdtars_apple_double_member_is_left_out_only_when_asked() {
	// bsdtar's AppleDouble member, ahead of the file it belongs to; its size depends on the
	// attributes macOS gave the file it was made from, so it is taken from the manifest written
	// along with the archive
	let tar = fixture("tar", "appledouble.tar");
	let manifest = String::from_utf8(fixture("tar", "manifest.tsv")).unwrap();
	let apple_double_size = manifest
		.lines()
		.find_map(|row| row.strip_prefix("appledouble.tar\tfile\t._note.txt\t"))
		.and_then(|rest| rest.split('\t').next())
		.unwrap();
	for (skip, expected) in [
		(
			true,
			[
				"skip ._note.txt MacMetadata".to_owned(),
				"file note.txt 39".to_owned(),
			],
		),
		(
			false,
			[
				format!("file ._note.txt {apple_double_size}"),
				"file note.txt 39".to_owned(),
			],
		),
	] {
		let (seen, end, _) = run_job(
			&tar,
			job_of(&tar, "appledouble.tar", skip, Task::Extract(None)),
		);
		end.unwrap();
		assert_eq!(outline(&seen), expected, "{skip}");
	}
}

#[test]
fn a_dot_underscore_name_alone_is_no_apple_double() {
	// the file is sent whole, the bytes read to tell included
	let tar = tar_of(&[("._notes.txt", b"just text")]);
	let (seen, end, _) = run_job(&tar, job_of(&tar, "a.tar", true, Task::Extract(None)));
	end.unwrap();
	assert_eq!(seen[1..], [file(0, "._notes.txt", b"just text")]);
}

/// A zip and a 7z of a file, its AppleDouble twin, a `._` file of plain text and a `._` file
/// with AppleDouble's magic but another version.
fn apple_double_archives() -> [(&'static str, Vec<u8>); 2] {
	let data = apple_double_data();
	let other_version = [&data[..4], b"\x00\x01\x00\x00"].concat();
	let entries = [
		("a.txt", Some(&b"a"[..])),
		("._a.txt", Some(&data[..])),
		("._b.txt", Some(&b"b"[..])),
		("._c.txt", Some(&other_version[..])),
	];
	[
		("m.zip", zip_of(&entries, None)),
		(
			"m.7z",
			sevenz_of(&entries, SevenZMethod::Lzma2 { level: 1 }, true, None),
		),
	]
}

#[test]
fn a_zip_or_7z_apple_double_file_is_told_by_its_magic_and_version() {
	for (name, archive) in apple_double_archives() {
		let (seen, end, _) = run_job(&archive, job_of(&archive, name, true, Task::Extract(None)));
		end.unwrap();
		assert_eq!(
			outline(&seen),
			[
				"file a.txt 1",
				"skip ._a.txt MacMetadata",
				"file ._b.txt 1",
				"file ._c.txt 8"
			],
			"{name}"
		);
	}
}

#[test]
fn a_zip_or_7z_listing_reads_the_bytes_an_extraction_tells_apple_double_by() {
	for (name, archive) in apple_double_archives() {
		let (shown, end) = listed(
			&archive,
			job_of(&archive, name, true, Task::List { archive: LISTED }),
		);
		end.unwrap();
		assert_eq!(
			shown
				.iter()
				.map(|entry| (entry.stored_path.as_str(), entry.skip))
				.collect::<Vec<_>>(),
			[
				("a.txt", None),
				("._a.txt", Some(ListedSkipReason::MacMetadata)),
				("._b.txt", None),
				("._c.txt", None),
			],
			"{name}"
		);
	}
}

#[test]
fn every_entry_in_finders_mac_folder_is_left_out() {
	let zip = fixture("zip", "finder-ditto.zip");
	let (seen, end, _) = run_job(&zip, job_of(&zip, "finder.zip", true, Task::Extract(None)));
	end.unwrap();
	let outline = outline(&seen);
	let (mac, rest): (Vec<&String>, Vec<&String>) =
		outline.iter().partition(|line| line.contains("__MACOSX"));
	assert_eq!(mac.len(), 8);
	assert!(
		mac.iter().all(|line| line.ends_with("MacMetadata")),
		"{mac:?}"
	);
	assert!(
		rest.iter().all(|line| !line.ends_with("MacMetadata")),
		"{rest:?}"
	);
}

#[test]
fn a_finder_zip_listing_tells_the_apple_double_files_an_extraction_leaves_out() {
	// the 3 files an extraction creates, and the 5 it leaves out
	let zip = fixture("zip", "finder-ditto.zip");
	let (entries, end) = listed(
		&zip,
		job_of(&zip, "finder.zip", true, Task::List { archive: LISTED }),
	);
	end.unwrap();
	let files = |skip: Option<ListedSkipReason>| {
		entries
			.iter()
			.filter(|entry| entry.kind == ArchiveEntryKind::File && entry.skip == skip)
			.count()
	};
	assert_eq!(files(None), 3, "{entries:?}");
	assert_eq!(files(Some(ListedSkipReason::MacMetadata)), 5, "{entries:?}");
}

/// What a listing of `archive` sent: its entries, and how it ended.
fn listed(archive: &[u8], job: StreamJob) -> (Vec<ArchiveEntry>, Result<ArchiveEnd, Error>) {
	let (seen, end, _) = run_job(archive, job);
	let entries = seen
		.into_iter()
		.filter_map(|seen| match seen {
			Seen::Opened(_) => None,
			Seen::Listed(entry) => Some(entry),
			other => panic!("a listing sent {other:?}"),
		})
		.collect();
	(entries, end)
}

/// The archive the listing tests list.
const LISTED: Uuid = Uuid::from_u128(0x15);

/// An entry of [`LISTED`], extracted to `path` (none when it cannot be), with no flag set.
fn listed_entry(
	index: u32,
	stored: &str,
	path: Option<&str>,
	kind: ArchiveEntryKind,
	size: Option<u64>,
	skip: Option<ListedSkipReason>,
) -> ArchiveEntry {
	ArchiveEntry {
		id: ArchiveEntryId {
			archive: LISTED,
			index,
		},
		stored_path: stored.to_owned(),
		stored_path_truncated: false,
		path: path.map(ListedPath::plain),
		kind,
		size,
		modified: None,
		encrypted: false,
		method: None,
		skip,
		mac_metadata: false,
	}
}

#[test]
fn a_tar_is_listed_member_by_member_without_its_data() {
	let tar = sample_tar();
	let (entries, end) = listed(
		&tar,
		job_of(&tar, "s.tar", true, Task::List { archive: LISTED }),
	);
	assert_eq!(end.unwrap().password, PasswordCheck::NotNeeded);
	let modified = DateTime::from_timestamp(1_700_000_000, 0);
	let entries: Vec<ArchiveEntry> = entries
		.into_iter()
		.map(|entry| {
			assert_eq!(entry.modified, modified, "{entry:?}");
			ArchiveEntry {
				modified: None,
				..entry
			}
		})
		.collect();
	assert_eq!(
		entries,
		[
			listed_entry(0, "docs/", Some("docs"), ArchiveEntryKind::Dir, None, None),
			listed_entry(
				1,
				"docs/a.txt",
				Some("docs/a.txt"),
				ArchiveEntryKind::File,
				Some(5),
				None
			),
			listed_entry(
				2,
				"docs/link",
				Some("docs/link"),
				ArchiveEntryKind::Symlink {
					target: "a.txt".into()
				},
				Some(0),
				// the target is the kind's
				Some(ListedSkipReason::Symlink),
			),
			// extracted as a copy of the file it names, at that file's size
			listed_entry(
				3,
				"docs/hard",
				Some("docs/hard"),
				ArchiveEntryKind::Hardlink {
					target: "docs/a.txt".into(),
					target_id: Some(ArchiveEntryId {
						archive: LISTED,
						index: 1,
					}),
				},
				Some(5),
				None,
			),
			listed_entry(
				4,
				"../evil",
				None,
				ArchiveEntryKind::File,
				Some(4),
				Some(ListedSkipReason::UnsafePath),
			),
		]
	);
}

#[test]
fn a_listed_hard_link_to_a_file_not_extracted_is_skipped() {
	// it has nothing to be a copy of
	let mut builder = tar::Builder::new(Vec::new());
	let mut hard = header(tar::EntryType::Link, 0);
	builder.append_link(&mut hard, "hard", "gone.txt").unwrap();
	let tar = builder.into_inner().unwrap();
	let (entries, _) = listed(
		&tar,
		job_of(&tar, "h.tar", true, Task::List { archive: LISTED }),
	);
	assert_eq!(
		(&entries[0].kind, &entries[0].skip),
		(
			&ArchiveEntryKind::Hardlink {
				target: "gone.txt".into(),
				target_id: None,
			},
			&Some(ListedSkipReason::Hardlink)
		)
	);
}

#[test]
fn a_listed_apple_double_member_is_told_by_its_data_and_skipped_only_when_asked() {
	let tar = fixture("tar", "appledouble.tar");
	for skip in [true, false] {
		let (entries, _) = listed(
			&tar,
			job_of(&tar, "a.tar", skip, Task::List { archive: LISTED }),
		);
		assert!(entries[0].mac_metadata && !entries[1].mac_metadata);
		assert_eq!(
			entries[0].skip,
			skip.then_some(ListedSkipReason::MacMetadata)
		);
	}
}

#[test]
fn a_listed_file_in_a_mac_folder_is_no_metadata_when_its_data_is_ordinary() {
	// the folder, listed once every entry was, is created for it as an extraction creates it
	let tar = tar_of(&[("__MACOSX/", b""), ("__MACOSX/notes.txt", b"plain")]);
	let (entries, _) = listed(
		&tar,
		job_of(&tar, "m.tar", true, Task::List { archive: LISTED }),
	);
	assert_eq!(
		entries
			.iter()
			.map(|entry| (entry.stored_path.as_str(), entry.mac_metadata, entry.skip))
			.collect::<Vec<_>>(),
		[
			("__MACOSX/notes.txt", false, None),
			("__MACOSX/", true, None)
		]
	);
}

/// A zip of `links` symlinks to `target`, each followed by a chunk of data: a link in every
/// chunk.
fn zip_of_spread_links(links: usize) -> Vec<u8> {
	let stored = ::zip::write::SimpleFileOptions::default()
		.compression_method(::zip::CompressionMethod::Stored);
	let mut writer = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
	for link in 0..links {
		writer
			.add_symlink(format!("link{link}"), "target", stored)
			.unwrap();
		writer.start_file(format!("filler{link}"), stored).unwrap();
		writer
			.write_all(&incompressible(CHUNK_SIZE, link as u64 + 1))
			.unwrap();
	}
	writer.finish().unwrap().into_inner()
}

#[test]
fn a_zip_listing_stops_reading_symlink_targets_at_a_failed_fetch() {
	let zip = zip_of_spread_links(8);
	// the index, at the end, is read; every link's chunk before it fails to fetch
	let index_from = (zip.len() as u64 - 4096) / CHUNK_SIZE_U64;
	let mut failed = 0;
	let (_, end, _) = run_job_answering(
		job_of(&zip, "l.zip", true, Task::List { archive: LISTED }),
		|index| {
			if index < index_from {
				failed += 1;
				return None;
			}
			let start = usize::try_from(index * CHUNK_SIZE_U64).unwrap();
			Some(zip[start..(start + CHUNK_SIZE).min(zip.len())].to_vec())
		},
	);
	// the listing ends with the job, asking no more of the source
	assert_eq!(kind(end), ErrorKind::Cancelled);
	assert_eq!(failed, 1);
}

#[test]
fn a_zip_listing_stops_reading_symlink_targets_past_its_budget() {
	// a link in every chunk: reading all their targets would fetch the whole archive
	let links = usize::try_from(LIST_READ_BYTES).unwrap() / CHUNK_SIZE + 4;
	let zip = zip_of_spread_links(links);
	let (entries, end) = listed(
		&zip,
		job_of(&zip, "l.zip", true, Task::List { archive: LISTED }),
	);
	end.unwrap();
	// read in the order they are stored until the budget is spent, the rest listed unread
	let read: Vec<bool> = entries
		.iter()
		.filter_map(|entry| match &entry.kind {
			ArchiveEntryKind::Symlink { target } => Some(target == "target"),
			_ => None,
		})
		.collect();
	assert_eq!(read.len(), links);
	let unread = read.iter().position(|read| !read).unwrap();
	assert!(unread > 1, "{read:?}");
	assert!(read[unread..].iter().all(|read| !read), "{read:?}");
}

/// A zip of one deflated symlink to `target`, its stream padded with `padding` empty stored
/// blocks: a few bytes of target stated in as much archive as the padding takes. Built by hand,
/// as no writer pads a stream.
fn zip_of_padded_symlink(target: &[u8], padding: usize) -> Vec<u8> {
	// an empty non-final stored block, then the target in a final one
	let mut stream = [0u8, 0, 0, 0xff, 0xff].repeat(padding);
	let len = u16::try_from(target.len()).unwrap();
	stream.push(1);
	stream.extend(len.to_le_bytes());
	stream.extend((!len).to_le_bytes());
	stream.extend(target);
	let name = b"link";
	// version needed, flags, method (deflate), time, date, CRC-32, sizes, name and extra length
	let common = [
		&20u16.to_le_bytes()[..],
		&0u16.to_le_bytes(),
		&8u16.to_le_bytes(),
		&0u16.to_le_bytes(),
		&0x21u16.to_le_bytes(),
		&crc32fast::hash(target).to_le_bytes(),
		&u32::try_from(stream.len()).unwrap().to_le_bytes(),
		&u32::try_from(target.len()).unwrap().to_le_bytes(),
		&u16::try_from(name.len()).unwrap().to_le_bytes(),
		&0u16.to_le_bytes(),
	]
	.concat();
	let mut zip = Vec::new();
	zip.extend(0x0403_4b50u32.to_le_bytes());
	zip.extend(&common);
	zip.extend(name);
	zip.extend(&stream);
	let mut central = Vec::new();
	central.extend(0x0201_4b50u32.to_le_bytes());
	// made by Unix, so the mode in the external attributes counts
	central.extend(0x0314u16.to_le_bytes());
	central.extend(&common);
	// comment length, disk, internal attributes
	central.extend([0u8; 6]);
	central.extend((0o120_777u32 << 16).to_le_bytes());
	// the local header's offset
	central.extend(0u32.to_le_bytes());
	central.extend(name);
	let central_at = u32::try_from(zip.len()).unwrap();
	zip.extend(&central);
	zip.extend(0x0605_4b50u32.to_le_bytes());
	zip.extend([0u8; 4]);
	zip.extend(1u16.to_le_bytes());
	zip.extend(1u16.to_le_bytes());
	zip.extend(u32::try_from(central.len()).unwrap().to_le_bytes());
	zip.extend(central_at.to_le_bytes());
	zip.extend([0u8; 2]);
	zip
}

#[test]
fn a_zip_listing_reads_no_padded_symlink_past_its_budget() {
	// 6 bytes of target in 40 MiB of deflate stream: its stated size is short, and reading it
	// would fetch the whole archive
	let zip = zip_of_padded_symlink(b"target", (40 << 20) / 5);
	let (seen, end, read) = run_job(
		&zip,
		job_of(&zip, "l.zip", true, Task::List { archive: LISTED }),
	);
	end.unwrap();
	assert!(
		matches!(
			&seen[..],
			[Seen::Opened(_), Seen::Listed(ArchiveEntry { kind: ArchiveEntryKind::Symlink { target }, .. })]
				if target.is_empty()
		),
		"{seen:?}"
	);
	// the chunks holding the index, at the archive's end, and at most the listing's budget
	// besides
	const INDEX_CHUNKS: usize = 2;
	let fetched = usize::try_from(read.div_ceil(CHUNK_SIZE_U64)).unwrap();
	assert!(
		fetched <= INDEX_CHUNKS + usize::try_from(LIST_READ_BYTES).unwrap() / CHUNK_SIZE,
		"{fetched} chunks fetched"
	);
}

#[test]
fn a_zip_listing_reads_a_short_symlink_target_through_its_padding() {
	// a target of a few bytes in a short stream
	let zip = zip_of_padded_symlink(b"target", 4);
	let (entries, end) = listed(
		&zip,
		job_of(&zip, "l.zip", true, Task::List { archive: LISTED }),
	);
	end.unwrap();
	assert!(
		matches!(&entries[..], [ArchiveEntry { kind: ArchiveEntryKind::Symlink { target }, .. }] if target == "target"),
		"{entries:?}"
	);
}

#[test]
fn a_zip_is_listed_from_its_index_with_its_password_checked() {
	let data = apple_double_data();
	let zip = zip_of(
		&[
			("docs", None),
			("docs/a.txt", Some(b"alpha")),
			("._a.txt", Some(&data)),
		],
		Some("pw"),
	);
	for (password, check) in [
		(Some("pw"), PasswordCheck::Right),
		(Some("nope"), PasswordCheck::Wrong),
		(None, PasswordCheck::Required),
	] {
		let job = StreamJob {
			password: password.map(archive_password),
			..job_of(&zip, "z.zip", true, Task::List { archive: LISTED })
		};
		let (entries, end) = listed(&zip, job);
		assert_eq!(end.unwrap().password, check, "{password:?}");
		let shown: Vec<_> = entries
			.iter()
			.map(|entry| {
				(
					entry.path.as_ref().map(|path| path.path.as_str()),
					entry.encrypted,
					entry.method.as_deref(),
					entry.mac_metadata,
					entry.skip.as_ref(),
				)
			})
			.collect();
		assert_eq!(
			shown,
			[
				(Some("docs"), false, None, false, None),
				(Some("docs/a.txt"), true, Some("Deflate"), false, None),
				// told by its name alone: a listing reads no entry's data, and an extraction
				// checks it
				(Some("._a.txt"), true, Some("Deflate"), true, None),
			]
		);
	}

	let ppmd = fixture("zip", "ppmd.zip");
	let (entries, _) = listed(
		&ppmd,
		job_of(&ppmd, "p.zip", true, Task::List { archive: LISTED }),
	);
	// 7-Zip stores what PPMd would not shrink
	let methods: Vec<(&str, Option<&ListedSkipReason>)> = entries
		.iter()
		.filter_map(|entry| Some((entry.method.as_deref()?, entry.skip.as_ref())))
		.collect();
	assert!(methods.contains(&("PPMd", Some(&ListedSkipReason::UnsupportedMethod))));
	assert!(methods.contains(&("Stored", None)));
}

#[test]
fn a_7z_is_listed_from_its_index() {
	let entries = [("docs", None), ("docs/a.txt", Some(&b"alpha"[..]))];
	let sevenz = sevenz_of(
		&entries,
		SevenZMethod::Lzma2 { level: 1 },
		true,
		Some((SevenZEncryption::EntriesAndHeaders, "pw")),
	);
	let job = |password: Option<&str>| StreamJob {
		password: password.map(archive_password),
		..job_of(&sevenz, "s.7z", true, Task::List { archive: LISTED })
	};
	let (listed_entries, end) = listed(&sevenz, job(Some("pw")));
	assert_eq!(end.unwrap().password, PasswordCheck::Right);
	assert_eq!(
		listed_entries
			.iter()
			.map(|entry| (
				entry.path.as_ref().map(|path| path.path.as_str()),
				entry.encrypted,
				entry.size
			))
			.collect::<Vec<_>>(),
		// in the index's order, which the SDK's writer gives files first
		[
			(Some("docs/a.txt"), true, Some(5)),
			(Some("docs"), false, None)
		]
	);
	assert!(
		listed_entries[0]
			.method
			.as_deref()
			.is_some_and(|method| method.contains("LZMA2") && method.contains("7zAES")),
		"{:?}",
		listed_entries[0].method
	);
	// an encrypted index names nothing without the password
	let (listed_entries, end) = listed(&sevenz, job(None));
	assert!(listed_entries.is_empty());
	assert_eq!(kind(end), ErrorKind::ArchivePasswordRequired);
}

#[test]
fn a_single_compressed_file_is_listed_at_the_size_it_decodes_to() {
	let data = pattern(3 * CHUNK_SIZE / 2, 2);
	let gz = gzip(&data);
	let (entries, end) = listed(
		&gz,
		job_of(&gz, "note.txt.gz", true, Task::List { archive: LISTED }),
	);
	end.unwrap();
	assert_eq!(
		entries,
		[listed_entry(
			0,
			"note.txt",
			Some("note.txt"),
			ArchiveEntryKind::File,
			Some(data.len() as u64),
			None
		)]
	);
}

/// A partial extraction of the entries at `ordinals`, relative to `base`.
fn chosen(ordinals: &[u64], base: &[&str]) -> Task {
	Task::Extract(Some(Selection::new(
		ordinals.iter().copied(),
		base.iter()
			.map(|segment| ValidatedName::try_from(*segment).unwrap())
			.collect(),
	)))
}

#[test]
fn a_partial_extraction_sends_what_was_chosen_below_its_base() {
	let tar = sample_tar();
	for (task, expected) in [
		// a file alone: the directories holding it are the driver's to create
		(chosen(&[1], &[]), vec!["file docs/a.txt 5"]),
		// a directory with everything below it
		(
			chosen(&[0], &[]),
			vec![
				"dir docs",
				"file docs/a.txt 5",
				"skip docs/link Symlink { target: \"a.txt\" }",
				"link docs/hard -> docs/a.txt",
			],
		),
		// what is in a directory, as the root's
		(
			chosen(&[0], &["docs"]),
			vec![
				"file a.txt 5",
				"skip docs/link Symlink { target: \"a.txt\" }",
				"link hard -> a.txt",
			],
		),
		// a chosen entry skipped for its path is reported so
		(chosen(&[4], &[]), vec!["skip ../evil UnsafePath"]),
	] {
		let (seen, end, _) = run_job(&tar, job_of(&tar, "s.tar", true, task.clone()));
		end.unwrap();
		assert_eq!(outline(&seen), expected, "{task:?}");
	}
}

#[test]
fn a_chosen_entry_a_tar_does_not_hold_is_found_missing_at_its_end() {
	let tar = sample_tar();
	let (seen, end, _) = run_job(&tar, job_of(&tar, "s.tar", true, chosen(&[1, 99], &[])));
	assert_eq!(outline(&seen), ["file docs/a.txt 5"]);
	assert_eq!(kind(end), ErrorKind::InvalidState);
}

#[test]
fn a_zip_missing_a_chosen_entry_sends_nothing() {
	// its entries are known up front: nothing is sent, not even what it is
	let (zip, _) = zip_sample();
	for task in [chosen(&[1, 99], &[]), chosen(&[1], &["other"])] {
		let (seen, end, _) = run_job(&zip, job_of(&zip, "z.zip", true, task.clone()));
		assert!(seen.is_empty(), "{task:?}: {seen:?}");
		assert_eq!(kind(end), ErrorKind::InvalidState, "{task:?}");
	}
}

#[test]
fn what_is_below_a_chosen_zip_directory_is_chosen_wherever_it_is_stored() {
	let zip = zip_of(
		&[
			("docs/a.txt", Some(b"a")),
			("docs", None),
			("b.txt", Some(b"b")),
		],
		None,
	);
	let (seen, end, _) = run_job(&zip, job_of(&zip, "z.zip", true, chosen(&[1], &[])));
	end.unwrap();
	assert_eq!(outline(&seen), ["file docs/a.txt 1", "dir docs"]);
}

/// A solid 7z of a file of the bytes `before`, then a symlink to `target`.
fn sevenz_link_after(before: &[u8], target: &[u8]) -> Vec<u8> {
	let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
	let mut link = SevenZEntry::new_file("link");
	link.has_windows_attributes = true;
	link.windows_attributes = 0x8000 | (0o120_777 << 16);
	writer
		.push_archive_entries(
			vec![SevenZEntry::new_file("zeros"), link],
			vec![
				SourceReader::new(Box::new(before) as Box<dyn Read>),
				SourceReader::new(Box::new(target) as Box<dyn Read>),
			],
		)
		.unwrap();
	writer.finish().unwrap().into_inner()
}

#[test]
fn a_7z_link_is_listed_unread_past_what_a_listing_decodes() {
	let target = |archive: &[u8]| {
		let (entries, end) = listed(
			archive,
			job_of(archive, "l.7z", true, Task::List { archive: LISTED }),
		);
		end.unwrap();
		entries[1].kind.clone()
	};
	assert_eq!(
		target(&sevenz_link_after(&[0; 10], b"there")),
		ArchiveEntryKind::Symlink {
			target: "there".into()
		}
	);
	// past what a listing decodes into its solid block, the link would cost decoding all that
	// first
	let past = usize::try_from(LIST_READ_BYTES).unwrap() + (1 << 20);
	assert_eq!(
		target(&sevenz_link_after(&vec![0; past], b"there")),
		ArchiveEntryKind::Symlink {
			target: String::new()
		}
	);
}

#[test]
fn a_7z_listing_ends_with_its_sources_failure_reading_a_link() {
	// the link's data sits 3 MiB into its folder, past chunks the index is not in
	let sevenz = sevenz_link_after(&incompressible(3 << 20, 0x7), b"there");
	let index_from = (sevenz.len() as u64 - 1) / CHUNK_SIZE_U64;
	// once the index was read, every chunk before it fails to fetch
	let mut index_read = false;
	let (_, end, _) = run_job_answering(
		job_of(&sevenz, "l.7z", true, Task::List { archive: LISTED }),
		|index| {
			if index >= index_from {
				index_read = true;
			} else if index_read {
				return None;
			}
			let start = usize::try_from(index * CHUNK_SIZE_U64).unwrap();
			Some(sevenz[start..(start + CHUNK_SIZE).min(sevenz.len())].to_vec())
		},
	);
	assert!(index_read);
	assert_eq!(kind(end), ErrorKind::Cancelled);
}

#[test]
fn a_7z_listing_says_whether_the_password_opens_its_entries() {
	let entries = [("a.txt", Some(&b"alpha"[..]))];
	let sevenz = sevenz_of(
		&entries,
		SevenZMethod::Lzma2 { level: 1 },
		true,
		Some((SevenZEncryption::Entries, "pw")),
	);
	let big = incompressible(
		usize::try_from(PASSWORD_PROBE_BYTES).unwrap() + (1 << 20),
		0x3,
	);
	// the only encrypted entry is too large to check the password on
	let unchecked = sevenz_of(
		&[("big.bin", Some(&big[..]))],
		SevenZMethod::Copy,
		true,
		Some((SevenZEncryption::Entries, "pw")),
	);
	for (archive, password, check) in [
		(&sevenz, "pw", PasswordCheck::Right),
		(&sevenz, "nope", PasswordCheck::Wrong),
		(&unchecked, "pw", PasswordCheck::Unchecked),
	] {
		let job = StreamJob {
			password: Some(archive_password(password)),
			..job_of(archive, "s.7z", true, Task::List { archive: LISTED })
		};
		let (entries, end) = listed(archive, job);
		assert_eq!(end.unwrap().password, check, "{password}");
		assert_eq!(entries.len(), 1);
		assert!(entries[0].encrypted);
	}
}

#[test]
fn a_file_chosen_at_the_base_is_refused_before_anything_is_sent() {
	let zip = zip_of(&[("docs", Some(b"a file named as the base"))], None);
	let (seen, end, _) = run_job(&zip, job_of(&zip, "z.zip", true, chosen(&[0], &["docs"])));
	assert!(seen.is_empty(), "{seen:?}");
	assert_eq!(kind(end), ErrorKind::InvalidState);
}

#[test]
fn a_tar_directory_chosen_brings_what_is_stored_after_it() {
	// a file stored before its directory is gone by the time the directory is reached
	let tar = tar_of(&[
		("docs/early.txt", b"e"),
		("docs/", b""),
		("docs/late.txt", b"l"),
	]);
	let (seen, end, _) = run_job(&tar, job_of(&tar, "t.tar", true, chosen(&[1], &[])));
	end.unwrap();
	assert_eq!(outline(&seen), ["dir docs", "file docs/late.txt 1"]);
}

#[test]
fn hard_links_are_looked_up_by_a_key_each_job_draws() {
	let path = entry_path("docs/a.txt").unwrap();
	let keys = LinkKeys::new();
	// within one job a link and the file it names meet
	assert_eq!(keys.of(&path), keys.of(&entry_path("docs/a.txt").unwrap()));
	assert_ne!(keys.of(&path), keys.of(&entry_path("docs/b.txt").unwrap()));
	// across jobs the same path hashes apart: no archive can pick paths that collide
	assert_ne!(keys.of(&path), LinkKeys::new().of(&path));
}

#[test]
fn a_listed_hard_link_to_mac_metadata_is_metadata() {
	let data = apple_double_data();
	let tar = tar_with(&[
		TarMember::Data("a.txt", b"alpha"),
		TarMember::Data("__MACOSX/._a.txt", &data),
		TarMember::HardLink {
			path: "__MACOSX/._b.txt",
			target: "__MACOSX/._a.txt",
		},
		TarMember::HardLink {
			path: "__MACOSX/copy.txt",
			target: "a.txt",
		},
	]);
	let (entries, end) = listed(
		&tar,
		job_of(&tar, "m.tar", true, Task::List { archive: LISTED }),
	);
	end.unwrap();
	assert_eq!(
		entries
			.iter()
			.map(|entry| (entry.stored_path.as_str(), entry.mac_metadata, entry.skip))
			.collect::<Vec<_>>(),
		[
			("a.txt", false, None),
			(
				"__MACOSX/._a.txt",
				true,
				Some(ListedSkipReason::MacMetadata)
			),
			(
				"__MACOSX/._b.txt",
				true,
				Some(ListedSkipReason::MacMetadata)
			),
			("__MACOSX/copy.txt", false, None),
		]
	);
}

/// The listing's skip of every entry of `tar`, by stored path.
fn listed_skips(tar: &[u8]) -> Vec<(String, Option<ListedSkipReason>)> {
	let (entries, end) = listed(
		tar,
		job_of(tar, "s.tar", true, Task::List { archive: LISTED }),
	);
	end.unwrap();
	entries
		.into_iter()
		.map(|entry| (entry.stored_path, entry.skip))
		.collect()
}

#[test]
fn a_hard_link_to_metadata_stored_over_a_file_is_left_out() {
	// a plain `._a`, then AppleDouble metadata at the same path: the link names the metadata
	let data = apple_double_data();
	let tar = tar_with(&[
		TarMember::Data("._a", b"plain"),
		TarMember::Data("._a", &data),
		TarMember::HardLink {
			path: "b",
			target: "._a",
		},
	]);
	let (seen, end, _) = run_job(&tar, job_of(&tar, "s.tar", true, Task::Extract(None)));
	end.unwrap();
	assert_eq!(
		outline(&seen),
		["file ._a 5", "skip ._a MacMetadata", "skip b MacMetadata"]
	);
	let skips = listed_skips(&tar);
	assert_eq!(
		skips[2],
		("b".to_owned(), Some(ListedSkipReason::MacMetadata))
	);
}

#[test]
fn a_hard_link_names_the_last_member_at_its_target() {
	// `a` is a file, then a symlink: a link to `a` after it has no file to copy
	let tar = tar_with(&[
		TarMember::Data("a", b"alpha"),
		TarMember::Symlink {
			path: "a",
			target: "elsewhere",
		},
		TarMember::HardLink {
			path: "b",
			target: "a",
		},
		TarMember::Data("a", b"again"),
		TarMember::HardLink {
			path: "c",
			target: "a",
		},
	]);
	let (seen, end, _) = run_job(&tar, job_of(&tar, "s.tar", true, Task::Extract(None)));
	end.unwrap();
	assert_eq!(
		outline(&seen),
		[
			"file a 5",
			"skip a Symlink { target: \"elsewhere\" }",
			"skip b Hardlink { target: \"a\" }",
			"file a 5",
			"link c -> a",
		]
	);
	let skips = listed_skips(&tar);
	assert_eq!(skips[2], ("b".to_owned(), Some(ListedSkipReason::Hardlink)));
	assert_eq!(skips[4], ("c".to_owned(), None));
}

#[test]
fn a_listing_tells_the_mac_folders_an_extraction_creates() {
	let data = apple_double_data();
	let entries = [
		("__MACOSX/", None),
		("__MACOSX/meta/", None),
		("__MACOSX/meta/._a.txt", Some(&data[..])),
		("__MACOSX/empty/", None),
		("__MACOSX/mine/", None),
		("__MACOSX/mine/notes.txt", Some(&b"plain"[..])),
	];
	let tar_members: Vec<(&str, &[u8])> = entries
		.iter()
		.map(|&(path, data)| (path, data.unwrap_or_default()))
		.collect();
	for (name, archive) in [
		("m.tar", tar_of(&tar_members)),
		("m.zip", zip_of(&entries, None)),
		(
			"m.7z",
			sevenz_of(&entries, SevenZMethod::Lzma2 { level: 1 }, true, None),
		),
	] {
		let (seen, end, _) = run_job(&archive, job_of(&archive, name, true, Task::Extract(None)));
		end.unwrap();
		let created: Vec<String> = seen
			.iter()
			.filter_map(|seen| match seen {
				Seen::Dir(_, path) => Some(path.clone()),
				_ => None,
			})
			.collect();
		let (listed, end) = listed(
			&archive,
			job_of(&archive, name, true, Task::List { archive: LISTED }),
		);
		end.unwrap();
		let mut listed_created: Vec<String> = listed
			.iter()
			.filter(|entry| entry.kind == ArchiveEntryKind::Dir && entry.skip.is_none())
			.filter_map(|entry| entry.path.clone().map(|path| path.path))
			.collect();
		let mut created = created;
		created.sort();
		listed_created.sort();
		assert_eq!(listed_created, created, "{name}");
		assert_eq!(
			created,
			["__MACOSX", "__MACOSX/empty", "__MACOSX/mine"],
			"{name}"
		);
	}
}

#[test]
fn deep_mac_folders_count_against_the_member_cap() {
	// ten members, each below 200 folders of its own in __MACOSX: 2000 folders to note
	let data = apple_double_data();
	let deep = "/d".repeat(200);
	let paths: Vec<String> = (0..10).map(|i| format!("__MACOSX/{i}{deep}/._f")).collect();
	let members: Vec<TarMember> = paths
		.iter()
		.map(|path| TarMember::Data(path, &data))
		.collect();
	let tar = tar_with(&members);
	let (_, end, _) = run_job(&tar, job_of(&tar, "m.tar", true, Task::Extract(None)));
	assert_eq!(end.unwrap_err().kind(), ErrorKind::ArchiveTooLarge);
	// kept whole, nothing is noted
	let (_, end, _) = run_job(&tar, job_of(&tar, "m.tar", false, Task::Extract(None)));
	end.unwrap();
}

#[test]
fn a_mac_folder_is_sent_once_everything_in_it_was_judged() {
	let data = apple_double_data();
	let entries = [
		("__MACOSX/", None),
		("__MACOSX/._a.txt", Some(&data[..])),
		("__MACOSX/empty/", None),
	];
	for (name, archive) in [
		("m.zip", zip_of(&entries, None)),
		(
			"m.7z",
			sevenz_of(&entries, SevenZMethod::Lzma2 { level: 1 }, true, None),
		),
	] {
		let (seen, end, _) = run_job(&archive, job_of(&archive, name, true, Task::Extract(None)));
		end.unwrap();
		// an empty folder may be the user's: created, and the folder above it with it
		assert_eq!(
			outline(&seen),
			[
				"skip __MACOSX/._a.txt MacMetadata",
				"dir __MACOSX",
				"dir __MACOSX/empty"
			],
			"{name}"
		);
	}
}
