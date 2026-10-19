//! Walks the members of a tar stream: headers read with the `tar` crate's field parsing, but the
//! stream position, the GNU and PAX records that modify a member, and every size limit kept here.
//!
//! The `tar` crate's own iterator either reads long-name and PAX records without a size limit, or
//! (in raw mode) leaves them to the caller while computing member sizes from the ustar field
//! alone, so a PAX `size` that differs would desync the two views of the stream — the parser
//! differential behind CVE-2025-62518. Here the PAX `size` overrides the ustar field, as POSIX,
//! GNU tar and bsdtar have it, and only this reader decides where the next header starts.
//!
//! A hard link's size is honoured as libarchive honours it: POSIX lets a hard link in a pax
//! archive carry the file's data, while older writers put the target's size on a hard link and
//! no data after it. So the header's size holds only once the archive is known to be pax (a PAX
//! header was seen, and no GNU or pre-POSIX header since), and is 0 on any other; a PAX `size`
//! record for the link itself always holds. GNU tar reads it the same way, but for ignoring the
//! header's size in a pax archive too. Reading a stated size as data anywhere else would let a
//! link hide members from both tools inside what they take for headers.

use std::io::{self, Read};

use tar::{EntryType, GnuExtSparseHeader, Header};

use crate::{Error, ErrorKind, fs::drive_job::exceeds_limit};

use super::{bytes::read_full, error::read_failure, format::is_tar_header};

/// Bytes of a tar block: headers, and member data padded to a whole number of them.
pub(crate) const TAR_BLOCK: usize = 512;
/// [`TAR_BLOCK`] as a size in a stream.
pub(crate) const TAR_BLOCK_U64: u64 = TAR_BLOCK as u64;
/// Bytes of a ustar header's name field; a longer path needs a GNU long-name or PAX record.
pub(crate) const USTAR_NAME_LEN: usize = 100;
/// Largest GNU long name or long link record read.
const MAX_LONG_NAME: u64 = 64 * 1024;
/// Largest PAX extended header record read.
const MAX_PAX: u64 = 1024 * 1024;
/// Most GNU extended sparse header blocks read for one member.
const MAX_SPARSE_BLOCKS: u32 = 4096;

/// What kind of item a member is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberKind {
	File,
	Dir,
	Symlink {
		/// The link target as stored, decoded like a path.
		target: String,
	},
	/// A second name for a member stored earlier, whose own data (when `size` is not 0: POSIX
	/// lets a pax hard link carry it) [`TarReader::read_body`] reads.
	Hardlink {
		/// The earlier member's path as stored, decoded like a path.
		target: String,
	},
	Device,
	Fifo,
	/// Stored with holes left out; skipped, since only its stored data could be written.
	Sparse,
	/// A type this reader does not extract (multivolume continuations, vendor types).
	Unsupported(u8),
}

/// A member's modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MemberTime {
	pub(crate) secs: i64,
	pub(crate) nanos: u32,
}

