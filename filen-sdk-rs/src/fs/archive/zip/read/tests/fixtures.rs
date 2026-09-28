//! Zips made by real tools, from `tests/fixtures/archives/zip` (see its README).

use super::*;

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

/// The README's `noise`: bytes from a linear congruential generator.
fn noise(mut seed: u32, len: usize) -> Vec<u8> {
	(0..len)
		.map(|_| {
			seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0x7FFF_FFFF;
			(seed >> 16).to_le_bytes()[0]
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

/// `frames` as the one entry, `z.bin` stating `size` bytes, of an AES-256 zip whose method is
/// zstd (93), which no tool at hand writes encrypted: the SDK's writer stores the frames, then
/// the method in the AES extra field and the size in the central record are set to a zstd
/// entry's. AE-2 keeps no CRC-32, so the authentication code over the stored bytes still holds.
fn aes_zstd_zip(frames: &[u8], size: u32) -> Vec<u8> {
	let mut zip = ours(
		&[("z.bin", Some(frames))],
		ZipMethod::Stored,
		Some((b"pw", AesStrength::Aes256)),
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
	let read = read_all(&zip, Some(b"pw")).unwrap();
	assert_eq!(read, [("z.bin".to_owned(), ZipKind::File, data.clone())]);

	// bytes after the last frame are none of the entry's data: the entry is damaged
	let zip = aes_zstd_zip(
		&[&frames[..], b"junk"].concat(),
		u32::try_from(data.len()).unwrap(),
	);
	assert!(matches!(
		read_all(&zip, Some(b"pw")),
		Err(ZipError::Corrupt(
			"an entry holds data after its compressed stream"
		))
	));
	// zero bytes there are padding, as behind a zstd file
	let zip = aes_zstd_zip(
		&[&frames[..], &[0; 8]].concat(),
		u32::try_from(data.len()).unwrap(),
	);
	assert_eq!(read_all(&zip, Some(b"pw")).unwrap()[0].2, data);
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
			zip[at + 5] = u8::try_from(host).unwrap();
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
