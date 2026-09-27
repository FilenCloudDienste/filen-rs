//! The codec side of extracting an archive (a tar, a compressed tar, one compressed file, or a
//! zip): runs on the codec worker, reads the archive through the driver chunk by chunk, and hands
//! the driver its entries in archive order.

use std::io::{self, Cursor, Read};

use chrono::DateTime;

use crate::{Error, ErrorKind};

use super::{
	super::{
		decode::{CodecError, StreamDecoder, Trailing, codec_error, open_stream},
		entry_path::{PathRejection, entry_path},
		format::{DETECT_HEAD_LEN, Detected, archive_default_name, detect, is_tar_header},
		limits::MAX_ARCHIVE_PATH_BYTES,
		limits::display_path,
		password::ArchivePassword,
		tar_iter::{MemberKind, TarError, TarMember, TarReader},
		worker::{
			ChunkInput, EntryHead, EntryKind, JobEnded, SeekInput, SkippedMember, StreamLayout,
			WorkerEvent, WorkerPort, read_full, send_file_data,
		},
		zip::{
			crypto::CryptoError,
			read::{
				EntryLimits, ZipEncryption, ZipEntry, ZipError, ZipKind, ZipLimits, open_entry,
				read_index,
			},
		},
	},
	DuplicateEntries, ExpansionLimit, ExtractSkipReason,
};

/// Bytes of a tar header block.
const TAR_BLOCK: usize = 512;

/// What the codec may spend on an archive.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CodecLimits {
	/// Memory for the decoder's state.
	pub(crate) decoder_memory: u64,
	/// Most tar headers read, every record counted.
	pub(crate) max_members: u64,
	pub(crate) expansion: Option<ExpansionLimit>,
	/// Most bytes of a zip's central directory read.
	pub(crate) max_index_bytes: u64,
}

/// A streaming archive to extract.
pub(crate) struct StreamJob {
	/// The archive's file name, for the formats told by their extension and for naming the file
	/// a single compressed file decodes to.
	pub(crate) name: String,
	pub(crate) len: u64,
	pub(crate) limits: CodecLimits,
	pub(crate) password: Option<ArchivePassword>,
}

/// How an archive ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchiveEnd {
	/// Bytes that belong to no entry: after the last one (see
	/// [`StreamEnd`](super::super::decode::StreamEnd)), or before a zip's first.
	pub(crate) unaccounted_bytes: u64,
	pub(crate) duplicates: Option<DuplicateEntries>,
}

/// Encrypted zip entries up to this size are read in full to check the password before anything
/// is created; a larger smallest one is checked as it is extracted.
const PASSWORD_PROBE_BYTES: u64 = 16 << 20;

/// Reads a streaming archive through `port`, sending its entries. An error the driver caused
/// (it went away, or a fetch failed) comes back as [`ErrorKind::Cancelled`] or
/// [`ErrorKind::IO`]; the driver knows the real one.
pub(crate) fn extract_stream(port: &WorkerPort, job: StreamJob) -> Result<ArchiveEnd, Error> {
	let mut input = ChunkInput::new(port, 0, job.len);
	let mut head = [0u8; DETECT_HEAD_LEN];
	let head_len = read_full(&mut input, &mut head).map_err(failure)?;
	let head = &head[..head_len];
	let source = Cursor::new(head).chain(input);
	match detect(head, &job.name) {
		Some(Detected::Tar) => {
			port.send(WorkerEvent::Opened(StreamLayout::Tar { codec: None }))
				.map_err(failure)?;
			let rest = walk_tar(port, source, job.limits.max_members)?;
			Ok(ArchiveEnd {
				unaccounted_bytes: drain_trailing(rest).map_err(failure)?,
				duplicates: None,
			})
		}
		Some(Detected::Stream(codec)) => {
			let decoder = open_stream(codec, source, job.limits.decoder_memory)
				.map_err(|e| failure(e.into()))?;
			let mut decoded = Expanding {
				inner: decoder,
				port,
				limit: job.limits.expansion,
				decoded: 0,
			};
			let mut block = [0u8; TAR_BLOCK];
			let block_len = read_full(&mut decoded, &mut block).map_err(failure)?;
			let block = &block[..block_len];
			if block_len == TAR_BLOCK && is_tar_header(block) {
				port.send(WorkerEvent::Opened(StreamLayout::Tar {
					codec: Some(codec),
				}))
				.map_err(failure)?;
				let rest = walk_tar(
					port,
					Cursor::new(block).chain(decoded),
					job.limits.max_members,
				)?;
				let (_, mut decoded) = rest.into_inner();
				// zero blocks after the end-of-archive marker are the usual record padding
				let tar_trailing = drain_trailing(&mut decoded).map_err(failure)?;
				let end = decoded.inner.end().expect("drained to the end");
				Ok(ArchiveEnd {
					unaccounted_bytes: tar_trailing + end.unaccounted_bytes,
					duplicates: None,
				})
			} else {
				port.send(WorkerEvent::Opened(StreamLayout::Single { codec }))
					.map_err(failure)?;
				extract_single(port, &job.name, Cursor::new(block).chain(decoded))
			}
		}
		Some(Detected::Zip) => extract_zip(port, &job),
		Some(Detected::SevenZ) => Err(Error::custom(
			ErrorKind::ArchiveUnsupported,
			"7z archives cannot be extracted yet",
		)),
		None => Err(Error::custom(
			ErrorKind::ArchiveUnsupported,
			"the file is not an archive the SDK can extract",
		)),
	}
}

