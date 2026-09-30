//! Reading a zip: its central directory, parsed here within the index budget, and each entry's
//! data, decrypted, decompressed and checked against its CRC-32 or authentication code.
//!
//! Everything read is untrusted. The end-of-central-directory record is searched for in the last
//! 64 KiB only; the directory's size is checked against the budget before it is read, and its
//! entry count against both the member cap and what its size can hold. Entries are visited in the
//! order of their local headers, whatever order the directory lists them in, so a shuffled
//! directory cannot make the archive be fetched more than once; overlapping entries are refused.
//! A name listed twice keeps its last entry, as zip tools do, and the duplicates are reported.

use std::{
	borrow::Cow,
	cell::RefCell,
	io::{self, BufReader, Read, Seek, SeekFrom},
	mem,
	rc::Rc,
};

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};

use super::{
	CENTRAL_HEADER_SIG, EOCD_SIG, EOCD64_LOCATOR_SIG, EOCD64_SIG, FLAG_DATA_DESCRIPTOR,
	FLAG_ENCRYPTED, FLAG_UTF8, HOST_OS_X, HOST_UNIX, LOCAL_HEADER_SIG, METHOD_AES, METHOD_BZIP2,
	METHOD_DEFLATE, METHOD_DEFLATE64, METHOD_LZMA, METHOD_STORED, METHOD_XZ, METHOD_ZSTD, cp437,
	crypto::{AesReader, AesStrength, CryptoError, ZipCryptoReader},
};
use crate::fs::drive_job::exceeds_limit;
use crate::{
	Error, ErrorKind,
	fs::archive::{
		bytes,
		decode::{StreamDecoder, clamp_lzma_dict, open_stream},
		error::read_failure,
		format::StreamCodec,
		limits::HEAP_PER_INDEX_BYTE,
		password::ArchivePassword,
	},
	util::SeededMap,
};

/// For LZMA: the stream ends with an end marker (a general-purpose flag).
const FLAG_LZMA_END_MARKER: u16 = 0x0002;
const FLAG_STRONG_ENCRYPTION: u16 = 0x0040;

const EOCD_LEN: usize = 22;
const EOCD_LEN_U64: u64 = EOCD_LEN as u64;
const EOCD64_LEN: usize = 56;
const EOCD64_LEN_U64: u64 = EOCD64_LEN as u64;
const EOCD64_LOCATOR_LEN: usize = 20;
const EOCD64_LOCATOR_LEN_U64: u64 = EOCD64_LOCATOR_LEN as u64;
const CENTRAL_HEADER_LEN: usize = 46;
const LOCAL_HEADER_LEN: usize = 30;
const LOCAL_HEADER_LEN_U64: u64 = LOCAL_HEADER_LEN as u64;
/// A comment may be up to this long, so the end record is in the file's last 64 KiB and change.
const MAX_COMMENT_LEN: u64 = 0xFFFF;
/// End-record candidates tried before giving up: a comment can hold the record's signature.
/// Telling whether one can be the zip's reads two signatures it points at (see [`plausible`]):
/// free when they are in the file's tail, read already, and otherwise one fetch each, so at most
/// 32 fetches, all before the index budget is checked. A candidate is only probed when that can
/// change which one is taken.
const MAX_EOCD_CANDIDATES: usize = 16;
/// Names kept one by one when a name is listed twice; beyond that they are counted.
const MAX_DUPLICATE_NAMES: usize = 100;

