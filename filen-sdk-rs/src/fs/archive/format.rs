//! Which archive format a file holds, told from its first bytes, and the file-name extensions
//! each format goes by.
//!
//! The magic bytes win over the name: a `.zip` that is really a tar is read as a tar. Only
//! brotli and LZMA-alone streams carry no magic, so for those the extension decides, and zip is
//! also tried by extension, since a self-extracting stub or other leading bytes hide its magic.
//! An empty tar is nothing but its end-of-archive marker, so its extension decides too.
//!
//! A tar header's checksum is checked right after the zip and 7z magics, before the stream
//! codecs': a plain tar starts with its first member's path, which can spell `LZIP` or `BZh9`,
//! while a compressed stream passing a header checksum by chance is all but impossible.

use filen_macros::js_type;

use crate::fs::name::{ValidatedName, keep_both::SourceName};

use super::{decode::SKIPPABLE_FRAME_MAGIC, tar_iter::TAR_BLOCK_LEN};

/// A single-stream compression codec: a standalone compressed file, or the outer layer of a
/// compressed tar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[js_type(import, export, no_default)]
pub enum StreamCodec {
	/// gzip (`.gz`).
	Gzip,
	/// bzip2 (`.bz2`).
	Bzip2,
	/// xz (`.xz`).
	Xz,
	/// The legacy `.lzma` ("LZMA alone") container.
	Lzma,
	/// lzip (`.lz`).
	Lzip,
	/// The LZ4 frame format (`.lz4`).
	Lz4,
	/// Brotli (`.br`).
	Brotli,
	/// Zstandard (`.zst`).
	Zstd,
}

/// What an archive is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, tagged, camel_case_fields, no_default)]
pub enum ArchiveFormat {
	/// A tar, bare or inside a compressed stream.
	Tar {
		/// The stream around the tar; `None` for a bare tar.
		codec: Option<StreamCodec>,
	},
	/// A zip, read from its central directory; every entry's data is checked against its
	/// CRC-32 or authentication code.
	Zip,
	/// A 7z, read from its header; entries are checked against the CRC-32s it lists.
	SevenZ,
	/// One compressed file.
	Single {
		/// The stream's codec.
		codec: StreamCodec,
	},
}

impl ArchiveFormat {
	/// What a file named `name` holds as far as its extension tells (`.tar.gz` and `.tgz` a
	/// gzip tar, `.gz` one gzip-compressed file), matched case-insensitively; `None` for a name
	/// with no archive extension. The file's own bytes decide once it is read: a `.zip` that is
	/// really a tar is read as a tar, and a tar named without an extension is read too.
	pub fn of_name(name: &str) -> Option<Self> {
		match_extension(name).map(|(_, format)| format)
	}
}

/// What a file turned out to hold, as far as its first bytes tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detected {
	Zip,
	SevenZ,
	Tar,
	/// A compressed stream: a compressed tar when the first block it decodes to is a tar
	/// header (see [`is_tar_header`]), otherwise a single compressed file.
	Stream(StreamCodec),
}

/// Bytes of the head of a file that [`detect`] looks at: enough for a tar header's checksum.
pub(crate) const DETECT_HEAD_LEN: usize = TAR_BLOCK_LEN;