/// A single compressed file: one entry, named after the archive without its codec extension.
fn extract_single(
	port: &WorkerPort,
	archive_name: &str,
	mut decoded: io::Chain<Cursor<&[u8]>, Expanding<'_, Box<dyn StreamDecoder + '_>>>,
) -> Result<ArchiveEnd, Error> {
	let path = entry_path(archive_default_name(archive_name)).map_err(|_| {
		Error::custom(
			ErrorKind::ArchiveUnsupported,
			"the archive's name cannot be made into a file name",
		)
	})?;
	port.send(WorkerEvent::Entry(EntryHead {
		ordinal: 0,
		path,
		modified: None,
		kind: EntryKind::File { size: None },
	}))
	.map_err(failure)?;
	send_file_data(port, &mut decoded).map_err(failure)?;
	let end = decoded.into_inner().1.inner.end().expect("read to the end");
	port.send(WorkerEvent::FileEnd).map_err(failure)?;
	Ok(ArchiveEnd {
		unaccounted_bytes: end.unaccounted_bytes,
		duplicates: None,
	})
}

/// A zip: its entries in local-header order, each checked against its CRC-32 or authentication
/// code.
fn extract_zip(port: &WorkerPort, job: &StreamJob) -> Result<ArchiveEnd, Error> {
	let mut source = SeekInput::new(port, 0, job.len);
	let limits = ZipLimits {
		max_index_bytes: job.limits.max_index_bytes,
		max_entries: job.limits.max_members,
	};
	let index = read_index(&mut source, job.len, limits).map_err(zip_failure)?;
	let entry_limits = EntryLimits {
		decoder_memory: job.limits.decoder_memory,
	};
	if let Some(limit) = job.limits.expansion {
		// the sizes a zip states are known up front, so a bomb is refused before it is decoded
		let stated: u64 = index.entries.iter().map(|entry| entry.size).sum();
		if stated > limit.floor.max(job.len.saturating_mul(limit.ratio)) {
			return Err(refused(Refused::Expansion(limit.ratio)));
		}
	}
	let password = job.password.as_ref().map(ArchivePassword::as_bytes);
	let encrypted = || {
		index
			.entries
			.iter()
			.filter(|entry| entry.kind == ZipKind::File && entry.encryption != ZipEncryption::None)
	};
	if encrypted().next().is_some() {
		let Some(password) = password else {
			return Err(zip_failure(ZipError::PasswordRequired));
		};
		// a password verifier alone lets a wrong password through now and then; reading the
		// smallest entry in full checks it against the CRC-32 or authentication code too
		if let Some(probe) = encrypted()
			.filter(|entry| zip_supported(entry))
			.min_by_key(|entry| entry.compressed_size)
			&& probe.compressed_size <= PASSWORD_PROBE_BYTES
		{
			let mut reader = open_entry(
				&mut source,
				index.shift,
				probe,
				Some(password),
				entry_limits,
			)
			.map_err(zip_failure)?;
			io::copy(&mut reader, &mut io::sink()).map_err(|error| {
				match zip_io_failure(error) {
					error if error.kind() == ErrorKind::ArchiveCorrupt => Error::custom(
						ErrorKind::ArchiveWrongPassword,
						"the password is likely wrong",
					),
					error => error,
				}
			})?;
		}
	}

	port.send(WorkerEvent::Opened(StreamLayout::Zip))
		.map_err(failure)?;
	for entry in &index.overlapping {
		port.send(zip_skipped(entry, ExtractSkipReason::OverlappingData))
			.map_err(failure)?;
	}
	for entry in &index.entries {
		if entry.kind == ZipKind::Symlink {
			let target =
				zip_symlink_target(&mut source, index.shift, entry, password, entry_limits);
			port.send(zip_skipped(entry, ExtractSkipReason::Symlink { target }))
				.map_err(failure)?;
			continue;
		}
		if entry.kind == ZipKind::File && !zip_supported(entry) {
			port.send(zip_skipped(entry, ExtractSkipReason::UnsupportedMethod))
				.map_err(failure)?;
			continue;
		}
		let is_dir = entry.kind == ZipKind::Dir;
		let mut path = match entry_path(&entry.name) {
			Ok(path) => path,
			Err(PathRejection::Empty) if is_dir => continue,
			Err(rejection) => {
				port.send(zip_skipped(entry, path_skip_reason(rejection)))
					.map_err(failure)?;
				continue;
			}
		};
		path.rewritten |= entry.name_rewritten;
		port.send(WorkerEvent::Entry(EntryHead {
			ordinal: entry.ordinal,
			path,
			modified: entry.modified,
			kind: if is_dir {
				EntryKind::Dir
			} else {
				EntryKind::File {
					size: Some(entry.size),
				}
			},
		}))
		.map_err(failure)?;
		if !is_dir {
			let mut reader = open_entry(&mut source, index.shift, entry, password, entry_limits)
				.map_err(zip_failure)?;
			send_file_data(port, &mut reader).map_err(zip_io_failure)?;
			port.send(WorkerEvent::FileEnd).map_err(failure)?;
		}
	}
	Ok(ArchiveEnd {
		unaccounted_bytes: index.prefix_bytes,
		duplicates: (index.duplicate_count > 0).then(|| DuplicateEntries {
			names: index.duplicate_names.clone(),
			count: index.duplicate_count,
		}),
	})
}

