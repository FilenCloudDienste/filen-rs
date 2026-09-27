//! Which archive format a file holds, told from its first bytes, and the file-name extensions
//! each format goes by.
//!
//! The magic bytes win over the name: a `.zip` that is really a tar is read as a tar. Only
//! brotli and LZMA-alone streams carry no magic, so for those the extension decides, and zip is
//! also tried by extension, since a self-extracting stub or other leading bytes hide its magic.

/// A single-stream compression codec: a standalone compressed file, or the outer layer of a
/// compressed tar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamCodec {
	Gzip,
	Bzip2,
	Xz,
	/// The legacy `.lzma` ("LZMA alone") container.
	Lzma,
	Lzip,
	Lz4,
	Brotli,
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
pub(crate) const DETECT_HEAD_LEN: usize = TAR_BLOCK;

const TAR_BLOCK: usize = 512;

const SEVEN_Z_MAGIC: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
const XZ_MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0x00];
const LZ4_MAGIC: [u8; 4] = [0x04, 0x22, 0x4D, 0x18];

/// Tells the format of a file from its first bytes (up to [`DETECT_HEAD_LEN`]) and its name.
pub(crate) fn detect(head: &[u8], name: &str) -> Option<Detected> {
	if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
		return Some(Detected::Zip);
	}
	if head.starts_with(&SEVEN_Z_MAGIC) {
		return Some(Detected::SevenZ);
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
	if head.starts_with(&LZ4_MAGIC) {
		return Some(Detected::Stream(StreamCodec::Lz4));
	}
	if head.starts_with(b"LZIP") {
		return Some(Detected::Stream(StreamCodec::Lzip));
	}
	if head.len() >= TAR_BLOCK && is_tar_header(&head[..TAR_BLOCK]) {
		return Some(Detected::Tar);
	}
	// no magic: the name is all there is to go by
	match extension_format(name)? {
		ExtensionFormat::Zip => Some(Detected::Zip),
		ExtensionFormat::Stream(codec @ (StreamCodec::Brotli | StreamCodec::Lzma)) => {
			Some(Detected::Stream(codec))
		}
		ExtensionFormat::CompressedTar(codec @ (StreamCodec::Brotli | StreamCodec::Lzma)) => {
			Some(Detected::Stream(codec))
		}
		_ => None,
	}
}

/// Whether `block` (one 512-byte block) is a tar header: its checksum field matches the sum of
/// its bytes with that field counted as spaces. Old tars summed signed bytes, so both sums are
/// accepted, as GNU tar does. An all-zero block (the end-of-archive marker) is not a header.
pub(crate) fn is_tar_header(block: &[u8]) -> bool {
	let Ok(block) = <&[u8; TAR_BLOCK]>::try_from(block) else {
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

/// What a file name's extension says it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtensionFormat {
	Zip,
	SevenZ,
	Tar,
	CompressedTar(StreamCodec),
	Stream(StreamCodec),
}

/// Recognised extensions, longest first so `.tar.gz` wins over `.gz`. Matched case-insensitively.
const EXTENSIONS: &[(&str, ExtensionFormat)] = &[
	(".tar.gz", ExtensionFormat::CompressedTar(StreamCodec::Gzip)),
	(
		".tar.bz2",
		ExtensionFormat::CompressedTar(StreamCodec::Bzip2),
	),
	(".tar.xz", ExtensionFormat::CompressedTar(StreamCodec::Xz)),
	(
		".tar.lzma",
		ExtensionFormat::CompressedTar(StreamCodec::Lzma),
	),
	(".tar.lz4", ExtensionFormat::CompressedTar(StreamCodec::Lz4)),
	(".tar.lz", ExtensionFormat::CompressedTar(StreamCodec::Lzip)),
	(
		".tar.br",
		ExtensionFormat::CompressedTar(StreamCodec::Brotli),
	),
	(".tgz", ExtensionFormat::CompressedTar(StreamCodec::Gzip)),
	(".tbz2", ExtensionFormat::CompressedTar(StreamCodec::Bzip2)),
	(".tbz", ExtensionFormat::CompressedTar(StreamCodec::Bzip2)),
	(".txz", ExtensionFormat::CompressedTar(StreamCodec::Xz)),
	(".tlz", ExtensionFormat::CompressedTar(StreamCodec::Lzip)),
	(".zip", ExtensionFormat::Zip),
	(".7z", ExtensionFormat::SevenZ),
	(".tar", ExtensionFormat::Tar),
	(".gz", ExtensionFormat::Stream(StreamCodec::Gzip)),
	(".bz2", ExtensionFormat::Stream(StreamCodec::Bzip2)),
	(".xz", ExtensionFormat::Stream(StreamCodec::Xz)),
	(".lzma", ExtensionFormat::Stream(StreamCodec::Lzma)),
	(".lz4", ExtensionFormat::Stream(StreamCodec::Lz4)),
	(".lz", ExtensionFormat::Stream(StreamCodec::Lzip)),
	(".br", ExtensionFormat::Stream(StreamCodec::Brotli)),
];

/// The recognised extension `name` ends in, with what it says the file holds.
fn match_extension(name: &str) -> Option<(&'static str, ExtensionFormat)> {
	EXTENSIONS.iter().copied().find(|(extension, _)| {
		name.len() > extension.len()
			&& name.is_char_boundary(name.len() - extension.len())
			&& name[name.len() - extension.len()..].eq_ignore_ascii_case(extension)
	})
}

/// What `name`'s extension says the file holds, if it is one this module knows.
pub(crate) fn extension_format(name: &str) -> Option<ExtensionFormat> {
	match_extension(name).map(|(_, format)| format)
}

/// The name an archive's contents are extracted under by default: its name without the archive
/// and codec extensions (`photos.tar.gz` → `photos`), or the whole name when it has none of them.
pub fn archive_default_name(name: &str) -> &str {
	match match_extension(name) {
		Some((extension, _)) => &name[..name.len() - extension.len()],
		None => name,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tar_header(name: &str) -> [u8; TAR_BLOCK] {
		let mut block = [0u8; TAR_BLOCK];
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
		assert!(!is_tar_header(&[0u8; TAR_BLOCK]));
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
	fn default_names_drop_archive_extensions() {
		assert_eq!(archive_default_name("photos.tar.gz"), "photos");
		assert_eq!(archive_default_name("photos.TGZ"), "photos");
		assert_eq!(archive_default_name("backup.2024.zip"), "backup.2024");
		assert_eq!(archive_default_name("notes.txt.gz"), "notes.txt");
		assert_eq!(archive_default_name("data.tar.lz4"), "data");
		assert_eq!(archive_default_name("data.tar.lz"), "data");
		assert_eq!(archive_default_name("report.pdf"), "report.pdf");
		// a name that is only an extension keeps it
		assert_eq!(archive_default_name(".zip"), ".zip");
		assert_eq!(archive_default_name("ünïcödé.7z"), "ünïcödé");
	}
}
