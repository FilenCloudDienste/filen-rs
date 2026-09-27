//! 7z reading, against archives our writer, the sevenz-rust2 crate and libarchive produce.

use std::io::{Cursor, Write};

use chrono::TimeZone;
use sevenz_rust2::{
	ArchiveEntry, ArchiveReader, ArchiveWriter, EncoderMethod, Password, SourceReader,
	encoder_options::{AesEncoderOptions, Lzma2Options},
};

use super::*;
use crate::fs::archive::sevenz::write::{SevenZEncryption, SevenZMethod, SevenZWriter};

const LIMITS: SevenZLimits = SevenZLimits {
	max_index_bytes: 32 << 20,
	max_entries: 1_000_000,
	decoder_memory: 256 << 20,
};

fn pattern(len: usize, seed: u8) -> Vec<u8> {
	(0..len).map(|i| ((i * 7) % 251) as u8 ^ seed).collect()
}

fn utf16(password: &str) -> Vec<u8> {
	password.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// An entry as read back: path, kind and data.
type Read7z = (String, SevenZKind, Vec<u8>);

/// Every entry of `archive`, its data read through the folder cursor (CRCs checked).
fn read_all(archive: &[u8], password: Option<&str>) -> Result<Vec<Read7z>, SevenZError> {
	read_with(archive, password, LIMITS)
}

fn read_with(
	archive: &[u8],
	password: Option<&str>,
	limits: SevenZLimits,
) -> Result<Vec<Read7z>, SevenZError> {
	let password = password.map(utf16);
	let mut keys = Keys::new(password.as_deref());
	let mut source = Cursor::new(archive);
	let index = read_index(&mut source, archive.len() as u64, limits, &mut keys)?;
	let mut cursor = FolderCursor::new(source, limits.decoder_memory);
	index
		.entries
		.iter()
		.map(|entry| {
			let mut data = Vec::new();
			if entry.stream.is_some() {
				cursor
					.open(&index, entry, &mut keys)?
					.read_to_end(&mut data)
					.map_err(read_error)?;
			}
			Ok((entry.name.clone(), entry.kind, data))
		})
		.collect()
}

/// A tree: a directory, files in and out of it (one over a megabyte), an empty file and a
/// non-ASCII path.
fn sample() -> Vec<(String, Option<Vec<u8>>)> {
	vec![
		("docs".into(), None),
		("docs/a.txt".into(), Some(b"alpha".to_vec())),
		("empty".into(), Some(Vec::new())),
		("big.bin".into(), Some(pattern(150_000, 3))),
		("docs/ünï 𝄞.txt".into(), Some(b"unicode".repeat(100))),
		(
			"text.txt".into(),
			Some(b"the quick brown fox ".repeat(2000)),
		),
	]
}

fn expected(sample: &[(String, Option<Vec<u8>>)]) -> Vec<Read7z> {
	sample
		.iter()
		.map(|(path, data)| match data {
			None => (path.clone(), SevenZKind::Dir, Vec::new()),
			Some(data) => (path.clone(), SevenZKind::File, data.clone()),
		})
		.collect()
}

/// The entries as our writer lists them: files with data first, then the rest, each in the
/// order given.
fn in_our_order(sample: &[(String, Option<Vec<u8>>)]) -> Vec<Read7z> {
	let mut entries = expected(sample);
	entries.sort_by_key(|(_, kind, data)| *kind == SevenZKind::Dir || data.is_empty());
	entries
}

/// Keys cheap to derive, for tests that read many encrypted archives; the interop tests keep
/// the real cost.
const TEST_CYCLES_POWER: u8 = 6;

fn ours(
	sample: &[(String, Option<Vec<u8>>)],
	method: SevenZMethod,
	solid: bool,
	encryption: Option<(SevenZEncryption, &str)>,
) -> Vec<u8> {
	ours_with(sample, method, solid, encryption, TEST_CYCLES_POWER)
}

fn ours_with(
	sample: &[(String, Option<Vec<u8>>)],
	method: SevenZMethod,
	solid: bool,
	encryption: Option<(SevenZEncryption, &str)>,
	cycles_power: u8,
) -> Vec<u8> {
	let password = encryption.map(|(_, password)| utf16(password));
	let mut writer = SevenZWriter::with_cycles_power(
		Vec::new(),
		method,
		solid,
		encryption.map(|(what, _)| (what, &password.as_ref().unwrap()[..])),
		cycles_power,
	)
	.unwrap();
	let when = Some(Utc.with_ymd_and_hms(2024, 5, 6, 7, 8, 9).unwrap());
	for (path, data) in sample {
		match data {
			None => writer.add_dir(path, when),
			Some(data) => {
				let read = writer
					.add_file(path, when, data.len() as u64, &mut &data[..])
					.unwrap();
				assert_eq!(read, data.len() as u64);
			}
		}
	}
	let (mut archive, start) = writer.finish().unwrap();
	archive[..32].copy_from_slice(&start);
	archive
}

const METHODS: [SevenZMethod; 7] = [
	SevenZMethod::Copy,
	SevenZMethod::Lzma2 { level: 1 },
	SevenZMethod::Lzma2 { level: 6 },
	SevenZMethod::Lzma { level: 3 },
	SevenZMethod::Ppmd { level: 3 },
	SevenZMethod::Bzip2 { level: 1 },
	SevenZMethod::Deflate { level: 6 },
];

#[test]
fn our_7z_reads_back_with_every_method_blocking_and_encryption() {
	let sample = sample();
	for method in METHODS {
		for solid in [false, true] {
			for encryption in [
				None,
				Some((SevenZEncryption::Entries, "pw")),
				Some((SevenZEncryption::EntriesAndHeaders, "pässwörd")),
			] {
				let case = format!("{method:?} solid {solid} {encryption:?}");
				let archive = ours(&sample, method, solid, encryption);
				let read = read_all(&archive, encryption.map(|(_, password)| password))
					.unwrap_or_else(|error| panic!("{case}: {error}"));
				assert_eq!(read, in_our_order(&sample), "{case}");
				let mut source = Cursor::new(&archive[..]);
				let password = encryption.map(|(_, password)| utf16(password));
				let index = read_index(
					&mut source,
					archive.len() as u64,
					LIMITS,
					&mut Keys::new(password.as_deref()),
				)
				.unwrap();
				assert_eq!(index.unaccounted_bytes, 0, "{case}");
				assert_eq!(
					index.headers_encrypted,
					matches!(encryption, Some((SevenZEncryption::EntriesAndHeaders, _))),
					"{case}"
				);
				// a solid archive keeps its files in one folder, else one folder each
				assert_eq!(index.folders.len(), if solid { 1 } else { 4 }, "{case}");
				let when = Utc.with_ymd_and_hms(2024, 5, 6, 7, 8, 9).unwrap();
				assert!(
					index
						.entries
						.iter()
						.all(|entry| entry.modified == Some(when))
				);
			}
		}
	}
}

#[test]
fn sevenz_rust2_reads_ours() {
	let sample = sample();
	for method in METHODS {
		for encryption in [
			None,
			Some((SevenZEncryption::Entries, "pw")),
			Some((SevenZEncryption::EntriesAndHeaders, "pw")),
		] {
			let case = format!("{method:?} {encryption:?}");
			if encryption.is_some()
				&& !matches!(method, SevenZMethod::Copy | SevenZMethod::Lzma2 { .. })
			{
				continue;
			}
			let archive = ours_with(
				&sample,
				method,
				true,
				encryption,
				crate::fs::archive::sevenz::crypto::WRITE_CYCLES_POWER,
			);
			let password = encryption.map_or_else(Password::empty, |(_, pw)| pw.into());
			let mut reader = ArchiveReader::new(Cursor::new(&archive), password)
				.unwrap_or_else(|error| panic!("{case}: {error}"));
			let mut read = Vec::new();
			reader
				.for_each_entries(|entry, data| {
					let mut bytes = Vec::new();
					data.read_to_end(&mut bytes)?;
					let kind = if entry.is_directory() {
						SevenZKind::Dir
					} else {
						SevenZKind::File
					};
					read.push((entry.name().to_owned(), kind, bytes));
					Ok(true)
				})
				.unwrap_or_else(|error| panic!("{case}: {error}"));
			let expected = in_our_order(&sample);
			let summary = |entries: &[Read7z]| {
				entries
					.iter()
					.map(|(name, kind, data)| {
						(name.clone(), *kind, data.len(), crc32fast::hash(data))
					})
					.collect::<Vec<_>>()
			};
			assert_eq!(summary(&read), summary(&expected), "{case}");
			assert!(
				reader
					.archive()
					.files
					.iter()
					.any(|file| file.name() == "docs" && file.is_directory()),
				"{case}"
			);
		}
	}
}

/// Written by sevenz-rust2: every file alone, or solid, with the content methods given.
fn theirs(
	sample: &[(String, Option<Vec<u8>>)],
	methods: Vec<sevenz_rust2::EncoderConfiguration>,
	solid: bool,
	encrypt_header: bool,
) -> Vec<u8> {
	let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
	writer.set_content_methods(methods);
	writer.set_encrypt_header(encrypt_header);
	let (mut entries, mut readers) = (Vec::new(), Vec::new());
	for (path, data) in sample {
		match data {
			None => {
				writer
					.push_archive_entry::<&[u8]>(ArchiveEntry::new_directory(path), None)
					.unwrap();
			}
			// with a reader, sevenz-rust2 gives an empty file a packed stream of its own and marks
			// the packed streams' CRCs wrongly; 7-Zip writes it without a stream
			Some(data) if data.is_empty() => {
				writer
					.push_archive_entry::<&[u8]>(ArchiveEntry::new_file(path), None)
					.unwrap();
			}
			Some(data) if solid => {
				entries.push(ArchiveEntry::new_file(path));
				readers.push(SourceReader::new(&data[..]));
			}
			Some(data) => {
				writer
					.push_archive_entry(ArchiveEntry::new_file(path), Some(&data[..]))
					.unwrap();
			}
		}
	}
	if !entries.is_empty() {
		writer.push_archive_entries(entries, readers).unwrap();
	}
	writer.finish().unwrap().into_inner()
}

#[test]
fn we_read_sevenz_rust2s_7z() {
	let sample = sample();
	let mut in_their_order: Vec<_> = sample.clone();
	for (n, (methods, solid, encrypt_header, password)) in [
		(vec![EncoderMethod::LZMA2.into()], false, false, None),
		(vec![EncoderMethod::LZMA2.into()], true, false, None),
		(vec![EncoderMethod::LZMA.into()], false, false, None),
		(vec![EncoderMethod::PPMD.into()], true, false, None),
		(vec![EncoderMethod::BZIP2.into()], false, false, None),
		(vec![EncoderMethod::DEFLATE.into()], false, false, None),
		(vec![EncoderMethod::COPY.into()], true, false, None),
		(
			vec![
				EncoderMethod::BCJ_X86_FILTER.into(),
				Lzma2Options::from_level(3).into(),
			],
			true,
			false,
			None,
		),
		(
			vec![
				sevenz_rust2::encoder_options::DeltaOptions::from_distance(4).into(),
				EncoderMethod::LZMA2.into(),
			],
			false,
			false,
			None,
		),
		(
			vec![
				AesEncoderOptions::new("secret".into()).into(),
				EncoderMethod::LZMA2.into(),
			],
			true,
			false,
			Some("secret"),
		),
		(
			vec![
				AesEncoderOptions::new("secret".into()).into(),
				EncoderMethod::LZMA2.into(),
			],
			false,
			true,
			Some("secret"),
		),
	]
	.into_iter()
	.enumerate()
	{
		let case = format!("case {n} solid {solid} {encrypt_header}");
		let archive = theirs(&sample, methods, solid, encrypt_header);
		let mut read =
			read_all(&archive, password).unwrap_or_else(|error| panic!("{case}: {error}"));
		// sevenz-rust2 writes a solid block's entries after the ones written alone
		read.sort_by(|a, b| a.0.cmp(&b.0));
		in_their_order.sort_by(|a, b| a.0.cmp(&b.0));
		assert_eq!(read, expected(&in_their_order), "{case}");
	}
}

#[test]
fn passwords_are_asked_for_and_checked() {
	let sample = sample();
	let entries = ours(
		&sample,
		SevenZMethod::Lzma2 { level: 1 },
		true,
		Some((SevenZEncryption::Entries, "right")),
	);
	// the names are readable, the data is not
	let mut source = Cursor::new(&entries[..]);
	let index = read_index(
		&mut source,
		entries.len() as u64,
		LIMITS,
		&mut Keys::new(None),
	)
	.unwrap();
	assert_eq!(index.entries.len(), sample.len());
	assert!(index.folders.iter().all(Folder::encrypted));
	assert!(matches!(
		read_all(&entries, None),
		Err(SevenZError::PasswordRequired)
	));
	// a wrong key decrypts to noise that fails to decode or to match its CRC
	assert!(read_all(&entries, Some("wrong")).is_err());
	assert_eq!(
		read_all(&entries, Some("right")).unwrap(),
		in_our_order(&sample)
	);

	let headers = ours(
		&sample,
		SevenZMethod::Copy,
		false,
		Some((SevenZEncryption::EntriesAndHeaders, "right")),
	);
	assert!(matches!(
		read_all(&headers, None),
		Err(SevenZError::PasswordRequired)
	));
	assert!(matches!(
		read_all(&headers, Some("wrong")),
		Err(SevenZError::WrongPassword)
	));
	assert_eq!(
		read_all(&headers, Some("right")).unwrap(),
		in_our_order(&sample)
	);
}

#[test]
fn limits_are_checked_before_allocating() {
	let archive = ours(&sample(), SevenZMethod::Lzma2 { level: 6 }, true, None);
	let too_few_entries = SevenZLimits {
		max_entries: 3,
		..LIMITS
	};
	assert!(matches!(
		read_with(&archive, None, too_few_entries),
		Err(SevenZError::TooLarge(_))
	));
	let tiny_index = SevenZLimits {
		max_index_bytes: 16,
		..LIMITS
	};
	assert!(matches!(
		read_with(&archive, None, tiny_index),
		Err(SevenZError::TooLarge(_))
	));
	// the level-6 dictionary is clamped to the 190 kB of data, which still needs more than this
	let tiny_codec = SevenZLimits {
		decoder_memory: 128 << 10,
		..LIMITS
	};
	assert!(matches!(
		read_with(&archive, None, tiny_codec),
		Err(SevenZError::TooLarge(_))
	));
	// and a folder of a few bytes decodes in little memory whatever dictionary it states
	let tiny_codec = SevenZLimits {
		decoder_memory: 256 << 10,
		..LIMITS
	};
	let small = ours(
		&[("a".into(), Some(b"abc".to_vec()))],
		SevenZMethod::Lzma2 { level: 9 },
		true,
		None,
	);
	assert_eq!(read_with(&small, None, tiny_codec).unwrap()[0].2, b"abc");
}

#[test]
fn damage_is_detected() {
	let archive = ours(&sample(), SevenZMethod::Lzma2 { level: 1 }, true, None);
	// the start header, the header, and the data are each checked
	for at in [10, 20, archive.len() - 20, 40, archive.len() / 2] {
		let mut damaged = archive.clone();
		damaged[at] ^= 0x40;
		assert!(read_all(&damaged, None).is_err(), "a flipped byte at {at}");
	}
	for len in [0, 20, 32, archive.len() / 2, archive.len() - 1] {
		assert!(read_all(&archive[..len], None).is_err(), "cut at {len}");
	}
	assert!(matches!(
		read_all(b"PK\x03\x04 not a 7z at all, but long enough", None),
		Err(SevenZError::Corrupt(_))
	));
}

#[test]
fn a_byte_flip_never_panics() {
	let archives = [
		ours(&sample(), SevenZMethod::Lzma2 { level: 1 }, true, None),
		ours(
			&sample(),
			SevenZMethod::Copy,
			false,
			Some((SevenZEncryption::EntriesAndHeaders, "pw")),
		),
		ours(&sample()[..3], SevenZMethod::Ppmd { level: 1 }, false, None),
	];
	for archive in archives {
		// the header is where parsing happens: flip every byte of its last part
		for at in (archive.len().saturating_sub(300)..archive.len()).chain((0..32).step_by(3)) {
			for bit in [0x01, 0x80] {
				let mut damaged = archive.clone();
				damaged[at] ^= bit;
				let _ = std::panic::catch_unwind(|| read_all(&damaged, Some("pw")))
					.unwrap_or_else(|_| panic!("a flip of {bit:#x} at {at} panicked"));
			}
		}
	}
}

#[test]
fn trailing_data_is_unaccounted() {
	let mut archive = ours(&sample(), SevenZMethod::Copy, false, None);
	archive.extend_from_slice(b"appended");
	let mut source = Cursor::new(&archive[..]);
	let index = read_index(
		&mut source,
		archive.len() as u64,
		LIMITS,
		&mut Keys::new(None),
	)
	.unwrap();
	assert_eq!(index.unaccounted_bytes, 8);
	assert_eq!(read_all(&archive, None).unwrap(), in_our_order(&sample()));
}

#[test]
fn an_empty_archive_reads_as_empty() {
	let archive = ours(&[], SevenZMethod::Lzma2 { level: 1 }, true, None);
	assert!(read_all(&archive, None).unwrap().is_empty());
	let mut reader = ArchiveReader::new(Cursor::new(&archive), Password::empty()).unwrap();
	reader
		.for_each_entries(|_, _| panic!("no entries"))
		.unwrap();
}

#[test]
fn entries_read_out_of_order_reopen_their_folder() {
	let sample = sample();
	let archive = ours(&sample, SevenZMethod::Lzma2 { level: 1 }, true, None);
	let mut source = Cursor::new(&archive[..]);
	let mut keys = Keys::new(None);
	let index = read_index(&mut source, archive.len() as u64, LIMITS, &mut keys).unwrap();
	let mut cursor = FolderCursor::new(source, LIMITS.decoder_memory);
	let expected = in_our_order(&sample);
	// the last file first, then the first, then skipping one
	for at in [3, 0, 2, 1] {
		let entry = &index.entries[at];
		let mut data = Vec::new();
		cursor
			.open(&index, entry, &mut keys)
			.unwrap()
			.read_to_end(&mut data)
			.unwrap();
		assert_eq!(data, expected[at].2, "{}", entry.name);
	}
}

/// libarchive's `bsdtar`, when installed, reads ours and writes 7z we read.
#[test]
fn bsdtar_interop() {
	let Ok(bsdtar) = which_bsdtar() else {
		eprintln!("bsdtar is not installed: skipping");
		return;
	};
	let dir = tempfile::tempdir().unwrap();
	let sample = sample();
	for method in [
		SevenZMethod::Copy,
		SevenZMethod::Lzma2 { level: 5 },
		SevenZMethod::Lzma { level: 5 },
		SevenZMethod::Bzip2 { level: 9 },
		SevenZMethod::Deflate { level: 9 },
		SevenZMethod::Ppmd { level: 6 },
	] {
		let path = dir.path().join("ours.7z");
		std::fs::write(&path, ours(&sample, method, true, None)).unwrap();
		let out = std::process::Command::new(&bsdtar)
			.arg("-xOf")
			.arg(&path)
			.arg("text.txt")
			.output()
			.unwrap();
		assert!(
			out.status.success(),
			"{method:?}: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		assert_eq!(out.stdout, sample[5].1.clone().unwrap(), "{method:?}");
		let out = std::process::Command::new(&bsdtar)
			.arg("-tf")
			.arg(&path)
			.output()
			.unwrap();
		// names are listed as the platform spells them (NFD on macOS)
		let listed = String::from_utf8_lossy(&out.stdout).into_owned();
		assert!(
			listed.lines().any(|line| line == "docs/a.txt"),
			"{method:?}: {listed}"
		);
	}
	let tree = dir.path().join("tree");
	std::fs::create_dir_all(tree.join("docs")).unwrap();
	std::fs::write(tree.join("docs/a.txt"), b"alpha").unwrap();
	std::fs::write(tree.join("big.bin"), pattern(300_000, 1)).unwrap();
	for compression in ["store", "deflate", "bzip2", "lzma1", "lzma2", "ppmd"] {
		let path = dir.path().join(format!("{compression}.7z"));
		let status = std::process::Command::new(&bsdtar)
			.args(["--format", "7zip", "--options"])
			.arg(format!("7zip:compression={compression}"))
			.arg("-cf")
			.arg(&path)
			.arg("-C")
			.arg(&tree)
			.args(["docs", "big.bin"])
			.status()
			.unwrap();
		if !status.success() {
			// libarchive builds differ in the 7z coders they write
			eprintln!("bsdtar cannot write 7z with {compression}: skipping it");
			continue;
		}
		let mut read = read_all(&std::fs::read(&path).unwrap(), None)
			.unwrap_or_else(|error| panic!("{compression}: {error}"));
		read.sort_by(|a, b| a.0.cmp(&b.0));
		assert_eq!(
			read,
			vec![
				("big.bin".into(), SevenZKind::File, pattern(300_000, 1)),
				("docs".into(), SevenZKind::Dir, Vec::new()),
				("docs/a.txt".into(), SevenZKind::File, b"alpha".to_vec()),
			],
			"{compression}"
		);
	}
	let _ = std::io::stdout().flush();
}

fn which_bsdtar() -> Result<std::path::PathBuf, ()> {
	[
		"/usr/bin/bsdtar",
		"/usr/local/bin/bsdtar",
		"/opt/homebrew/bin/bsdtar",
	]
	.into_iter()
	.map(std::path::PathBuf::from)
	.find(|path| path.exists())
	.ok_or(())
}

#[test]
fn a_wrong_password_under_lzma_reads_as_one() {
	// LZMA reads its first bytes as it starts: under a wrong key, that is where it fails
	let sample = sample();
	let archive = theirs(
		&sample,
		vec![
			AesEncoderOptions::new("secret".into()).into(),
			EncoderMethod::LZMA.into(),
		],
		true,
		true,
	);
	for attempt in 0..16 {
		let wrong = format!("wrong {attempt}");
		assert!(
			matches!(
				read_all(&archive, Some(&wrong)),
				Err(SevenZError::WrongPassword)
			),
			"{wrong}"
		);
	}
	assert!(read_all(&archive, Some("secret")).is_ok());
}

#[test]
fn aes_data_of_a_partial_block_is_damage_whatever_the_key() {
	let folder = |packed: u64| {
		let folder = Folder {
			coders: vec![Coder {
				method: Some(Method::Aes),
				props: Box::new([]),
				inputs: 1,
			}],
			bind_pairs: Vec::new(),
			packed: vec![0],
			unpack_sizes: vec![packed],
			crc: None,
			first_pack: 0,
			main: 0,
		};
		let source = Rc::new(RefCell::new(Cursor::new(vec![0u8; 64])));
		let mut keys = Keys::new(None);
		open_folder(&source, &folder, &[0], &[packed], 1 << 20, &mut keys).map(|_| ())
	};
	// told before any key is asked for
	assert!(matches!(
		folder(17),
		Err(SevenZError::Corrupt(AES_PARTIAL_BLOCK))
	));
	// whole blocks pass on to the key (which this archive cannot give)
	assert!(!matches!(
		folder(32),
		Err(SevenZError::Corrupt(AES_PARTIAL_BLOCK))
	));
}

#[test]
fn every_coder_has_to_feed_the_folders_output() {
	let coder = |method| Coder {
		method: Some(method),
		props: Box::default(),
		inputs: 1,
	};
	let folder = |bind_pairs, packed| Folder {
		coders: vec![
			coder(Method::Copy),
			coder(Method::Copy),
			coder(Method::Copy),
		],
		bind_pairs,
		packed,
		unpack_sizes: vec![1, 1, 1],
		crc: None,
		first_pack: 0,
		main: 0,
	};
	// a chain: coder 0 reads coder 1, which reads coder 2
	check_acyclic(&folder(vec![(0, 1), (1, 2)], vec![2])).unwrap();
	// coders 1 and 2 feed each other and nothing else: every stream is bound once, but the
	// folder's output never reaches them
	assert!(matches!(
		check_acyclic(&folder(vec![(1, 2), (2, 1)], vec![0])),
		Err(SevenZError::Corrupt(_))
	));
}
