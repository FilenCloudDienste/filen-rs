//! Zip archives, read and written by the SDK itself: the central directory is parsed within the
//! index budget, entries are decrypted and decompressed one at a time, and archives are written
//! front to back with data descriptors, so neither side ever needs the whole archive at once.
//!
//! Download-as-zip (`fs::zip`) writes through `async_zip` instead, but that crate (the SDK's
//! fork) can neither write WinZip AES entries nor run synchronously on the codec worker, so
//! archive jobs keep their own writer. The two share the extended timestamp field's encoding and
//! both write DOS times in UTC.

pub(crate) mod cp437;
pub(crate) mod crypto;
pub(crate) mod read;
pub(crate) mod write;

/// Record signatures.
const LOCAL_HEADER_SIG: u32 = 0x0403_4b50;
const CENTRAL_HEADER_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
const EOCD64_SIG: u32 = 0x0606_4b50;
const EOCD64_LOCATOR_SIG: u32 = 0x0706_4b50;

/// General-purpose flags (zip specification 4.4.4).
const FLAG_ENCRYPTED: u16 = 0x0001;
/// For LZMA: the stream ends with an end marker.
const FLAG_LZMA_END_MARKER: u16 = 0x0002;
const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;
/// PKWARE strong encryption, which the SDK does not read.
const FLAG_STRONG_ENCRYPTION: u16 = 0x0040;
const FLAG_UTF8: u16 = 0x0800;

/// The version-made-by hosts (zip specification 4.4.2.2) whose external attributes carry a Unix
/// mode, and whose writers store names in the system's encoding, UTF-8 by now.
const HOST_UNIX: u16 = 3;
const HOST_OS_X: u16 = 19;

/// Compression methods (zip specification 4.4.5), and WinZip AES, which stands in for the
/// method it encrypts.
pub(crate) const METHOD_STORED: u16 = 0;
pub(crate) const METHOD_DEFLATE: u16 = 8;
pub(crate) const METHOD_DEFLATE64: u16 = 9;
pub(crate) const METHOD_BZIP2: u16 = 12;
pub(crate) const METHOD_LZMA: u16 = 14;
pub(crate) const METHOD_ZSTD: u16 = 93;
pub(crate) const METHOD_XZ: u16 = 95;
pub(crate) const METHOD_PPMD: u16 = 98;
const METHOD_AES: u16 = 99;

/// Extra field ids (zip specification 4.5.2); the extended timestamp's is
/// `fs::zip::ZipExtendedTime::HEADER_ID`.
const EXTRA_ZIP64: u16 = 0x0001;
const EXTRA_NTFS: u16 = 0x000A;
const EXTRA_UNICODE_PATH: u16 = 0x7075;
const EXTRA_AES: u16 = 0x9901;

/// What a 32-bit size or offset field holds when the zip64 extra field has the value.
const ZIP64_MARKER: u32 = 0xFFFF_FFFF;

/// A compression method's name, for display; `None` for one without a name here.
pub(crate) fn method_name(method: u16) -> Option<&'static str> {
	Some(match method {
		METHOD_STORED => "Stored",
		METHOD_DEFLATE => "Deflate",
		METHOD_DEFLATE64 => "Deflate64",
		METHOD_BZIP2 => "BZip2",
		METHOD_LZMA => "LZMA",
		METHOD_ZSTD => "Zstd",
		METHOD_XZ => "XZ",
		METHOD_PPMD => "PPMd",
		_ => return None,
	})
}