const SEVEN_Z_MAGIC: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
pub(crate) const XZ_MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0x00];
const LZ4_MAGIC: [u8; 4] = [0x04, 0x22, 0x4D, 0x18];
pub(crate) const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Tells the format of a file from its first bytes (up to [`DETECT_HEAD_LEN`]) and its name.
pub(crate) fn detect(head: &[u8], name: &str) -> Option<Detected> {
	if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
		return Some(Detected::Zip);
	}
	if head.starts_with(&SEVEN_Z_MAGIC) {
		return Some(Detected::SevenZ);
	}
	if head.len() >= DETECT_HEAD_LEN && is_tar_header(&head[..DETECT_HEAD_LEN]) {
		return Some(Detected::Tar);
	}
	if head.starts_with(&[0x1F, 0x8B]) {
		return Some(Detected::Stream(StreamCodec::Gzip));
	}
	if head.starts_with(b"BZh") && head.get(3).is_some_and(|b| (b'1'..=b'9').contains(b)) {
		return Some(Detected::Stream(StreamCodec::Bzip2));
	}
	if head.starts_with(&XZ_MAGIC) {
		return Some(Detected::Stream(StreamCodec::Xz));
	}
	match after_skippable_frames(head) {
		Some(rest) if rest.starts_with(&LZ4_MAGIC) => {
			return Some(Detected::Stream(StreamCodec::Lz4));
		}
		Some(rest) if rest.starts_with(&ZSTD_MAGIC) => {
			return Some(Detected::Stream(StreamCodec::Zstd));
		}
		// lz4 and zstd share their skippable frames: past the head, only the name tells them
		// apart
		None => {
			return match ArchiveFormat::of_name(name)? {
				ArchiveFormat::Single {
					codec: codec @ (StreamCodec::Lz4 | StreamCodec::Zstd),
				}
				| ArchiveFormat::Tar {
					codec: Some(codec @ (StreamCodec::Lz4 | StreamCodec::Zstd)),
				} => Some(Detected::Stream(codec)),
				_ => None,
			};
		}
		Some(_) => {}
	}
	if head.starts_with(b"LZIP") {
		return Some(Detected::Stream(StreamCodec::Lzip));
	}
	// no magic: the name is all there is to go by
	let (extension, format) = match_extension(name)?;
	match format {
		ArchiveFormat::Zip => Some(Detected::Zip),
		ArchiveFormat::Tar { codec: None } if is_end_marker(head) => Some(Detected::Tar),
		// a real tar.lzip starts with lzip's magic, so a `.tlz` without it is the older
		// tar.lzma that went by the same name
		ArchiveFormat::Tar {
			codec: Some(StreamCodec::Lzip),
		} if extension == ".tlz" => Some(Detected::Stream(StreamCodec::Lzma)),
		ArchiveFormat::Single {
			codec: codec @ (StreamCodec::Brotli | StreamCodec::Lzma),
		}
		| ArchiveFormat::Tar {
			codec: Some(codec @ (StreamCodec::Brotli | StreamCodec::Lzma)),
		} => Some(Detected::Stream(codec)),
		_ => None,
	}
}

/// `head` past the skippable frames lz4 and zstd streams may start with, or `None` when it ends
/// inside one.
fn after_skippable_frames(mut head: &[u8]) -> Option<&[u8]> {
	loop {
		let Some((magic, rest)) = head.split_first_chunk::<4>() else {
			return Some(head);
		};
		if !SKIPPABLE_FRAME_MAGIC.contains(&u32::from_le_bytes(*magic)) {
			return Some(head);
		}
		let (size, rest) = rest.split_first_chunk::<4>()?;
		head = rest.get(usize::try_from(u32::from_le_bytes(*size)).ok()?..)?;
	}
}

/// Whether `block` (one 512-byte block) is a tar header: its checksum field matches the sum of
/// its bytes with that field counted as spaces. Old tars summed signed bytes, so both sums are
/// accepted, as GNU tar does. An all-zero block (the end-of-archive marker) is not a header.
pub(crate) fn is_tar_header(block: &[u8]) -> bool {
	let Ok(block) = <&[u8; TAR_BLOCK_LEN]>::try_from(block) else {
		return false;
	};
	let Some(stored) = parse_octal(&block[148..156]) else {
		return false;
	};
	let (unsigned, signed) =
		block
			.iter()
			.enumerate()
			.fold((0u64, 0i64), |(unsigned, signed), (i, &byte)| {
				let byte = if (148..156).contains(&i) { b' ' } else { byte };
				(unsigned + u64::from(byte), signed + i64::from(byte as i8))
			});
	// an all-zero block sums to 8 spaces
	if unsigned == 8 * u64::from(b' ') {
		return false;
	}
	stored == unsigned || i64::try_from(stored).is_ok_and(|stored| stored == signed)
}

/// Whether `block` is a whole block of zeros: a tar's end-of-archive marker, and all an empty
/// tar holds.
pub(crate) fn is_end_marker(block: &[u8]) -> bool {
	block.len() == DETECT_HEAD_LEN && block.iter().all(|&b| b == 0)
}

/// A tar octal number field: leading spaces, octal digits, then NUL or space padding.
fn parse_octal(field: &[u8]) -> Option<u64> {
	let digits = field
		.iter()
		.skip_while(|&&b| b == b' ')
		.take_while(|&&b| b != 0 && b != b' ');
	let mut value: u64 = 0;
	let mut any = false;
	for &b in digits {
		if !(b'0'..=b'7').contains(&b) {
			return None;
		}
		value = value.checked_mul(8)?.checked_add(u64::from(b - b'0'))?;
		any = true;
	}
	any.then_some(value)
}

