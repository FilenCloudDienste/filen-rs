//! Zips made by real tools, from `tests/fixtures/archives/zip` (see its README).

use super::{
	AesStrength::{Aes128, Aes256},
	*,
};

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

pub(super) use fixture;

/// `generate.sh`'s `noise`: bytes from a linear congruential generator.
fn noise(mut seed: u32, len: usize) -> Vec<u8> {
	(0..len)
		.map(|_| {
			seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0x7FFF_FFFF;
			(seed >> 16).to_le_bytes()[0]
		})
		.collect()
}

/// The files of `generate.sh`'s `src/` tree, which most fixtures hold.
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

/// The files of a zip of `generate.sh`'s `src/` tree, which holds no directory but `sub/`.
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
		fixture!("lzma.zip"),
		fixture!("lzma-no-eos.zip"),
		fixture!("xz.zip"),
		fixture!("zstd-python.zip"),
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
		let index = read_index(&mut source, zip.len() as u64, READ_BACK_ZIP).unwrap();
		for entry in index.entries.iter().filter(|e| e.kind == ZipKind::File) {
			assert_eq!(
				Protection::of(entry),
				Some(protection),
				"{name} {}",
				entry.name
			);
		}
		let read = read_all(zip, Some("pw")).unwrap_or_else(|e| panic!("{name}: {e}"));
		assert_eq!(files_of_tree(read), fixture_tree(), "{name}");
		assert!(
			matches!(read_all(zip, None), Err(ZipError::PasswordRequired)),
			"{name}"
		);
		assert!(
			matches!(read_all(zip, Some("nope")), Err(ZipError::WrongPassword)),
			"{name}"
		);
	}
}

/// `frames` as the one entry, `z.bin` stating `size` bytes, of an AES-256 zip whose method is
/// zstd (93), which no tool at hand writes encrypted: the SDK's writer stores the frames, then
/// the method in the AES extra field and the size in the central record are set to a zstd
/// entry's. AE-2 keeps no CRC-32, so the authentication code over the stored bytes still holds.
fn aes_zstd_zip(frames: &[u8], size: u32) -> Vec<u8> {
	let mut zip = ours(
		&[("z.bin", Some(frames))],
		ZipMethod::Stored,
		Some(("pw", AesStrength::Aes256)),
	);
	// the extra field's header and length, its version and vendor; the method follows the
	// strength byte
	let extra = |at: usize, zip: &[u8]| {
		zip[at..].starts_with(&[0x01, 0x99, 0x07, 0x00]) && &zip[at + 6..at + 8] == b"AE"
	};
	let fields: Vec<usize> = (0..zip.len() - 11).filter(|&at| extra(at, &zip)).collect();
	assert_eq!(fields.len(), 2, "a local and a central extra field");
	for at in fields {
		zip[at + 9..at + 11].copy_from_slice(&93u16.to_le_bytes());
	}
	let central = (0..zip.len() - 4)
		.find(|&at| u32_at(&zip, at) == CENTRAL_HEADER_SIG)
		.unwrap();
	zip[central + 24..central + 28].copy_from_slice(&size.to_le_bytes());
	zip
}

#[test]
fn an_encrypted_zstd_entry_reads_back_and_data_after_its_frames_is_damage() {
	let data = pattern(200_000, 5);
	let frames =
		ruzstd::encoding::compress_to_vec(&data[..], ruzstd::encoding::CompressionLevel::Fastest);
	let zip = aes_zstd_zip(&frames, u32::try_from(data.len()).unwrap());
	let read = read_all(&zip, Some("pw")).unwrap();
	assert_eq!(read, [("z.bin".to_owned(), ZipKind::File, data.clone())]);

	// bytes after the last frame are none of the entry's data: the entry is damaged
	let zip = aes_zstd_zip(
		&[&frames[..], b"junk"].concat(),
		u32::try_from(data.len()).unwrap(),
	);
	assert!(matches!(
		read_all(&zip, Some("pw")),
		Err(ZipError::Corrupt(
			"an entry holds data after its compressed stream"
		))
	));
	// zero bytes there are padding, as behind a zstd file
	let zip = aes_zstd_zip(
		&[&frames[..], &[0; 8]].concat(),
		u32::try_from(data.len()).unwrap(),
	);
	assert_eq!(read_all(&zip, Some("pw")).unwrap()[0].2, data);
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
			// decomposed, and passed on as it is
			file("finder/Cafe\u{301} NFD.txt", b"decomposed\n"),
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

const HELLO: &[u8] = b"hello from a real zip tool\n";

#[test]
fn a_comment_with_bytes_after_it_is_read_past() {
	// as Info-ZIP's `zip -z` writes a comment, with bytes appended after the zip
	let trailing = b"trailing bytes after the comment\n";
	let comment = b"a zip comment";
	let mut zip = ours(&[("hello.txt", Some(HELLO))], ZipMethod::Stored, None);
	let at = zip.len() - 2;
	zip[at..].copy_from_slice(&u16::try_from(comment.len()).unwrap().to_le_bytes());
	zip.extend_from_slice(comment);
	zip.extend_from_slice(trailing);
	let mut source = Cursor::new(&zip);
	let index = read_index(&mut source, zip.len() as u64, READ_BACK_ZIP).unwrap();
	assert_eq!(index.trailing_bytes, trailing.len() as u64);
	assert_eq!(
		read_all(&zip, None).unwrap(),
		[("hello.txt".to_owned(), ZipKind::File, HELLO.to_vec())]
	);
}

/// A zip of `hello.txt` and `link`, a symlink to it, as the `zip` crate writes them, with each
/// central record's version made by and external attributes set to `made_by` and to the file's
/// or the link's `attributes`, as a tool writes them.
fn symlink_zip(made_by: u16, attributes: [u32; 2]) -> Vec<u8> {
	let mut writer = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
	let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
	writer.start_file("hello.txt", options).unwrap();
	writer.write_all(HELLO).unwrap();
	writer.add_symlink("link", "hello.txt", options).unwrap();
	let mut zip = writer.finish().unwrap().into_inner();
	let records: Vec<usize> = (0..zip.len() - 4)
		.filter(|&at| u32_at(&zip, at) == CENTRAL_HEADER_SIG)
		.collect();
	assert_eq!(records.len(), 2);
	for (at, attributes) in records.into_iter().zip(attributes) {
		zip[at + 4..at + 6].copy_from_slice(&made_by.to_le_bytes());
		zip[at + 38..at + 42].copy_from_slice(&attributes.to_le_bytes());
	}
	zip
}

#[test]
fn symlinks_are_recognised_as_real_tools_store_them() {
	// a Unix mode in the high half; 7-Zip also sets its Unix-extension flag and the archive bit
	// in the low half
	let unix = [0o100_644 << 16, 0o120_755 << 16];
	let seven_zip = unix.map(|mode| mode | 0x8020);
	let made_by = |host: u16, version: u16| (host << 8) | version;
	for (name, zip, link) in [
		(
			"Info-ZIP",
			symlink_zip(made_by(HOST_UNIX, 30), unix),
			ZipKind::Symlink,
		),
		(
			"7-Zip",
			symlink_zip(made_by(HOST_UNIX, 63), seven_zip),
			ZipKind::Symlink,
		),
		(
			"OS X",
			symlink_zip(made_by(HOST_OS_X, 30), unix),
			ZipKind::Symlink,
		),
		// a DOS host's high attribute bits are no Unix mode
		(
			"DOS",
			symlink_zip(made_by(HOST_DOS, 30), unix),
			ZipKind::File,
		),
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
			],
			"{name}"
		);
	}
}