/// Whether the SDK reads the entry's compression method under its encryption.
fn zip_supported(entry: &ZipEntry) -> bool {
	match entry.method {
		0 | 8 | 9 | 12 => true,
		14 | 95 => entry.encryption == ZipEncryption::None,
		_ => false,
	}
}

/// A symlink entry's target, for reporting: its data, when small and readable.
fn zip_symlink_target<R: Read + std::io::Seek>(
	source: &mut R,
	shift: u64,
	entry: &ZipEntry,
	password: Option<&[u8]>,
	limits: EntryLimits,
) -> String {
	if entry.size > MAX_ARCHIVE_PATH_BYTES as u64 || !zip_supported(entry) {
		return String::new();
	}
	let mut target = Vec::new();
	match open_entry(source, shift, entry, password, limits)
		.map(|mut reader| reader.read_to_end(&mut target))
	{
		Ok(Ok(_)) => display_path(&String::from_utf8_lossy(&target)).0.to_owned(),
		_ => String::new(),
	}
}

fn zip_skipped(entry: &ZipEntry, reason: ExtractSkipReason) -> WorkerEvent {
	let (path, path_truncated) = display_path(&entry.name);
	WorkerEvent::Skipped(SkippedMember {
		ordinal: entry.ordinal,
		path: path.to_owned(),
		path_truncated,
		bytes: entry.size,
		reason,
	})
}