/// Why reading a zip failed, other than its source.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ZipError {
	#[error("the zip is damaged: {0}")]
	Corrupt(&'static str),
	#[error("the zip uses a feature that isn't supported: {0}")]
	Unsupported(&'static str),
	#[error("the zip's index is larger than allowed: {0}")]
	TooLarge(&'static str),
	#[error("an entry is encrypted, and no password was given")]
	PasswordRequired,
	#[error("the password does not open the entry")]
	WrongPassword,
	#[error("the entry's data runs into the next entry's")]
	Overlapping,
	#[error(transparent)]
	Read(#[from] io::Error),
}

impl From<ZipError> for Error {
	fn from(error: ZipError) -> Self {
		let kind = match error {
			ZipError::Read(error) => return read_failure(error),
			ZipError::Corrupt(_) | ZipError::Overlapping => ErrorKind::ArchiveCorrupt,
			ZipError::Unsupported(_) => ErrorKind::ArchiveUnsupported,
			ZipError::TooLarge(_) => ErrorKind::ArchiveTooLarge,
			ZipError::PasswordRequired => ErrorKind::ArchivePasswordRequired,
			ZipError::WrongPassword => ErrorKind::ArchiveWrongPassword,
		};
		Error::custom_with_source(kind, error, None::<&str>)
	}
}

/// What a reader may spend on a zip's index.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ZipLimits {
	pub(crate) max_index_bytes: u64,
	pub(crate) max_entries: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZipKind {
	File,
	Dir,
	Symlink,
}

/// How an entry's data is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZipEncryption {
	None,
	/// Traditional PKWARE encryption, whose header check byte is `check`.
	ZipCrypto {
		check: u8,
	},
	/// WinZip AES; `authenticated_only` for AE-2, which stores no CRC-32 (the authentication
	/// code covers the data instead).
	Aes {
		strength: AesStrength,
		authenticated_only: bool,
	},
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ZipEntry {
	/// Its position in the central directory, which identifies it.
	pub(crate) ordinal: u64,
	pub(crate) name: String,
	/// The stored name may not read as its writer meant: it claimed to be UTF-8 and was not, or
	/// it came from a Unix host, which writes its own character set, and was not UTF-8. Either
	/// way it was read as CP437, a guess.
	pub(crate) name_rewritten: bool,
	pub(crate) kind: ZipKind,
	/// The compression method, after any encryption.
	pub(crate) method: u16,
	pub(crate) encryption: ZipEncryption,
	/// General-purpose flag bit 1 for LZMA: the stream ends with an end marker.
	pub(crate) lzma_end_marker: bool,
	/// General-purpose flag bit 3: a data descriptor follows the data.
	pub(crate) has_descriptor: bool,
	pub(crate) crc: u32,
	pub(crate) compressed_size: u64,
	pub(crate) size: u64,
	/// Where the local header is, relative to the start of the zip (prepended data excluded).
	pub(crate) header_offset: u64,
	/// Where its data has to end by, relative like `header_offset`: the next local header,
	/// or the central directory. Its local header's name and extra field, whose lengths only
	/// that header gives, sit between the two.
	pub(crate) data_limit: u64,
	pub(crate) modified: Option<DateTime<Utc>>,
}

/// A zip's entries, in the order their local headers are in.
#[derive(Debug)]
pub(crate) struct ZipIndex {
	pub(crate) entries: Vec<ZipEntry>,
	/// Entries whose data overlaps another's, which are not read.
	pub(crate) overlapping: Vec<ZipEntry>,
	/// Names listed more than once (up to [`MAX_DUPLICATE_NAMES`]), and how many such listings.
	pub(crate) duplicate_names: Vec<String>,
	pub(crate) duplicate_count: u64,
	/// Bytes before the zip's first entry (a self-extracting stub, or another file).
	pub(crate) prefix_bytes: u64,
	/// What the zip's recorded offsets are off by: the bytes prepended to it.
	pub(crate) shift: u64,
	/// Bytes in the central directory after its last record.
	pub(crate) directory_slack: u64,
	/// Bytes after the end record and its comment (padding, or a comment length that falls short).
	pub(crate) trailing_bytes: u64,
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
	u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
	u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
	u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

fn read_at<R: Read + Seek>(source: &mut R, offset: u64, len: usize) -> Result<Vec<u8>, ZipError> {
	bytes::read_at(
		source,
		offset,
		len,
		ZipError::Corrupt("a record runs past the end of the file"),
	)
}

/// The file's tail, read whole to find the end record, which the records around that are read
/// from when they are in it rather than fetched again.
struct Tail {
	bytes: Vec<u8>,
	/// Where it starts in the file.
	at: u64,
}

impl Tail {
	/// The `len` bytes of the file at `pos`: from the tail when it holds them.
	fn read<R: Read + Seek>(
		&self,
		source: &mut R,
		pos: u64,
		len: usize,
	) -> Result<Cow<'_, [u8]>, ZipError> {
		match pos
			.checked_sub(self.at)
			.and_then(|at| usize::try_from(at).ok())
			.and_then(|at| self.bytes.get(at..at.checked_add(len)?))
		{
			Some(bytes) => Ok(Cow::Borrowed(bytes)),
			None => read_at(source, pos, len).map(Cow::Owned),
		}
	}
}

/// Where the central directory is, and what it says of itself.
struct Directory {
	/// Where it starts in the file.
	start: u64,
	size: u64,
	entries: u64,
	/// What its recorded offsets are off by: the bytes prepended to the zip.
	shift: u64,
	/// Bytes after the end record's comment.
	trailing: u64,
}

fn find_directory<R: Read + Seek>(
	source: &mut R,
	len: u64,
	limits: ZipLimits,
) -> Result<Directory, ZipError> {
	if len < EOCD_LEN_U64 {
		return Err(ZipError::Corrupt("too short to be a zip"));
	}
	let window = len.min(EOCD_LEN_U64 + MAX_COMMENT_LEN);
	let tail = Tail {
		bytes: read_at(source, len - window, window as usize)?,
		at: len - window,
	};
	let comment_end =
		|at: usize| at as u64 + EOCD_LEN_U64 + u64::from(u16_at(&tail.bytes, at + 20));
	// the record whose comment ends the file, or else the last one whose comment fits (other
	// tools open a zip with bytes after its comment); either only if its directory can be where
	// it says, since a comment, and the bytes after one, can hold the record's signature too
	let mut candidates = 0;
	let (mut exact, mut fitting, mut implausible) = (None, None, None);
	for at in (0..=tail.bytes.len() - EOCD_LEN).rev() {
		if u32_at(&tail.bytes, at) != EOCD_SIG {
			continue;
		}
		candidates += 1;
		let end = comment_end(at);
		// a later record that fits is taken over this one unless this one ends the file
		if end <= window && (end == window || fitting.is_none()) {
			let record = &tail.bytes[at..at + EOCD_LEN];
			match (
				end == window,
				plausible(source, &tail, tail.at + at as u64, record)?,
			) {
				(true, true) => {
					exact = Some(at);
					break;
				}
				(false, true) => {
					fitting.get_or_insert(at);
				}
				// kept for the error reading it gives, should nothing else do
				(true, false) => {
					implausible.get_or_insert(at);
				}
				(false, false) => {}
			}
		}
		if candidates == MAX_EOCD_CANDIDATES {
			break;
		}
	}
	let at = exact
		.or(fitting)
		.or(implausible)
		.ok_or(ZipError::Corrupt("no end of central directory record"))?;
	let trailing = window - comment_end(at);
	let eocd_pos = tail.at + at as u64;
	let eocd = &tail.bytes[at..at + EOCD_LEN];
	if u16_at(eocd, 4) != 0 || u16_at(eocd, 6) != 0 {
		return Err(ZipError::Unsupported("a zip split across several files"));
	}
	let mut entries = u64::from(u16_at(eocd, 10));
	let mut size = u64::from(u32_at(eocd, 12));
	let mut offset = u64::from(u32_at(eocd, 16));
	let mut end = eocd_pos;

	// zip64: a locator right before the end record points at the zip64 end record
	if eocd_pos >= EOCD64_LOCATOR_LEN_U64 + EOCD64_LEN_U64 {
		let locator = tail.read(
			source,
			eocd_pos - EOCD64_LOCATOR_LEN_U64,
			EOCD64_LOCATOR_LEN,
		)?;
		if u32_at(&locator, 0) == EOCD64_LOCATOR_SIG {
			// where it is, whatever the locator says: right before the locator, unless the record
			// carries extensible data, which is then found at the recorded offset
			let recorded = u64_at(&locator, 8);
			let before = eocd_pos - EOCD64_LOCATOR_LEN_U64 - EOCD64_LEN_U64;
			let record_at =
				if tail.read(source, before, 4).map(|b| u32_at(&b, 0)).ok() == Some(EOCD64_SIG) {
					before
				} else {
					recorded
				};
			let record = tail.read(source, record_at, EOCD64_LEN)?;
			if u32_at(&record, 0) != EOCD64_SIG {
				return Err(ZipError::Corrupt("the zip64 end record is missing"));
			}
			// one elsewhere carries extensible data up to the locator; bytes between the two
			// would belong to nothing
			let reaches = u64_at(&record, 4)
				.checked_add(12)
				.and_then(|len| record_at.checked_add(len));
			if record_at != before && reaches != Some(eocd_pos - EOCD64_LOCATOR_LEN_U64) {
				return Err(ZipError::Corrupt(
					"the zip64 end record does not reach its locator",
				));
			}
			entries = u64_at(&record, 32);
			size = u64_at(&record, 40);
			offset = u64_at(&record, 48);
			end = record_at;
		}
	}

	if exceeds_limit(size, limits.max_index_bytes) {
		return Err(ZipError::TooLarge(
			"its central directory is over the index budget",
		));
	}
	// each entry takes 46 bytes at least, so the size bounds the count before anything is read
	if exceeds_limit(entries, limits.max_entries) || entries > size / CENTRAL_HEADER_LEN as u64 {
		return Err(ZipError::TooLarge(
			"it lists more entries than allowed or than fit",
		));
	}
	let start = end.checked_sub(size).ok_or(ZipError::Corrupt(
		"the central directory does not fit before its end record",
	))?;
	let shift = start.checked_sub(offset).ok_or(ZipError::Corrupt(
		"the central directory is not where the end record says",
	))?;
	Ok(Directory {
		start,
		size,
		entries,
		shift,
		trailing,
	})
}

/// Whether the end record at `eocd_pos` can be the zip's: it has a zip64 locator before it, or
/// its central directory fits before it and starts with a record's signature.
fn plausible<R: Read + Seek>(
	source: &mut R,
	tail: &Tail,
	eocd_pos: u64,
	eocd: &[u8],
) -> Result<bool, ZipError> {
	let mut signature = |pos: u64| tail.read(source, pos, 4).map(|bytes| u32_at(&bytes, 0));
	if eocd_pos >= EOCD64_LOCATOR_LEN_U64
		&& signature(eocd_pos - EOCD64_LOCATOR_LEN_U64)? == EOCD64_LOCATOR_SIG
	{
		return Ok(true);
	}
	let size = u64::from(u32_at(eocd, 12));
	let offset = u64::from(u32_at(eocd, 16));
	let Some(start) = eocd_pos.checked_sub(size).filter(|&start| offset <= start) else {
		return Ok(false);
	};
	Ok(size == 0 || signature(start)? == CENTRAL_HEADER_SIG)
}

/// Reads a zip's central directory.
pub(crate) fn read_index<R: Read + Seek>(
	source: &mut R,
	len: u64,
	limits: ZipLimits,
) -> Result<ZipIndex, ZipError> {
	let directory = find_directory(source, len, limits)?;
	let size = usize::try_from(directory.size)
		.map_err(|_| ZipError::TooLarge("its central directory is over the index budget"))?;
	let bytes = read_at(source, directory.start, size)?;
	// a capacity hint only: find_directory bounds the count by the directory's size, which fits
	let mut entries: Vec<ZipEntry> =
		Vec::with_capacity(usize::try_from(directory.entries).unwrap_or(0));
	// names are the archive's to choose: a map with fixed keys (std's on the web) could be made
	// to collide on every insert
	let mut by_name: SeededMap<String, usize> = SeededMap::default();
	let mut duplicate_names = Vec::new();
	let mut duplicate_count = 0;
	// every listed entry's data is accounted for, a replaced duplicate's included
	let mut first_header = directory.start - directory.shift;
	// the index is charged for the heap it takes, names (decoded CP437 grows up to 3 times)
	// twice over with the name map
	let mut heap_left = limits.max_index_bytes.saturating_mul(HEAP_PER_INDEX_BYTE);
	let mut at = 0;
	let mut ordinal = 0u64;
	// every record is read, however many the end record counts: old writers wrapped the count
	// at 65536, and records past it would be neither extracted nor reported
	while at + 4 <= bytes.len() && u32_at(&bytes, at) == CENTRAL_HEADER_SIG {
		if exceeds_limit(ordinal + 1, limits.max_entries) {
			return Err(ZipError::TooLarge("a zip lists more entries than allowed"));
		}
		let (entry, next) = parse_central_header(&bytes, at, ordinal)?;
		at = next;
		ordinal += 1;
		heap_left = heap_left
			.checked_sub(mem::size_of::<ZipEntry>() as u64 + 2 * entry.name.len() as u64)
			.ok_or(ZipError::TooLarge("a zip's index over the memory budget"))?;
		first_header = first_header.min(entry.header_offset);
		match by_name.get(&entry.name) {
			Some(&index) => {
				duplicate_count += 1;
				if duplicate_names.len() < MAX_DUPLICATE_NAMES {
					duplicate_names.push(entry.name.chars().take(256).collect());
				}
				// the last one listed wins, as with other zip tools
				entries[index] = entry;
			}
			None => {
				by_name.insert(entry.name.clone(), entries.len());
				entries.push(entry);
			}
		}
	}
	if ordinal != directory.entries && ordinal % 0x1_0000 != directory.entries % 0x1_0000 {
		return Err(ZipError::Corrupt(
			"a zip's central directory holds other than the entries it counts",
		));
	}
	// bytes in the central directory after its last record belong to nothing
	let directory_slack = (bytes.len() - at) as u64;
	drop(by_name);
	drop(bytes);
	entries.sort_by_key(|entry| entry.header_offset);

	let prefix_bytes = directory.shift + first_header;
	// a local header and the data after it take at least its fixed part and the stored size;
	// an entry starting inside that span of an earlier one shares its bytes
	let mut kept = Vec::with_capacity(entries.len());
	let mut overlapping = Vec::new();
	let mut end = 0u64;
	for entry in entries {
		let start = entry.header_offset;
		let data_end = start
			.saturating_add(LOCAL_HEADER_LEN_U64)
			.saturating_add(entry.compressed_size);
		if start < end || data_end > directory.start - directory.shift {
			overlapping.push(entry);
			continue;
		}
		end = data_end;
		kept.push(entry);
	}
	let directory_start = directory.start - directory.shift;
	let limits: Vec<u64> = kept
		.iter()
		.skip(1)
		.map(|entry| entry.header_offset)
		.chain([directory_start])
		.collect();
	for (entry, limit) in kept.iter_mut().zip(limits) {
		entry.data_limit = limit;
	}
	Ok(ZipIndex {
		entries: kept,
		overlapping,
		duplicate_names,
		duplicate_count,
		prefix_bytes,
		shift: directory.shift,
		directory_slack,
		trailing_bytes: directory.trailing,
	})
}

fn parse_central_header(
	bytes: &[u8],
	at: usize,
	ordinal: u64,
) -> Result<(ZipEntry, usize), ZipError> {
	let header = bytes
		.get(at..at + CENTRAL_HEADER_LEN)
		.ok_or(ZipError::Corrupt(
			"the central directory ends inside an entry",
		))?;
	if u32_at(header, 0) != CENTRAL_HEADER_SIG {
		return Err(ZipError::Corrupt(
			"a central directory entry has no signature",
		));
	}
	let made_by_unix = matches!(u16_at(header, 4) >> 8, HOST_UNIX | HOST_OS_X);
	let flags = u16_at(header, 8);
	let mut method = u16_at(header, 10);
	let dos_time = u16_at(header, 12);
	let dos_date = u16_at(header, 14);
	let crc = u32_at(header, 16);
	let mut compressed_size = u64::from(u32_at(header, 20));
	let mut size = u64::from(u32_at(header, 24));
	let name_len = usize::from(u16_at(header, 28));
	let extra_len = usize::from(u16_at(header, 30));
	let comment_len = usize::from(u16_at(header, 32));
	let external = u32_at(header, 38);
	let mut header_offset = u64::from(u32_at(header, 42));
	let name_at = at + CENTRAL_HEADER_LEN;
	let next = name_at + name_len + extra_len + comment_len;
	let raw_name = bytes
		.get(name_at..name_at + name_len)
		.ok_or(ZipError::Corrupt(
			"the central directory ends inside a name",
		))?;
	let extra = bytes
		.get(name_at + name_len..name_at + name_len + extra_len)
		.ok_or(ZipError::Corrupt(
			"the central directory ends inside an extra field",
		))?;
	if next > bytes.len() {
		return Err(ZipError::Corrupt(
			"the central directory ends inside a comment",
		));
	}

	let mut unicode_name = None;
	let mut modified = None;
	let mut unix_modified = None;
	let mut aes = None;
	let mut fields = extra;
	while fields.len() >= 4 {
		let id = u16_at(fields, 0);
		let len = usize::from(u16_at(fields, 2));
		let Some(data) = fields.get(4..4 + len) else {
			break;
		};
		match id {
			// zip64: the values the fixed fields hold 0xFFFFFFFF for, in this order
			0x0001 => {
				let mut values = data.chunks_exact(8).map(|value| u64_at(value, 0));
				if size == 0xFFFF_FFFF {
					size = values
						.next()
						.ok_or(ZipError::Corrupt("a short zip64 field"))?;
				}
				if compressed_size == 0xFFFF_FFFF {
					compressed_size = values
						.next()
						.ok_or(ZipError::Corrupt("a short zip64 field"))?;
				}
				if header_offset == 0xFFFF_FFFF {
					header_offset = values
						.next()
						.ok_or(ZipError::Corrupt("a short zip64 field"))?;
				}
			}
			// NTFS times: a tag of mtime, atime and ctime as FILETIMEs
			0x000A if data.len() >= 32 && u16_at(data, 4) == 1 && u16_at(data, 6) >= 24 => {
				modified = filetime(u64_at(data, 8));
			}
			// Unix extended timestamp: flags, then the modification time when flagged
			0x5455 if data.len() >= 5 && data[0] & 1 == 1 => {
				let secs = i32::from_le_bytes(data[1..5].try_into().expect("4 bytes"));
				unix_modified = DateTime::from_timestamp(i64::from(secs), 0);
			}
			// Info-ZIP Unicode path, valid only for the name it was written with
			0x7075 if data.len() >= 5 && data[0] == 1 => {
				if u32_at(data, 1) == crc32fast::hash(raw_name) {
					unicode_name = std::str::from_utf8(&data[5..]).ok().map(str::to_owned);
				}
			}
			// WinZip AES: vendor version, "AE", strength, the method under the encryption
			0x9901 if data.len() >= 7 => {
				let strength = AesStrength::try_from(data[4])
					.map_err(|_| ZipError::Unsupported("an AES strength"))?;
				aes = Some((strength, u16_at(data, 0) == 2, u16_at(data, 5)));
			}
			_ => {}
		}
		fields = &fields[4 + len..];
	}

	let (name, name_rewritten) = if let Some(name) = unicode_name {
		(name, false)
	} else if flags & FLAG_UTF8 != 0 {
		match std::str::from_utf8(raw_name) {
			Ok(name) => (name.to_owned(), false),
			Err(_) => (cp437::decode(raw_name), true),
		}
	} else {
		// macOS Archive Utility, ditto and Info-ZIP on Unix store names in the system's
		// character set without saying so, UTF-8 by now: one that reads as UTF-8 is taken as
		// that, as 7-Zip does, and one that does not is in a set nothing records
		match (std::str::from_utf8(raw_name), made_by_unix) {
			(Ok(name), true) => (name.to_owned(), false),
			(Err(_), true) => (cp437::decode(raw_name), true),
			(_, false) => (cp437::decode(raw_name), false),
		}
	};
	let encryption = if flags & FLAG_ENCRYPTED == 0 {
		ZipEncryption::None
	} else if flags & FLAG_STRONG_ENCRYPTION != 0 {
		return Err(ZipError::Unsupported("PKWARE strong encryption"));
	} else if method == METHOD_AES {
		let (strength, authenticated_only, actual) =
			aes.ok_or(ZipError::Corrupt("an AES entry without its AES field"))?;
		method = actual;
		ZipEncryption::Aes {
			strength,
			authenticated_only,
		}
	} else {
		// with a data descriptor, the check byte is the high byte of the DOS time instead of
		// the CRC's
		let check = if flags & FLAG_DATA_DESCRIPTOR != 0 {
			(dos_time >> 8) as u8
		} else {
			(crc >> 24) as u8
		};
		ZipEncryption::ZipCrypto { check }
	};
	let unix_mode = external >> 16;
	let kind = if made_by_unix && unix_mode & 0o170_000 == 0o120_000 {
		ZipKind::Symlink
	} else if name.ends_with('/') || (external & 0x10 != 0 && size == 0) {
		ZipKind::Dir
	} else {
		ZipKind::File
	};
	let modified = modified
		.or(unix_modified)
		.or_else(|| dos_datetime(dos_date, dos_time));
	Ok((
		ZipEntry {
			ordinal,
			name,
			name_rewritten,
			kind,
			method,
			encryption,
			lzma_end_marker: flags & FLAG_LZMA_END_MARKER != 0,
			has_descriptor: flags & FLAG_DATA_DESCRIPTOR != 0,
			crc,
			compressed_size,
			size,
			header_offset,
			data_limit: 0,
			modified,
		},
		next,
	))
}

/// A Windows FILETIME (100 ns ticks since 1601) as a time, if it is one chrono can hold.
fn filetime(ticks: u64) -> Option<DateTime<Utc>> {
	const EPOCH_DIFFERENCE_SECS: i64 = 11_644_473_600;
	let secs = i64::try_from(ticks / 10_000_000).ok()? - EPOCH_DIFFERENCE_SECS;
	DateTime::from_timestamp(secs, (ticks % 10_000_000) as u32 * 100)
}

/// A DOS date and time, which is in whatever time zone the writer was in: read as local time,
/// which is right for an archive made on the same machine or in the same zone.
fn dos_datetime(date: u16, time: u16) -> Option<DateTime<Utc>> {
	let day = NaiveDate::from_ymd_opt(
		1980 + i32::from(date >> 9),
		u32::from((date >> 5) & 0x0F),
		u32::from(date & 0x1F),
	)?;
	let naive = day.and_hms_opt(
		u32::from(time >> 11),
		u32::from((time >> 5) & 0x3F),
		2 * u32::from(time & 0x1F),
	)?;
	Local
		.from_local_datetime(&naive)
		.earliest()
		.map(|time| time.with_timezone(&Utc))
}

/// Bytes between the end of `entry` (its local header, name, extra field and data) and its
/// `data_limit` that belong to nothing: anything but the data descriptor an entry flagged for one
/// ends with (12 to 24 bytes, with or without its signature and zip64 sizes). A local header that
/// cannot be read counts as a whole.
pub(crate) fn unaccounted_after<R: Read + Seek>(
	source: &mut R,
	shift: u64,
	entry: &ZipEntry,
) -> u64 {
	let whole = entry.data_limit.saturating_sub(entry.header_offset);
	let Ok(header) = read_at(source, shift + entry.header_offset, LOCAL_HEADER_LEN) else {
		return whole;
	};
	if u32_at(&header, 0) != LOCAL_HEADER_SIG {
		return whole;
	}
	let skip = u64::from(u16_at(&header, 26)) + u64::from(u16_at(&header, 28));
	let end = entry
		.header_offset
		.saturating_add(LOCAL_HEADER_LEN_U64 + skip)
		.saturating_add(entry.compressed_size);
	match entry.data_limit.saturating_sub(end) {
		12 | 16 | 20 | 24 if entry.has_descriptor => 0,
		gap => gap,
	}
}

impl ZipEntry {
	/// Whether [`open_entry`] reads the entry's compression method under its encryption.
	pub(crate) fn supported(&self) -> bool {
		self.unsupported().is_none()
	}

	/// What of the entry [`open_entry`] cannot read, if anything: its compression method, or
	/// that method under its encryption. LZMA and XZ have their own memory limits, which the
	/// encrypted forms would first have to buffer around: only their plain forms are read.
	pub(crate) fn unsupported(&self) -> Option<&'static str> {
		match self.method {
			METHOD_STORED | METHOD_DEFLATE | METHOD_DEFLATE64 | METHOD_BZIP2 | METHOD_ZSTD => None,
			METHOD_LZMA | METHOD_XZ if self.encryption == ZipEncryption::None => None,
			METHOD_LZMA | METHOD_XZ => Some("an encrypted LZMA or XZ entry"),
			_ => Some("a compression method"),
		}
	}
}

/// Memory a zip entry's decoder may use.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EntryLimits {
	pub(crate) decoder_memory: u64,
}

/// Opens an entry's data, decrypted and decompressed. Reading it to `Ok(0)` checks its CRC-32
/// (or, for an AE-2 entry, its authentication code) and its size; the data is cut off one byte
/// past its stated size, which then fails the check.
pub(crate) fn open_entry<'s, R: Read + Seek>(
	source: &'s mut R,
	shift: u64,
	entry: &ZipEntry,
	password: Option<&ArchivePassword>,
	limits: EntryLimits,
) -> Result<Box<dyn Read + 's>, ZipError> {
	let header = read_at(source, shift + entry.header_offset, LOCAL_HEADER_LEN)?;
	if u32_at(&header, 0) != LOCAL_HEADER_SIG {
		return Err(ZipError::Corrupt(
			"an entry's local header has no signature",
		));
	}
	let skip = u64::from(u16_at(&header, 26)) + u64::from(u16_at(&header, 28));
	// the lengths of the local name and extra field are only known now: data they push into
	// the next entry's is data two entries share
	if entry
		.header_offset
		.saturating_add(LOCAL_HEADER_LEN_U64 + skip)
		.saturating_add(entry.compressed_size)
		> entry.data_limit
	{
		return Err(ZipError::Overlapping);
	}
	source.seek(SeekFrom::Current(skip as i64))?;
	let stored = (&mut *source).take(entry.compressed_size);

	let decrypted: Box<dyn Read + 's> = match entry.encryption {
		ZipEncryption::None => Box::new(stored),
		ZipEncryption::ZipCrypto { check } => {
			let password = password.ok_or(ZipError::PasswordRequired)?;
			Box::new(ZipCryptoReader::new(stored, password, check)?)
		}
		ZipEncryption::Aes { strength, .. } => {
			let password = password.ok_or(ZipError::PasswordRequired)?;
			Box::new(AesReader::new(
				stored,
				password,
				strength,
				entry.compressed_size,
			)?)
		}
	};
	// the decompressor reads through a shared handle, so what it leaves (an AES entry's last
	// bytes and its authentication code) can be read to the end once it is done
	let decrypted = Shared(Rc::new(RefCell::new(decrypted)));
	let rest = decrypted.clone();
	if let Some(what) = entry.unsupported() {
		return Err(ZipError::Unsupported(what));
	}
	let decoded: Box<dyn Read + 's> = match entry.method {
		METHOD_STORED => Box::new(decrypted),
		METHOD_DEFLATE => Box::new(flate2::read::DeflateDecoder::new(decrypted)),
		METHOD_DEFLATE64 => Box::new(deflate64::Deflate64Decoder::new(decrypted)),
		METHOD_BZIP2 => Box::new(bzip2::read::BzDecoder::new(decrypted)),
		METHOD_LZMA => lzma_entry(Box::new(decrypted), entry, limits)?,
		METHOD_ZSTD => stream_entry(StreamCodec::Zstd, decrypted, limits)?,
		METHOD_XZ => stream_entry(StreamCodec::Xz, decrypted, limits)?,
		_ => unreachable!("unsupported() refused every other method above"),
	};
	let check_crc = !matches!(
		entry.encryption,
		ZipEncryption::Aes {
			authenticated_only: true,
			..
		}
	);
	Ok(Box::new(Checked {
		inner: decoded.take(entry.size.saturating_add(1)),
		rest,
		crc: crc32fast::Hasher::new(),
		expected_crc: check_crc.then_some(entry.crc),
		size: entry.size,
		read: 0,
	}))
}