/// One member's header, with every GNU and PAX record before it applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TarMember {
	pub(crate) kind: MemberKind,
	/// The stored path, decoded as UTF-8 or else byte by byte as Latin-1.
	pub(crate) path: String,
	/// Whether decoding the path had to fall back to Latin-1.
	pub(crate) path_rewritten: bool,
	/// Bytes of data stored for the member, which [`TarReader::read_body`] reads.
	pub(crate) size: u64,
	pub(crate) modified: Option<MemberTime>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TarError {
	#[error(transparent)]
	Read(#[from] io::Error),
	#[error("the tar archive is damaged: {0}")]
	Corrupt(&'static str),
	/// More headers than the caller allows.
	#[error("the tar archive has more than {0} members")]
	TooManyMembers(u64),
}

impl From<TarError> for Error {
	fn from(error: TarError) -> Self {
		let kind = match error {
			TarError::Read(error) => return read_failure(error),
			TarError::Corrupt(_) => ErrorKind::ArchiveCorrupt,
			TarError::TooManyMembers(_) => ErrorKind::ArchiveTooLarge,
		};
		Error::custom_with_source(kind, error, None::<&str>)
	}
}

/// The records seen before a member, applied to it.
#[derive(Debug, Default)]
struct Pending {
	long_name: Option<Vec<u8>>,
	long_link: Option<Vec<u8>>,
	pax: Option<PaxOverrides>,
}

#[derive(Debug, Default)]
struct PaxOverrides {
	path: Option<Vec<u8>>,
	link_path: Option<Vec<u8>>,
	size: Option<u64>,
	modified: Option<MemberTime>,
	sparse: bool,
}

/// What the headers so far say the archive is, as libarchive tracks it for a hard link's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
	/// No header with a magic yet, or the last one had none.
	Old,
	Gnu,
	Ustar,
	/// A PAX header was seen; ustar headers after it keep the archive pax.
	Pax,
}

impl Flavor {
	/// What the archive is after `header`, when it was `self` before.
	fn after(self, header: &Header) -> Self {
		match header.entry_type().as_byte() {
			// PAX extended and global headers, Sun's and Solaris ACLs, as libarchive has them
			b'x' | b'g' | b'X' | b'A' => Self::Pax,
			// GNU long names and links and volume headers change nothing
			b'L' | b'K' | b'V' => self,
			_ if header.as_gnu().is_some() => Self::Gnu,
			_ if header.as_ustar().is_some() && self == Self::Pax => Self::Pax,
			_ if header.as_ustar().is_some() => Self::Ustar,
			_ => Self::Old,
		}
	}
}

pub(crate) struct TarReader<R> {
	inner: R,
	flavor: Flavor,
	/// Unread bytes of the current member's data, then its padding to the next block.
	remaining: u64,
	padding: u64,
	members: u64,
	max_members: u64,
	ended: bool,
}

impl<R: Read> TarReader<R> {
	/// Reads tar headers from `inner`; more than `max_members` headers (every record counts)
	/// is refused.
	pub(crate) fn new(inner: R, max_members: u64) -> Self {
		Self {
			inner,
			flavor: Flavor::Old,
			remaining: 0,
			padding: 0,
			members: 0,
			max_members,
			ended: false,
		}
	}

	/// The next member, skipping whatever of the previous one's data was not read. `None` once
	/// the end-of-archive marker (or the end of the stream at a block boundary) is reached.
	pub(crate) fn next_member(&mut self) -> Result<Option<TarMember>, TarError> {
		if self.ended {
			return Ok(None);
		}
		self.skip_rest()?;
		let mut pending = Pending::default();
		loop {
			let Some(block) = self.read_header_block()? else {
				self.ended = true;
				return Ok(None);
			};
			self.members += 1;
			if exceeds_limit(self.members, self.max_members) {
				return Err(TarError::TooManyMembers(self.max_members));
			}
			let header = Header::from_byte_slice(&block);
			let size = header
				.entry_size()
				.map_err(|_| TarError::Corrupt("a member's size field is invalid"))?;
			self.flavor = self.flavor.after(header);
			match header.entry_type() {
				EntryType::GNULongName => {
					let name = self.read_record(size, MAX_LONG_NAME)?;
					set_once(&mut pending.long_name, trim_nuls(name))?;
				}
				EntryType::GNULongLink => {
					let link = self.read_record(size, MAX_LONG_NAME)?;
					set_once(&mut pending.long_link, trim_nuls(link))?;
				}
				EntryType::XHeader => {
					let record = self.read_record(size, MAX_PAX)?;
					set_once(&mut pending.pax, parse_pax(&record)?)?;
				}
				EntryType::XGlobalHeader => {
					// defaults for every following member; GNU tar ignores them, and so do we
					self.read_record(size, MAX_PAX)?;
				}
				_ => return self.member(header, size, pending).map(Some),
			}
		}
	}

	fn member(
		&mut self,
		header: &Header,
		stored_size: u64,
		pending: Pending,
	) -> Result<TarMember, TarError> {
		let pax = pending.pax.unwrap_or_default();
		let raw_path = pax
			.path
			.or(pending.long_name)
			.unwrap_or_else(|| header.path_bytes().into_owned());
		let entry_type = header.entry_type();
		// a hard link's own header states data only in a pax archive; its PAX record always
		let stored_size = match entry_type {
			EntryType::Link if self.flavor != Flavor::Pax => 0,
			_ => stored_size,
		};
		let size = pax.size.unwrap_or(stored_size);
		let modified = pax.modified.or_else(|| {
			header.mtime().ok().and_then(|secs| {
				Some(MemberTime {
					secs: i64::try_from(secs).ok()?,
					nanos: 0,
				})
			})
		});
		let mut kind = match entry_type {
			EntryType::Regular | EntryType::Continuous => MemberKind::File,
			EntryType::Directory => MemberKind::Dir,
			EntryType::Symlink => MemberKind::Symlink {
				target: link_target(pax.link_path, pending.long_link, header),
			},
			EntryType::Link => MemberKind::Hardlink {
				target: link_target(pax.link_path, pending.long_link, header),
			},
			EntryType::Char | EntryType::Block => MemberKind::Device,
			EntryType::Fifo => MemberKind::Fifo,
			EntryType::GNUSparse => {
				self.skip_sparse_extensions(header)?;
				MemberKind::Sparse
			}
			other => MemberKind::Unsupported(other.as_byte()),
		};
		if pax.sparse {
			kind = MemberKind::Sparse;
		}
		// pre-POSIX tars mark a directory with a trailing slash on a regular member
		if kind == MemberKind::File && raw_path.ends_with(b"/") {
			kind = MemberKind::Dir;
		}
		let (path, path_rewritten) = decode_path(&raw_path);
		self.remaining = size;
		self.padding = padding(size);
		Ok(TarMember {
			kind,
			path,
			path_rewritten,
			size,
			modified,
		})
	}

	/// Reads the current member's data; `Ok(0)` once all of it was read.
	pub(crate) fn read_body(&mut self, buf: &mut [u8]) -> Result<usize, TarError> {
		if self.remaining == 0 || buf.is_empty() {
			return Ok(0);
		}
		let want = buf
			.len()
			.min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
		let read = self.inner.read(&mut buf[..want])?;
		if read == 0 {
			return Err(TarError::Corrupt("a member's data ends early"));
		}
		self.remaining -= read as u64;
		Ok(read)
	}

	/// The stream after the end-of-archive marker, to drain (whatever follows is not part of any
	/// member).
	pub(crate) fn into_inner(self) -> R {
		self.inner
	}

	fn skip_rest(&mut self) -> Result<(), TarError> {
		// a member size near u64::MAX (PAX, or GNU base-256) must not wrap to a short skip,
		// which would read the member's data as headers other tools never see
		let rest = self
			.remaining
			.checked_add(self.padding)
			.ok_or(TarError::Corrupt("a member's size is out of range"))?;
		self.remaining = 0;
		self.padding = 0;
		let skipped = io::copy(&mut (&mut self.inner).take(rest), &mut io::sink())?;
		if skipped != rest {
			return Err(TarError::Corrupt("a member's data ends early"));
		}
		Ok(())
	}

	/// The next header block, or `None` at the end-of-archive marker or a stream that ends at a
	/// block boundary.
	fn read_header_block(&mut self) -> Result<Option<[u8; TAR_BLOCK]>, TarError> {
		let mut block = [0u8; TAR_BLOCK];
		let read = read_full(&mut self.inner, &mut block)?;
		if read == 0 {
			return Ok(None);
		}
		if read < block.len() {
			return Err(TarError::Corrupt("the archive ends inside a header"));
		}
		if block.iter().all(|&b| b == 0) {
			return Ok(None);
		}
		if !is_tar_header(&block) {
			return Err(TarError::Corrupt("a header's checksum does not match"));
		}
		Ok(Some(block))
	}

	/// Reads a record member's data of `size` bytes, refusing one larger than `max` before
	/// reading anything.
	fn read_record(&mut self, size: u64, max: u64) -> Result<Vec<u8>, TarError> {
		if size > max {
			return Err(TarError::Corrupt(
				"a long name or extended header is too large",
			));
		}
		let mut record = vec![0u8; usize::try_from(size).expect("capped above")];
		if read_full(&mut self.inner, &mut record)? != record.len() {
			return Err(TarError::Corrupt(
				"a long name or extended header ends early",
			));
		}
		let pad = padding(size);
		if io::copy(&mut (&mut self.inner).take(pad), &mut io::sink())? != pad {
			return Err(TarError::Corrupt(
				"a long name or extended header ends early",
			));
		}
		Ok(record)
	}

	/// Reads past the extended sparse headers of an old GNU sparse member.
	fn skip_sparse_extensions(&mut self, header: &Header) -> Result<(), TarError> {
		let gnu = header
			.as_gnu()
			.ok_or(TarError::Corrupt("a sparse member without a GNU header"))?;
		let mut extended = gnu.is_extended();
		let mut blocks = 0;
		while extended {
			blocks += 1;
			if blocks > MAX_SPARSE_BLOCKS {
				return Err(TarError::Corrupt("a sparse member's map is too long"));
			}
			let mut block = GnuExtSparseHeader::new();
			if read_full(&mut self.inner, block.as_mut_bytes())? != TAR_BLOCK {
				return Err(TarError::Corrupt("a sparse member's map ends early"));
			}
			extended = block.isextended[0] != 0;
		}
		Ok(())
	}
}

fn padding(size: u64) -> u64 {
	(TAR_BLOCK_U64 - size % TAR_BLOCK_U64) % TAR_BLOCK_U64
}

fn set_once<T>(slot: &mut Option<T>, value: T) -> Result<(), TarError> {
	if slot.is_some() {
		return Err(TarError::Corrupt(
			"a member has two records of the same kind",
		));
	}
	*slot = Some(value);
	Ok(())
}

fn trim_nuls(mut bytes: Vec<u8>) -> Vec<u8> {
	while bytes.last() == Some(&0) {
		bytes.pop();
	}
	bytes
}

/// A link member's target: its PAX `linkpath`, else its GNU long link, else the header's own
/// field.
fn link_target(pax: Option<Vec<u8>>, long_link: Option<Vec<u8>>, header: &Header) -> String {
	let target = pax
		.or(long_link)
		.or_else(|| header.link_name_bytes().map(|link| link.into_owned()))
		.unwrap_or_default();
	decode_path(&target).0
}

/// A stored path as text: UTF-8 when it is, otherwise each byte as the Latin-1 character of the
/// same value (lossless, and what most tars without a charset wrote), flagged as rewritten.
fn decode_path(bytes: &[u8]) -> (String, bool) {
	match std::str::from_utf8(bytes) {
		Ok(path) => (path.to_owned(), false),
		Err(_) => (bytes.iter().map(|&b| char::from(b)).collect(), true),
	}
}

/// Parses a PAX extended header: records of `LEN KEY=VALUE\n`, where `LEN` counts the whole
/// record. Each record's length is checked against what is left, so a malformed one cannot run
/// past the header.
fn parse_pax(mut record: &[u8]) -> Result<PaxOverrides, TarError> {
	const MALFORMED: TarError = TarError::Corrupt("a PAX record is malformed");
	let mut overrides = PaxOverrides::default();
	while !record.is_empty() {
		let space = record.iter().position(|&b| b == b' ').ok_or(MALFORMED)?;
		let len: usize = std::str::from_utf8(&record[..space])
			.ok()
			.filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
			.and_then(|digits| digits.parse().ok())
			.ok_or(MALFORMED)?;
		if len <= space + 1 || len > record.len() || record[len - 1] != b'\n' {
			return Err(MALFORMED);
		}
		let body = &record[space + 1..len - 1];
		let equals = body.iter().position(|&b| b == b'=').ok_or(MALFORMED)?;
		let (key, value) = (&body[..equals], &body[equals + 1..]);
		match key {
			b"path" => overrides.path = Some(value.to_vec()),
			b"linkpath" => overrides.link_path = Some(value.to_vec()),
			b"size" => {
				overrides.size = Some(
					std::str::from_utf8(value)
						.ok()
						.and_then(|size| size.parse().ok())
						.ok_or(MALFORMED)?,
				)
			}
			b"mtime" => overrides.modified = parse_pax_time(value),
			key if key.starts_with(b"GNU.sparse.") => overrides.sparse = true,
			_ => {}
		}
		record = &record[len..];
	}
	Ok(overrides)
}

/// A PAX time: optional sign, whole seconds, optional fraction.
fn parse_pax_time(value: &[u8]) -> Option<MemberTime> {
	let value = std::str::from_utf8(value).ok()?;
	let (negative, value) = match value.strip_prefix('-') {
		Some(rest) => (true, rest),
		None => (false, value),
	};
	let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
	if whole.is_empty()
		|| !whole.bytes().all(|b| b.is_ascii_digit())
		|| !fraction.bytes().all(|b| b.is_ascii_digit())
	{
		return None;
	}
	let whole: i64 = whole.parse().ok()?;
	let nanos: u32 = format!("{:0<9}", fraction.get(..fraction.len().min(9))?)
		.parse()
		.ok()?;
	Some(if negative && nanos > 0 {
		MemberTime {
			secs: -whole - 1,
			nanos: 1_000_000_000 - nanos,
		}
	} else {
		MemberTime {
			secs: if negative { -whole } else { whole },
			nanos,
		}
	})
}

#[cfg(test)]
mod tests;
