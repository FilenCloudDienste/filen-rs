//! The codec side of extracting a streaming archive (a tar, a compressed tar, or one compressed
//! file): runs on the codec worker, reads the archive through the driver chunk by chunk, and
//! hands the driver its entries in archive order.

use std::io::{self, Cursor, Read};

use chrono::DateTime;

use crate::{Error, ErrorKind};

use super::{
	super::{
		decode::{CodecError, StreamCheck, StreamDecoder, codec_error, open_stream},
		entry_path::{PathRejection, entry_path},
		format::{DETECT_HEAD_LEN, Detected, archive_default_name, detect, is_tar_header},
		limits::display_path,
		tar_iter::{MemberKind, TarError, TarMember, TarReader},
		worker::{
			ChunkInput, EntryHead, EntryKind, Integrity, JobEnded, SkippedMember, StreamLayout,
			WorkerEvent, WorkerPort, read_full, send_file_data,
		},
	},
	ExtractSkipReason,
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
}

/// How much more than it reads a compressed stream may decode to: at most `ratio` times the
/// compressed bytes read so far, but always at least `floor` bytes. Stops decompression bombs
/// before they cost their full output in time and storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExpansionLimit {
	pub(crate) ratio: u64,
	pub(crate) floor: u64,
}

/// A streaming archive to extract.
pub(crate) struct StreamJob {
	/// The archive's file name, for the formats told by their extension and for naming the file
	/// a single compressed file decodes to.
	pub(crate) name: String,
	pub(crate) len: u64,
	pub(crate) limits: CodecLimits,
}

/// How a streaming archive ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArchiveEnd {
	/// Bytes after the last entry that belong to none (see
	/// [`StreamEnd`](super::super::decode::StreamEnd)).
	pub(crate) unaccounted_bytes: u64,
}

/// Reads a streaming archive through `port`, sending its entries. An error the driver caused
/// (it went away, or a fetch failed) comes back as [`ErrorKind::Cancelled`] or
/// [`ErrorKind::IO`]; the driver knows the real one.
pub(crate) fn extract_stream(port: &WorkerPort, job: StreamJob) -> Result<ArchiveEnd, Error> {
	let mut input = ChunkInput::new(port, job.len);
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
				})
			} else {
				port.send(WorkerEvent::Opened(StreamLayout::Single { codec }))
					.map_err(failure)?;
				extract_single(port, &job.name, Cursor::new(block).chain(decoded))
			}
		}
		Some(Detected::Zip | Detected::SevenZ) => Err(Error::custom(
			ErrorKind::ArchiveUnsupported,
			"zip and 7z archives cannot be extracted yet",
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
	let integrity = match end.check {
		StreamCheck::Verified => Integrity::Verified,
		StreamCheck::Unverifiable => Integrity::Unverifiable,
	};
	port.send(WorkerEvent::FileEnd(integrity))
		.map_err(failure)?;
	Ok(ArchiveEnd {
		unaccounted_bytes: end.unaccounted_bytes,
	})
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
				let reason = match rejection {
					PathRejection::TooLong => ExtractSkipReason::PathTooLong,
					PathRejection::TooDeep => ExtractSkipReason::PathTooDeep,
					PathRejection::Unsafe | PathRejection::Empty => ExtractSkipReason::UnsafePath,
				};
				port.send(skipped(this, &member, reason)).map_err(failure)?;
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
			// tar stores no checksum of a member's data
			port.send(WorkerEvent::FileEnd(Integrity::Unverifiable))
				.map_err(failure)?;
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

/// Reads `reader` to its end; how many bytes that was, or 0 when all of them are zero.
fn drain_trailing(mut reader: impl Read) -> io::Result<u64> {
	let mut buf = [0u8; 8192];
	let (mut total, mut non_zero) = (0u64, false);
	loop {
		let n = read_full(&mut reader, &mut buf)?;
		if n == 0 {
			return Ok(if non_zero { total } else { 0 });
		}
		total += n as u64;
		non_zero |= buf[..n].iter().any(|&b| b != 0);
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
