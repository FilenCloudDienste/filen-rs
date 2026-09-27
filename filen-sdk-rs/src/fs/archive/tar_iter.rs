//! Walks the members of a tar stream: headers read with the `tar` crate's field parsing, but the
//! stream position, the GNU and PAX records that modify a member, and every size limit kept here.
//!
//! The `tar` crate's own iterator either reads long-name and PAX records without a size limit, or
//! (in raw mode) leaves them to the caller while computing member sizes from the ustar field
//! alone, so a PAX `size` that differs would desync the two views of the stream — the parser
//! differential behind CVE-2025-62518. Here the PAX `size` overrides the ustar field, as POSIX,
//! GNU tar and bsdtar have it, and only this reader decides where the next header starts.

use std::io::{self, Read};

use tar::{EntryType, Header};

use super::format::is_tar_header;

const BLOCK: u64 = 512;
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
	Hardlink,
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
	#[error("corrupt tar archive: {0}")]
	Corrupt(&'static str),
	/// More headers than the caller allows.
	#[error("the tar archive has more than {0} members")]
	TooManyMembers(u64),
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

pub(crate) struct TarReader<R> {
	inner: R,
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
			if self.members > self.max_members {
				return Err(TarError::TooManyMembers(self.max_members));
			}
			let header = Header::from_byte_slice(&block);
			let size = header
				.entry_size()
				.map_err(|_| TarError::Corrupt("a member's size field is invalid"))?;
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
		let size = pax.size.unwrap_or(stored_size);
		let modified = pax.modified.or_else(|| {
			header.mtime().ok().and_then(|secs| {
				Some(MemberTime {
					secs: i64::try_from(secs).ok()?,
					nanos: 0,
				})
			})
		});
		let entry_type = header.entry_type();
		let mut kind = match entry_type {
			EntryType::Regular | EntryType::Continuous => MemberKind::File,
			EntryType::Directory => MemberKind::Dir,
			EntryType::Symlink => {
				let target = pax
					.link_path
					.or(pending.long_link)
					.or_else(|| header.link_name_bytes().map(|link| link.into_owned()))
					.unwrap_or_default();
				MemberKind::Symlink {
					target: decode_path(&target).0,
				}
			}
			EntryType::Link => MemberKind::Hardlink,
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
		let rest = self.remaining + self.padding;
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
	fn read_header_block(&mut self) -> Result<Option<[u8; BLOCK as usize]>, TarError> {
		let mut block = [0u8; BLOCK as usize];
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
			let mut block = [0u8; BLOCK as usize];
			if read_full(&mut self.inner, &mut block)? != block.len() {
				return Err(TarError::Corrupt("a sparse member's map ends early"));
			}
			// the extended sparse header's `isextended` flag
			extended = block[504] != 0;
		}
		Ok(())
	}
}

/// Reads until `buf` is full or the stream ends; the number of bytes read.
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
	let mut filled = 0;
	while filled < buf.len() {
		match reader.read(&mut buf[filled..]) {
			Ok(0) => break,
			Ok(read) => filled += read,
			Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
			Err(error) => return Err(error),
		}
	}
	Ok(filled)
}