/// A reader two decoders share, one after the other.
struct Shared<'s>(Rc<RefCell<Box<dyn Read + 's>>>);

impl Clone for Shared<'_> {
	fn clone(&self) -> Self {
		Self(Rc::clone(&self.0))
	}
}

impl Read for Shared<'_> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		self.0.borrow_mut().read(buf)
	}
}

/// An entry stored as a whole stream of `codec` (XZ, zstd), decoded within the codec budget.
fn stream_entry<'s>(
	codec: StreamCodec,
	input: Shared<'s>,
	limits: EntryLimits,
) -> Result<Box<dyn Read + 's>, ZipError> {
	let decoder =
		open_stream(codec, input, limits.decoder_memory).map_err(|e| ZipError::Read(e.into()))?;
	Ok(Box::new(WholeStream(decoder)))
}

/// A stream decoder whose input is the entry's stored data and nothing else: bytes after its
/// stream, which the decoder reads to its end, belong to no data, and damage the entry. Zero
/// bytes there are padding, as behind a standalone stream.
///
/// Stricter than the other methods on purpose. Their decoders stop at their stream's end and
/// never see what follows it, which [`Checked`] drains unread, as other zip readers do. A
/// whole-stream decoder reads on to its input's end to take every frame of a stream of
/// several, so it sees and counts what follows the last one, and an entry holds one stream.
struct WholeStream<'s>(Box<dyn StreamDecoder + 's>);