/// A tar compressed with `codec`, as an extension names it.
const fn tar(codec: StreamCodec) -> ArchiveFormat {
	ArchiveFormat::Tar { codec: Some(codec) }
}

/// One file compressed with `codec`, as an extension names it.
const fn single(codec: StreamCodec) -> ArchiveFormat {
	ArchiveFormat::Single { codec }
}

/// Recognised extensions, longest first so `.tar.gz` wins over `.gz`. Matched case-insensitively.
/// Each format's own extension comes before its aliases (`.tar.gz` before `.tgz`): the first row
/// of a format is the extension the SDK names it with.
const EXTENSIONS: &[(&str, ArchiveFormat)] = &[
	(".tar.gz", tar(StreamCodec::Gzip)),
	(".tar.bz2", tar(StreamCodec::Bzip2)),
	(".tar.xz", tar(StreamCodec::Xz)),
	(".tar.lzma", tar(StreamCodec::Lzma)),
	(".tar.lz4", tar(StreamCodec::Lz4)),
	(".tar.lz", tar(StreamCodec::Lzip)),
	(".tar.br", tar(StreamCodec::Brotli)),
	(".tar.zst", tar(StreamCodec::Zstd)),
	(".tgz", tar(StreamCodec::Gzip)),
	(".tbz2", tar(StreamCodec::Bzip2)),
	(".tbz", tar(StreamCodec::Bzip2)),
	(".txz", tar(StreamCodec::Xz)),
	(".tlz", tar(StreamCodec::Lzip)),
	(".tzst", tar(StreamCodec::Zstd)),
	(".zip", ArchiveFormat::Zip),
	(".7z", ArchiveFormat::SevenZ),
	(".tar", ArchiveFormat::Tar { codec: None }),
	(".gz", single(StreamCodec::Gzip)),
	(".bz2", single(StreamCodec::Bzip2)),
	(".xz", single(StreamCodec::Xz)),
	(".lzma", single(StreamCodec::Lzma)),
	(".lz4", single(StreamCodec::Lz4)),
	(".lz", single(StreamCodec::Lzip)),
	(".br", single(StreamCodec::Brotli)),
	(".zst", single(StreamCodec::Zstd)),
];

impl ArchiveFormat {
	/// The extension the SDK names a file of this format with, dot included.
	pub(crate) fn extension(self) -> &'static str {
		EXTENSIONS
			.iter()
			.find(|(_, format)| *format == self)
			.map(|(extension, _)| *extension)
			.expect("every format has an extension")
	}
}

/// The recognised extension `name` ends in, with what it says the file holds.
pub(crate) fn match_extension(name: &str) -> Option<(&'static str, ArchiveFormat)> {
	EXTENSIONS.iter().copied().find(|(extension, _)| {
		name.len()
			.checked_sub(extension.len())
			.filter(|&at| at > 0)
			.and_then(|at| name.get(at..))
			.is_some_and(|tail| tail.eq_ignore_ascii_case(extension))
	})
}

/// An archive's name without its archive and codec extensions (`photos.tar.gz` → `photos`), or
/// the whole name when it has none of them. Never empty when `name` is not, but not always a name
/// an item may have (`..zip` → `.`).
pub(crate) fn archive_stem(name: &str) -> &str {
	match match_extension(name) {
		Some((extension, _)) => name.get(..name.len() - extension.len()).expect(
			"match_extension found the extension at a char boundary (should be impossible)",
		),
		None => name,
	}
}

/// The folder an archive named `name` is extracted into by default: its name without the archive
/// and codec extensions (`photos.tar.gz` → `photos`), or the whole name when it has none of them.
/// A stem no item may be called is encoded as a listed name would be (`..zip` → `．`), and one
/// that cannot be even so (too long) is replaced with `Archive`.
pub fn archive_default_name(name: &str) -> ValidatedName {
	SourceName::parse(archive_stem(name))
		.map(SourceName::into_name)
		.unwrap_or_else(|_| ValidatedName::try_from("Archive").expect("a valid name"))
}