fn path_skip_reason(rejection: PathRejection) -> ExtractSkipReason {
	match rejection {
		PathRejection::TooLong => ExtractSkipReason::PathTooLong,
		PathRejection::TooDeep => ExtractSkipReason::PathTooDeep,
		PathRejection::Unsafe | PathRejection::Empty => ExtractSkipReason::UnsafePath,
	}
}

fn zip_failure(error: ZipError) -> Error {
	let kind = match &error {
		ZipError::Corrupt(_) => ErrorKind::ArchiveCorrupt,
		ZipError::Unsupported(_) => ErrorKind::ArchiveUnsupported,
		ZipError::TooLarge(_) => ErrorKind::ArchiveTooLarge,
		ZipError::PasswordRequired => ErrorKind::ArchivePasswordRequired,
		ZipError::WrongPassword => ErrorKind::ArchiveWrongPassword,
		ZipError::Read(_) => {
			let ZipError::Read(error) = error else {
				unreachable!("matched above")
			};
			return zip_io_failure(error);
		}
	};
	Error::custom(kind, error.to_string())
}

/// The error an entry's read ended with: the reader's own, or its source's.
fn zip_io_failure(error: io::Error) -> Error {
	if error
		.get_ref()
		.is_some_and(|inner| inner.is::<ZipError>() || inner.is::<CryptoError>())
	{
		let inner = error.into_inner().expect("checked above");
		return match inner.downcast::<ZipError>() {
			Ok(zip) => zip_failure(*zip),
			// the only crypto error left after opening is a failed authentication code
			Err(crypto) => Error::custom(ErrorKind::ArchiveCorrupt, crypto.to_string()),
		};
	}
	failure(error)
}

/// Sends every member of the tar in `reader`; returns what follows its end-of-archive marker.
fn walk_tar<R: Read>(port: &WorkerPort, reader: R, max_members: u64) -> Result<R, Error> {
	let mut tar = TarReader::new(reader, max_members);
	let mut ordinal = 0;
	while let Some(member) = tar.next_member().map_err(tar_failure)? {
		let this = ordinal;
		ordinal += 1;
		let is_dir = match &member.kind {
			MemberKind::File => false,
			MemberKind::Dir => true,
			MemberKind::Symlink { target } => {
				let reason = ExtractSkipReason::Symlink {
					target: display_path(target).0.to_owned(),
				};
				port.send(skipped(this, &member, reason)).map_err(failure)?;
				continue;
			}
			other => {
				let reason = match other {
					MemberKind::Hardlink => ExtractSkipReason::Hardlink,
					MemberKind::Device | MemberKind::Fifo => ExtractSkipReason::Device,
					MemberKind::Sparse => ExtractSkipReason::Sparse,
					_ => ExtractSkipReason::UnsupportedType,
				};
				port.send(skipped(this, &member, reason)).map_err(failure)?;
				continue;
			}
		};
		let mut path = match entry_path(&member.path) {
			Ok(path) => path,
			// the archive's own root
			Err(PathRejection::Empty) if is_dir => continue,
			Err(rejection) => {
				port.send(skipped(this, &member, path_skip_reason(rejection)))
					.map_err(failure)?;
				continue;
			}
		};
		path.rewritten |= member.path_rewritten;
		let modified = member
			.modified
			.and_then(|time| DateTime::from_timestamp(time.secs, time.nanos));
		let kind = if is_dir {
			EntryKind::Dir
		} else {
			EntryKind::File {
				size: Some(member.size),
			}
		};
		port.send(WorkerEvent::Entry(EntryHead {
			ordinal: this,
			path,
			modified,
			kind,
		}))
		.map_err(failure)?;
		if !is_dir {
			send_file_data(port, &mut TarBody(&mut tar)).map_err(failure)?;
			port.send(WorkerEvent::FileEnd).map_err(failure)?;
		}
	}
	Ok(tar.into_inner())
}

fn skipped(ordinal: u64, member: &TarMember, reason: ExtractSkipReason) -> WorkerEvent {
	let (path, path_truncated) = display_path(&member.path);
	WorkerEvent::Skipped(SkippedMember {
		ordinal,
		path: path.to_owned(),
		path_truncated,
		bytes: member.size,
		reason,
	})
}