impl Read for WholeStream<'_> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let n = self.0.read(buf)?;
		if n == 0 && !buf.is_empty() && self.0.end().is_some_and(|end| end.unaccounted_bytes > 0) {
			return Err(io::Error::new(
				io::ErrorKind::InvalidData,
				ZipError::Corrupt("an entry holds data after its compressed stream"),
			));
		}
		Ok(n)
	}
}

/// Zip's LZMA: a version, the properties' length and the properties, then the raw stream.
fn lzma_entry<'s>(
	mut input: Box<dyn Read + 's>,
	entry: &ZipEntry,
	limits: EntryLimits,
) -> Result<Box<dyn Read + 's>, ZipError> {
	let mut header = [0u8; 9];
	input.read_exact(&mut header)?;
	if u16_at(&header, 2) != 5 {
		return Err(ZipError::Corrupt("LZMA properties of an unexpected length"));
	}
	let props = header[4];
	let dict_size = u32_at(&header, 5);
	let decoded_size = if entry.lzma_end_marker {
		u64::MAX
	} else {
		entry.size
	};
	let dict_size = clamp_lzma_dict(dict_size, Some(entry.size));
	let kib = lzma_rust2::lzma_get_memory_usage_by_props(dict_size, props)
		.map_err(|_| ZipError::Corrupt("invalid LZMA properties"))?;
	if u64::from(kib) * 1024 > limits.decoder_memory {
		return Err(ZipError::TooLarge(
			"an entry's LZMA dictionary is over the codec budget",
		));
	}
	let reader = lzma_rust2::LzmaReader::new_with_props(
		BufReader::new(input),
		decoded_size,
		props,
		dict_size,
		None,
	)?;
	Ok(Box::new(reader))
}