/// [`archive_default_name`] for an archive whose name may be unknown: `Archive` then.
pub(crate) fn extract_folder_name(name: Option<&str>) -> ValidatedName {
	archive_default_name(name.unwrap_or_default())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tar_header(name: &str) -> [u8; DETECT_HEAD_LEN] {
		let mut block = [0u8; DETECT_HEAD_LEN];
		block[..name.len()].copy_from_slice(name.as_bytes());
		block[100..108].copy_from_slice(b"0000644\0");
		block[124..136].copy_from_slice(b"00000000005\0");
		block[156] = b'0';
		block[257..263].copy_from_slice(b"ustar\0");
		block[148..156].fill(b' ');
		let sum: u32 = block.iter().map(|&b| u32::from(b)).sum();
		block[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
		block
	}

	#[test]
	fn magic_bytes_identify_formats() {
		assert_eq!(detect(b"PK\x03\x04rest", "a.bin"), Some(Detected::Zip));
		assert_eq!(detect(b"PK\x05\x06", "empty"), Some(Detected::Zip));
		assert_eq!(
			detect(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0, 4], "x"),
			Some(Detected::SevenZ)
		);
		assert_eq!(
			detect(&[0x1F, 0x8B, 8, 0], "x"),
			Some(Detected::Stream(StreamCodec::Gzip))
		);
		assert_eq!(
			detect(b"BZh91AY&SY", "x"),
			Some(Detected::Stream(StreamCodec::Bzip2))
		);
		assert_eq!(
			detect(&[0xFD, b'7', b'z', b'X', b'Z', 0, 0, 4], "x"),
			Some(Detected::Stream(StreamCodec::Xz))
		);
		assert_eq!(
			detect(&[0x04, 0x22, 0x4D, 0x18, 0x64], "x"),
			Some(Detected::Stream(StreamCodec::Lz4))
		);
		assert_eq!(
			detect(b"LZIP\x01\x0c", "x"),
			Some(Detected::Stream(StreamCodec::Lzip))
		);
		assert_eq!(
			detect(&[0x28, 0xB5, 0x2F, 0xFD, 0x04], "x"),
			Some(Detected::Stream(StreamCodec::Zstd))
		);
		assert_eq!(detect(&tar_header("a.txt"), "x"), Some(Detected::Tar));
	}

	#[test]
	fn magic_wins_over_the_name() {
		assert_eq!(
			detect(&tar_header("a.txt"), "photos.zip"),
			Some(Detected::Tar)
		);
		assert_eq!(
			detect(&[0x1F, 0x8B, 8, 0], "notes.7z"),
			Some(Detected::Stream(StreamCodec::Gzip))
		);
	}

	#[test]
	fn skippable_frames_are_looked_past() {
		let skippable = |len: u8| {
			[
				&[0x5E, 0x2A, 0x4D, 0x18, len, 0, 0, 0][..],
				&[7; 255][..len as usize],
			]
			.concat()
		};
		let head = [
			skippable(3),
			skippable(0),
			vec![0x28, 0xB5, 0x2F, 0xFD, 0x04],
		]
		.concat();
		assert_eq!(
			detect(&head, "x"),
			Some(Detected::Stream(StreamCodec::Zstd))
		);
		let head = [skippable(9), vec![0x04, 0x22, 0x4D, 0x18, 0x64]].concat();
		assert_eq!(detect(&head, "x"), Some(Detected::Stream(StreamCodec::Lz4)));
		// frames longer than the head leave the name to decide
		let long = [0x50, 0x2A, 0x4D, 0x18, 0, 0, 1, 0];
		assert_eq!(
			detect(&long, "a.tar.zst"),
			Some(Detected::Stream(StreamCodec::Zstd))
		);
		assert_eq!(
			detect(&long, "a.lz4"),
			Some(Detected::Stream(StreamCodec::Lz4))
		);
		assert_eq!(detect(&long, "a.gz"), None);
		// a skippable frame before anything else is no stream
		assert_eq!(detect(&skippable(2), "x"), None);
	}

	#[test]
	fn every_format_is_named_with_its_own_extension() {
		for &(_, format) in EXTENSIONS {
			let extension = format.extension();
			assert_eq!(
				match_extension(&format!("a{extension}")),
				Some((extension, format))
			);
		}
		assert_eq!(tar(StreamCodec::Gzip).extension(), ".tar.gz");
		assert_eq!(tar(StreamCodec::Bzip2).extension(), ".tar.bz2");
		assert_eq!(tar(StreamCodec::Lzip).extension(), ".tar.lz");
		assert_eq!(tar(StreamCodec::Zstd).extension(), ".tar.zst");
		assert_eq!(single(StreamCodec::Lzma).extension(), ".lzma");
	}

	#[test]
	fn a_tar_header_wins_over_magic_its_first_path_happens_to_spell() {
		// a plain tar's first bytes are its first member's path, which may spell a weak magic
		for name in ["LZIP/notes.txt", "BZh9.txt", "BZh1/"] {
			assert_eq!(
				detect(&tar_header(name), "x"),
				Some(Detected::Tar),
				"{name}"
			);
		}
	}

	#[test]
	fn an_empty_tar_is_told_by_its_extension() {
		// the end-of-archive marker is all an empty tar holds
		let empty = [0u8; DETECT_HEAD_LEN];
		assert_eq!(detect(&empty, "e.tar"), Some(Detected::Tar));
		assert_eq!(detect(&empty, "e.bin"), None);
		assert_eq!(detect(&empty[..100], "e.tar"), None);
	}

	#[test]
	fn formats_without_magic_are_told_by_their_extension() {
		assert_eq!(
			detect(b"\x0b\x02\x80data", "a.br"),
			Some(Detected::Stream(StreamCodec::Brotli))
		);
		assert_eq!(
			detect(b"\x0b\x02\x80data", "a.TAR.BR"),
			Some(Detected::Stream(StreamCodec::Brotli))
		);
		assert_eq!(
			detect(&[0x5D, 0, 0, 0x80, 0], "a.lzma"),
			Some(Detected::Stream(StreamCodec::Lzma))
		);
		// `.tlz` is tar.lzip, and also the older name of tar.lzma: without lzip's magic it is
		// the latter
		assert_eq!(
			detect(&[0x5D, 0, 0, 0x80, 0], "a.tlz"),
			Some(Detected::Stream(StreamCodec::Lzma))
		);
		// a zip behind a self-extractor stub has no magic at its start
		assert_eq!(detect(b"MZ\x90\x00stub", "setup.zip"), Some(Detected::Zip));
		// a name promising a format that has magic, without the magic, is not trusted
		assert_eq!(detect(b"plain text", "a.gz"), None);
		assert_eq!(detect(b"plain text", "notes.txt"), None);
	}

	#[test]
	fn tar_headers_are_recognised_by_checksum() {
		let header = tar_header("dir/file.txt");
		assert!(is_tar_header(&header));
		let mut corrupted = header;
		corrupted[0] ^= 1;
		assert!(!is_tar_header(&corrupted));
		// the end-of-archive marker is not a header
		assert!(!is_tar_header(&[0u8; DETECT_HEAD_LEN]));
		// a block of the wrong size is not a header
		assert!(!is_tar_header(&header[..511]));
	}

	#[test]
	fn old_tars_summing_signed_bytes_are_accepted() {
		let mut block = tar_header("\u{e9}.txt");
		block[148..156].fill(b' ');
		let signed: i64 = block.iter().map(|&b| i64::from(b as i8)).sum();
		block[148..155].copy_from_slice(format!("{signed:06o}\0").as_bytes());
		assert!(is_tar_header(&block));
	}

	#[test]
	fn stems_drop_archive_extensions() {
		assert_eq!(archive_stem("photos.tar.gz"), "photos");
		assert_eq!(archive_stem("photos.TGZ"), "photos");
		assert_eq!(archive_stem("backup.2024.zip"), "backup.2024");
		assert_eq!(archive_stem("notes.txt.gz"), "notes.txt");
		assert_eq!(archive_stem("data.tar.lz4"), "data");
		assert_eq!(archive_stem("data.tar.lz"), "data");
		assert_eq!(archive_stem("data.tar.zst"), "data");
		assert_eq!(archive_stem("data.TZST"), "data");
		assert_eq!(archive_stem("notes.txt.zst"), "notes.txt");
		assert_eq!(archive_stem("report.pdf"), "report.pdf");
		// a name that is only an extension keeps it
		assert_eq!(archive_stem(".zip"), ".zip");
		assert_eq!(archive_stem("ünïcödé.7z"), "ünïcödé");
	}

	#[test]
	fn an_archives_default_name_is_always_a_valid_one() {
		let name = |archive: &str| String::from(archive_default_name(archive));
		assert_eq!(name("photos.tar.gz"), "photos");
		assert_eq!(name("report.pdf"), "report.pdf");
		// a stem no item may be called is encoded, as a listed name would be
		assert_eq!(name("..zip"), "\u{FF0E}");
		assert_eq!(name("CON.7z"), "\u{FF23}ON");
		// one that cannot be even so is replaced
		assert_eq!(name(&format!("{}.zip", "a".repeat(300))), "Archive");
		assert_eq!(String::from(extract_folder_name(None)), "Archive");
	}
}
