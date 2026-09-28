//! Zip archives, read and written by the SDK itself: the central directory is parsed within the
//! index budget, entries are decrypted and decompressed one at a time, and archives are written
//! front to back with data descriptors, so neither side ever needs the whole archive at once.

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
const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;
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

/// Whether the SDK reads data compressed with `method`, `encrypted` or not. LZMA and XZ have
/// their own memory limits, which the encrypted forms would first have to buffer around: only
/// their plain forms are read.
pub(crate) fn method_supported(method: u16, encrypted: bool) -> bool {
	match method {
		METHOD_STORED | METHOD_DEFLATE | METHOD_DEFLATE64 | METHOD_BZIP2 | METHOD_ZSTD => true,
		METHOD_LZMA | METHOD_XZ => !encrypted,
		_ => false,
	}
}