fn padding(size: u64) -> u64 {
	(BLOCK - size % BLOCK) % BLOCK
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
	let nanos: u32 = format!("{:0<9}", &fraction[..fraction.len().min(9)])
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
mod tests {
	use super::*;

	/// A tar header block of `kind` for `name` with `size`, checksummed.
	fn header(name: &[u8], kind: u8, size: u64) -> [u8; 512] {
		let mut block = [0u8; 512];
		block[..name.len()].copy_from_slice(name);
		block[100..108].copy_from_slice(b"0000644\0");
		block[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
		block[136..148].copy_from_slice(format!("{:011o}\0", 1_700_000_000u64).as_bytes());
		block[156] = kind;
		block[257..263].copy_from_slice(b"ustar\0");
		block[263..265].copy_from_slice(b"00");
		block[148..156].fill(b' ');
		let sum: u32 = block.iter().map(|&b| u32::from(b)).sum();
		block[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
		block
	}

	/// `header` with the old GNU magic, as GNU tar writes for its own extensions.
	fn gnu_header(name: &[u8], kind: u8, size: u64) -> [u8; 512] {
		let mut block = header(name, kind, size);
		block[257..265].copy_from_slice(b"ustar  \0");
		block[148..156].fill(b' ');
		let sum: u32 = block.iter().map(|&b| u32::from(b)).sum();
		block[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
		block
	}

	fn padded(data: &[u8]) -> Vec<u8> {
		let mut out = data.to_vec();
		out.resize(data.len().div_ceil(512) * 512, 0);
		out
	}

	fn member(archive: &mut Vec<u8>, name: &[u8], kind: u8, data: &[u8]) {
		archive.extend_from_slice(&header(name, kind, data.len() as u64));
		archive.extend(padded(data));
	}

	fn pax_record(key: &str, value: &str) -> String {
		let body = format!(" {key}={value}\n");
		let mut len = body.len() + 1;
		while format!("{len}{body}").len() != len {
			len += 1;
		}
		format!("{len}{body}")
	}

	fn read_all(reader: &mut TarReader<&[u8]>) -> Vec<(TarMember, Vec<u8>)> {
		let mut out = Vec::new();
		while let Some(member) = reader.next_member().unwrap() {
			let mut data = Vec::new();
			let mut buf = [0u8; 100];
			loop {
				let read = reader.read_body(&mut buf).unwrap();
				if read == 0 {
					break;
				}
				data.extend_from_slice(&buf[..read]);
			}
			out.push((member, data));
		}
		out
	}

	#[test]
	fn members_and_their_data_are_read_in_order() {
		let mut archive = Vec::new();
		member(&mut archive, b"dir/", b'5', b"");
		member(&mut archive, b"dir/a.txt", b'0', b"hello");
		member(&mut archive, b"b.bin", b'0', &[7u8; 1000]);
		archive.extend([0u8; 1024]);
		let mut reader = TarReader::new(archive.as_slice(), 100);
		let members = read_all(&mut reader);
		let summary: Vec<(&str, MemberKind, Vec<u8>)> = members
			.iter()
			.map(|(m, d)| (m.path.as_str(), m.kind.clone(), d.clone()))
			.collect();
		assert_eq!(
			summary,
			[
				("dir/", MemberKind::Dir, Vec::new()),
				("dir/a.txt", MemberKind::File, b"hello".to_vec()),
				("b.bin", MemberKind::File, vec![7u8; 1000]),
			]
		);
		assert_eq!(
			members[1].0.modified,
			Some(MemberTime {
				secs: 1_700_000_000,
				nanos: 0
			})
		);
	}

	#[test]
	fn unread_data_is_skipped() {
		let mut archive = Vec::new();
		member(&mut archive, b"big", b'0', &[1u8; 3000]);
		member(&mut archive, b"next", b'0', b"x");
		let mut reader = TarReader::new(archive.as_slice(), 100);
		assert_eq!(reader.next_member().unwrap().unwrap().path, "big");
		assert_eq!(reader.next_member().unwrap().unwrap().path, "next");
		// a stream that ends at a block boundary without the marker ends the archive
		assert_eq!(reader.next_member().unwrap(), None);
	}

	#[test]
	fn gnu_long_names_and_links_apply_to_the_next_member() {
		let long = "d/".repeat(100) + "file.txt";
		let mut archive = Vec::new();
		member(
			&mut archive,
			b"././@LongLink",
			b'L',
			format!("{long}\0").as_bytes(),
		);
		member(&mut archive, b"truncated", b'0', b"data");
		member(&mut archive, b"././@LongLink", b'K', b"target/of/link\0");
		member(&mut archive, b"link", b'2', b"");
		let members = read_all(&mut TarReader::new(archive.as_slice(), 100));
		assert_eq!(members[0].0.path, long);
		assert_eq!(members[0].1, b"data");
		assert_eq!(
			members[1].0.kind,
			MemberKind::Symlink {
				target: "target/of/link".to_owned()
			}
		);
	}

	#[test]
	fn pax_size_overrides_the_ustar_field() {
		// the ustar field says 0, PAX says 700: the data is read as 700 bytes, so the next header
		// is found where GNU tar and bsdtar find it, and nothing inside the data is parsed as one
		let data = vec![9u8; 700];
		let pax = pax_record("size", "700") + &pax_record("path", "real/name.bin");
		let mut archive = Vec::new();
		member(&mut archive, b"PaxHeader", b'x', pax.as_bytes());
		archive.extend_from_slice(&header(b"shortname", b'0', 0));
		archive.extend(padded(&data));
		member(&mut archive, b"after", b'0', b"z");
		let members = read_all(&mut TarReader::new(archive.as_slice(), 100));
		assert_eq!(members.len(), 2);
		assert_eq!(members[0].0.path, "real/name.bin");
		assert_eq!(members[0].1, data);
		assert_eq!(members[1].0.path, "after");
	}

	#[test]
	fn pax_times_keep_fractions_and_sign() {
		assert_eq!(
			parse_pax_time(b"1700000000.25"),
			Some(MemberTime {
				secs: 1_700_000_000,
				nanos: 250_000_000
			})
		);
		assert_eq!(
			parse_pax_time(b"-10.5"),
			Some(MemberTime {
				secs: -11,
				nanos: 500_000_000
			})
		);
		assert_eq!(
			parse_pax_time(b"-3"),
			Some(MemberTime { secs: -3, nanos: 0 })
		);
		assert_eq!(parse_pax_time(b"x"), None);
	}

	#[test]
	fn gnu_sparse_members_are_classified_as_sparse() {
		let pax = pax_record("GNU.sparse.major", "1") + &pax_record("GNU.sparse.name", "vm.img");
		let mut archive = Vec::new();
		member(&mut archive, b"PaxHeader", b'x', pax.as_bytes());
		member(&mut archive, b"./GNUSparseFile.1/vm.img", b'0', b"map+data");
		archive.extend_from_slice(&gnu_header(b"old-sparse", b'S', 4));
		archive.extend(padded(b"data"));
		let members = read_all(&mut TarReader::new(archive.as_slice(), 100));
		assert_eq!(members[0].0.kind, MemberKind::Sparse);
		assert_eq!(members[1].0.kind, MemberKind::Sparse);
	}

	#[test]
	fn records_are_capped_before_they_are_read() {
		let mut archive = Vec::new();
		archive.extend_from_slice(&header(b"PaxHeader", b'x', MAX_PAX + 1));
		let error = TarReader::new(archive.as_slice(), 100)
			.next_member()
			.unwrap_err();
		assert!(matches!(error, TarError::Corrupt(_)), "{error}");
	}

	#[test]
	fn every_header_counts_toward_the_member_limit() {
		let mut archive = Vec::new();
		for _ in 0..3 {
			member(
				&mut archive,
				b"PaxHeader",
				b'g',
				pax_record("comment", "x").as_bytes(),
			);
		}
		member(&mut archive, b"a", b'0', b"");
		let mut reader = TarReader::new(archive.as_slice(), 3);
		assert!(matches!(
			reader.next_member(),
			Err(TarError::TooManyMembers(3))
		));
	}

	#[test]
	fn corruption_is_reported() {
		let mut archive = Vec::new();
		member(&mut archive, b"a", b'0', b"data");
		let mut bad_checksum = archive.clone();
		bad_checksum[0] = b'b';
		assert!(matches!(
			TarReader::new(bad_checksum.as_slice(), 100).next_member(),
			Err(TarError::Corrupt(_))
		));
		// data that ends early
		let truncated = &archive[..520];
		let mut reader = TarReader::new(truncated, 100);
		reader.next_member().unwrap();
		assert!(matches!(reader.next_member(), Err(TarError::Corrupt(_))));
		// a header cut short
		assert!(matches!(
			TarReader::new(&archive[..511], 100).next_member(),
			Err(TarError::Corrupt(_))
		));
		// a PAX record whose length runs past the header
		let mut archive = Vec::new();
		member(&mut archive, b"PaxHeader", b'x', b"99 path=a\n");
		member(&mut archive, b"a", b'0', b"");
		assert!(matches!(
			TarReader::new(archive.as_slice(), 100).next_member(),
			Err(TarError::Corrupt(_))
		));
	}

	#[test]
	fn non_utf8_paths_decode_as_latin1() {
		let mut archive = Vec::new();
		member(&mut archive, b"caf\xe9.txt", b'0', b"");
		let members = read_all(&mut TarReader::new(archive.as_slice(), 100));
		assert_eq!(members[0].0.path, "café.txt");
		assert!(members[0].0.path_rewritten);
	}
}