/// The current member's data.
struct TarBody<'t, R>(&'t mut TarReader<R>);

impl<R: Read> Read for TarBody<'_, R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		self.0.read_body(buf).map_err(|e| match refusal(e) {
			Ok(refused) => io::Error::new(io::ErrorKind::InvalidData, refused),
			Err(e) => e,
		})
	}
}

/// Decoded bytes, stopped once they exceed the [`ExpansionLimit`].
struct Expanding<'p, D> {
	inner: D,
	port: &'p WorkerPort,
	limit: Option<ExpansionLimit>,
	decoded: u64,
}

impl<D: Read> Read for Expanding<'_, D> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let n = self.inner.read(buf)?;
		self.decoded += n as u64;
		if let Some(limit) = self.limit {
			let allowed = limit
				.floor
				.max(self.port.shared().input_bytes().saturating_mul(limit.ratio));
			if self.decoded > allowed {
				return Err(io::Error::new(
					io::ErrorKind::InvalidData,
					Refused::Expansion(limit.ratio),
				));
			}
		}
		Ok(n)
	}
}

/// Reads `reader` to its end; the bytes from the first non-zero one on (see [`Trailing`]).
fn drain_trailing(mut reader: impl Read) -> io::Result<u64> {
	let mut buf = [0u8; 8192];
	let mut trailing = Trailing::default();
	loop {
		let n = read_full(&mut reader, &mut buf)?;
		if n == 0 {
			return Ok(trailing.unaccounted());
		}
		trailing.push(&buf[..n]);
	}
}

/// Why the codec gave up on an archive, carried inside an [`io::Error`] out of a `Read`.
#[derive(Debug, thiserror::Error)]
enum Refused {
	#[error("the tar archive is damaged: {0}")]
	Corrupt(&'static str),
	#[error("the tar archive has more than {0} members")]
	TooManyMembers(u64),
	#[error("the archive decodes to more than {0} times its size")]
	Expansion(u64),
}

/// What the tar reader refused, or the read error it passed on.
fn refusal(error: TarError) -> Result<Refused, io::Error> {
	match error {
		TarError::Read(error) => Err(error),
		TarError::Corrupt(what) => Ok(Refused::Corrupt(what)),
		TarError::TooManyMembers(max) => Ok(Refused::TooManyMembers(max)),
	}
}

fn tar_failure(error: TarError) -> Error {
	match refusal(error) {
		Ok(refused) => self::refused(refused),
		Err(error) => failure(error),
	}
}

fn refused(refused: Refused) -> Error {
	let kind = match refused {
		Refused::Corrupt(_) => ErrorKind::ArchiveCorrupt,
		Refused::TooManyMembers(_) | Refused::Expansion(_) => ErrorKind::ArchiveTooLarge,
	};
	Error::custom(kind, refused.to_string())
}

/// The error an archive's read ended with, as the job reports it.
fn failure(error: io::Error) -> Error {
	if let Some(codec) = codec_error(&error) {
		let kind = match codec {
			CodecError::Corrupt(_) => ErrorKind::ArchiveCorrupt,
			CodecError::Unsupported(_) => ErrorKind::ArchiveUnsupported,
			CodecError::OverBudget { .. } => ErrorKind::ArchiveTooLarge,
		};
		return Error::custom(kind, codec.to_string());
	}
	let kind = error.kind();
	match error.into_inner() {
		Some(inner) => match inner.downcast::<Refused>() {
			Ok(refusal) => refused(*refusal),
			Err(inner) if inner.is::<JobEnded>() => {
				Error::custom(ErrorKind::Cancelled, "the archive job ended")
			}
			Err(inner) => Error::custom(ErrorKind::IO, inner.to_string()),
		},
		None if kind == io::ErrorKind::UnexpectedEof => {
			Error::custom(ErrorKind::ArchiveCorrupt, "the archive ends early")
		}
		None => Error::custom(ErrorKind::IO, kind.to_string()),
	}
}

#[cfg(test)]
mod tests;