impl From<CryptoError> for ZipError {
	fn from(error: CryptoError) -> Self {
		match error {
			CryptoError::WrongPassword => ZipError::WrongPassword,
			CryptoError::TooShort => ZipError::Corrupt("an AES entry too short for its header"),
			CryptoError::Read(error) => ZipError::Read(error),
			// only a read to the end checks the code, but it is damage all the same
			error @ CryptoError::AuthenticationFailed => ZipError::Read(error.into()),
		}
	}
}

/// An entry's data, checked against its stated size and CRC-32 once read to the end.
struct Checked<'s, R> {
	inner: io::Take<R>,
	/// The decrypted stored data under the decompressor, read to its end once the data is.
	rest: Shared<'s>,
	crc: crc32fast::Hasher,
	expected_crc: Option<u32>,
	size: u64,
	read: u64,
}

impl<R: Read> Read for Checked<'_, R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		let n = self.inner.read(buf)?;
		self.read += n as u64;
		if self.read > self.size {
			return Err(io::Error::new(
				io::ErrorKind::InvalidData,
				ZipError::Corrupt("an entry holds more data than it states"),
			));
		}
		if n > 0 {
			self.crc.update(&buf[..n]);
			return Ok(n);
		}
		if self.read != self.size {
			return Err(io::Error::new(
				io::ErrorKind::InvalidData,
				ZipError::Corrupt("an entry holds less data than it states"),
			));
		}
		// what the decompressor did not need, up to an AES entry's authentication code, which
		// is checked when its reader reaches the end
		io::copy(&mut self.rest, &mut io::sink())?;
		if let Some(expected) = self.expected_crc
			&& self.crc.clone().finalize() != expected
		{
			return Err(io::Error::new(
				io::ErrorKind::InvalidData,
				ZipError::Corrupt("an entry's CRC-32 does not match"),
			));
		}
		Ok(0)
	}
}

#[cfg(test)]
mod tests;
