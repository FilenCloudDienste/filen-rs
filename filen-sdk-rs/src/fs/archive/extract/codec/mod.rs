//! The codec side of extracting an archive (a tar, a compressed tar, one compressed file, a zip or
//! 7z): runs on the codec worker, reads the archive through the driver chunk by chunk, and hands
//! the driver its entries in archive order. A listing runs the same readers, sending what each
//! entry is instead of its data.

mod entries;
mod sevenz;
mod tar;
mod zip;

use std::io::{self, Cursor, Read};

use crate::{Error, ErrorKind};

use super::{
	super::{
		decode::{CodecError, StreamCheck, StreamDecoder, Trailing, codec_error, open_stream},
		entry_path::{ArchivePath, entry_path},
		format::{
			ArchiveFormat, DETECT_HEAD_LEN, Detected, archive_stem, detect, is_end_marker,
			is_tar_header,
		},
		password::ArchivePassword,
		tar_iter::TAR_BLOCK_LEN,
		worker::{
			ChunkInput, EntryHead, EntryKind, JobEnded, SeekInput, SourceFailed, WorkerEvent,
			WorkerPort, read_full, send_file_data,
		},
	},
	DuplicateEntries, ExpansionLimit, ExtractSkipReason,
	list::{ArchiveEntryKind, PasswordCheck},
};
use entries::{Found, Verdict, Walk, apple_double};
pub(crate) use entries::{LinkKeys, Selection, Task};
use sevenz::extract_sevenz;
use tar::walk_tar;
use zip::extract_zip;

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
	/// Storage free for the files, which an archive stating its sizes up front is refused for
	/// before anything is created (see [`ExtractConfig::max_bytes`](super::ExtractConfig)).
	pub(crate) max_bytes: Option<u64>,
}

/// A streaming archive to extract.
pub(crate) struct StreamJob {
	/// The archive's file name, for the formats told by their extension and for naming the file
	/// a single compressed file decodes to.
	pub(crate) name: String,
	pub(crate) len: u64,
	pub(crate) limits: CodecLimits,
	pub(crate) password: Option<ArchivePassword>,
	/// Leaves macOS metadata out (see
	/// [`ExtractConfig::skip_mac_metadata`](super::ExtractConfig::skip_mac_metadata)).
	pub(crate) skip_mac_metadata: bool,
	pub(crate) task: Task,
}

/// How an archive ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchiveEnd {
	/// Bytes that belong to no entry: after the last one (see
	/// [`StreamEnd`](super::super::decode::StreamEnd)), or before a zip's first.
	pub(crate) unaccounted_bytes: u64,
	pub(crate) duplicates: Option<DuplicateEntries>,
	/// Entries extracted whose decoded data nothing in the archive checked: a 7z entry without a
	/// CRC-32, or the files of a stream whose codec carries no checksum (brotli, LZMA-alone,
	/// and lz4, xz or zstd written without one). A bare tar's data is stored rather than
	/// decoded, and is checked by the archive's own hash instead.
	pub(crate) unchecked_entries: u64,
	/// What checking the password up front found.
	pub(crate) password: PasswordCheck,
}

impl ArchiveEnd {
	/// The end of an archive nothing in which needs a password.
	fn plain(unaccounted_bytes: u64, unchecked_entries: u64) -> Self {
		Self {
			unaccounted_bytes,
			duplicates: None,
			unchecked_entries,
			password: PasswordCheck::NotNeeded,
		}
	}
}

/// Encrypted zip entries up to this size are read in full to check the password before anything
/// is created; a larger smallest one is checked as it is extracted.
const PASSWORD_PROBE_BYTES: u64 = 16 << 20;

/// Most a listing decodes of a 7z folder to read a symlink's target, or tell whether a reparse
/// point is a link: a link past it (at the end of a solid block, say) is listed unread. For a
/// zip, most of the archive it fetches for symlink targets, and most it holds of them.
const LIST_READ_BYTES: u64 = 16 << 20;

/// Reads a streaming archive through `port`, sending its entries. An error the driver caused
/// (it went away, or a fetch failed) comes back as [`ErrorKind::Cancelled`] or
/// [`ErrorKind::IO`]; the driver knows the real one.
pub(crate) fn extract_stream(port: &WorkerPort, job: StreamJob) -> Result<ArchiveEnd, Error> {
	let mut walk = Walk::new(port, &job.task, job.skip_mac_metadata);
	let mut input = ChunkInput::new(port, 0, job.len);
	let mut head = [0u8; DETECT_HEAD_LEN];
	let head_len = read_full(&mut input, &mut head).map_err(failure)?;
	let head = &head[..head_len];
	let source = Cursor::new(head).chain(input);
	match detect(head, &job.name) {
		Some(Detected::Tar) => {
			port.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
				.map_err(failure)?;
			let walked = walk_tar(&mut walk, source, job.limits.max_members)?;
			Ok(ArchiveEnd::plain(
				walked.unread + drain_trailing(walked.rest).map_err(failure)?,
				0,
			))
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
			let mut block = [0u8; TAR_BLOCK_LEN];
			let block_len = read_full(&mut decoded, &mut block).map_err(failure)?;
			let block = &block[..block_len];
			// both refuse a block cut short. An empty tar decodes to its end-of-archive marker
			// alone, so only its name tells it from a file of zeros
			let tar = is_tar_header(block)
				|| is_end_marker(block)
					&& matches!(
						ArchiveFormat::of_name(&job.name),
						Some(ArchiveFormat::Tar { codec: Some(_) })
					);
			if tar {
				port.send(WorkerEvent::Opened(ArchiveFormat::Tar {
					codec: Some(codec),
				}))
				.map_err(failure)?;
				let walked = walk_tar(
					&mut walk,
					Cursor::new(block).chain(decoded),
					job.limits.max_members,
				)?;
				let (_, mut decoded) = walked.rest.into_inner();
				// zero blocks after the end-of-archive marker are the usual record padding
				let tar_trailing = drain_trailing(&mut decoded).map_err(failure)?;
				let end = decoded.inner.end().expect("drained to the end");
				Ok(ArchiveEnd::plain(
					walked.unread + tar_trailing + end.unaccounted_bytes,
					unchecked(end.check, walked.files),
				))
			} else {
				port.send(WorkerEvent::Opened(ArchiveFormat::Single { codec }))
					.map_err(failure)?;
				extract_single(&mut walk, &job.name, Cursor::new(block).chain(decoded))
			}
		}
		Some(Detected::Zip) => {
			extract_zip(&mut walk, SeekInput::rereading(source.into_inner().1), &job)
		}
		Some(Detected::SevenZ) => {
			extract_sevenz(&mut walk, SeekInput::rereading(source.into_inner().1), &job)
		}
		None => Err(Error::custom(
			ErrorKind::ArchiveUnsupported,
			"the file is not an archive the SDK can extract",
		)),
	}
}

/// A single compressed file: one entry, named after the archive without its codec extension.
fn extract_single(
	walk: &mut Walk,
	archive_name: &str,
	mut decoded: io::Chain<Cursor<&[u8]>, Expanding<'_, Box<dyn StreamDecoder + '_>>>,
) -> Result<ArchiveEnd, Error> {
	let path = entry_path(archive_stem(archive_name)).map_err(|_| {
		Error::custom(
			ErrorKind::ArchiveUnsupported,
			"the archive's name cannot be made into a file name",
		)
	})?;
	let stored = path.joined();
	let mut found = Found {
		ordinal: 0,
		stored: &stored,
		path: Ok(path),
		kind: ArchiveEntryKind::File,
		unreadable: None,
		size: 0,
		modified: None,
		encrypted: false,
		method: None,
	};
	let files = if walk.listing() {
		// what it decodes to is only known once it is decoded
		found.size = io::copy(&mut decoded, &mut io::sink()).map_err(failure)?;
		walk.list(found, None).map_err(failure)?;
		0
	} else {
		match walk.judge(&found)? {
			Verdict::Take { path, apple_double } => {
				take_file(walk, &found, path, None, apple_double, &mut decoded).map_err(failure)?
			}
			Verdict::Skip(reason) => {
				walk.port.send(found.skipped(reason)).map_err(failure)?;
				0
			}
			Verdict::Ignore | Verdict::Root => 0,
		}
	};
	// read to the end whatever became of the file, for the stream's own checks
	io::copy(&mut decoded, &mut io::sink()).map_err(failure)?;
	walk.finish()?;
	let end = decoded.into_inner().1.inner.end().expect("read to the end");
	Ok(ArchiveEnd::plain(
		end.unaccounted_bytes,
		unchecked(end.check, files),
	))
}

/// Sends the file `found` at `path`, `size` bytes if its archive states it, its data read from
/// `data`, unless its first bytes show it AppleDouble when `apple_double` asks for that to be
/// checked: then it is sent as skipped.
/// Whether it was sent as a file (1) or not (0), to count.
fn take_file(
	walk: &Walk,
	found: &Found,
	path: ArchivePath,
	size: Option<u64>,
	apple_double: bool,
	data: &mut dyn Read,
) -> io::Result<u64> {
	let head = if apple_double {
		let (is_apple_double, head) = self::apple_double(data)?;
		if is_apple_double {
			walk.port
				.send(found.skipped(ExtractSkipReason::MacMetadata))?;
			return Ok(0);
		}
		head
	} else {
		Vec::new()
	};
	walk.port.send(WorkerEvent::Entry(EntryHead {
		ordinal: found.ordinal,
		path,
		modified: found.modified,
		kind: EntryKind::File { size },
	}))?;
	send_file_data(walk.port, &mut Cursor::new(head).chain(data))?;
	walk.port.send(WorkerEvent::FileEnd)?;
	Ok(1)
}

/// Refuses an archive whose entries state more than the [`ExpansionLimit`] lets it decode to:
/// the `sizes` a zip or a 7z states are known before anything is decoded.
fn check_stated_size(job: &StreamJob, sizes: impl IntoIterator<Item = u64>) -> Result<(), Error> {
	let stated = sizes
		.into_iter()
		.fold(0u64, |total, size| total.saturating_add(size));
	match job.limits.expansion {
		Some(limit) if stated > limit.floor.max(job.len.saturating_mul(limit.ratio)) => {
			Err(refused(Refused::Expansion(limit.ratio)))
		}
		_ => Ok(()),
	}
}

/// The `files` a stream decoded to that are unchecked: all of them when its codec verified
/// nothing (see [`StreamCheck`]).
fn unchecked(check: StreamCheck, files: u64) -> u64 {
	match check {
		StreamCheck::Verified => 0,
		StreamCheck::Unverifiable => files,
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
			Err(inner) if inner.is::<SourceFailed>() => {
				Error::custom(ErrorKind::IO, inner.to_string())
			}
			// whatever else a read ended with came from a decoder: the data is damaged
			Err(inner) => Error::custom(ErrorKind::ArchiveCorrupt, inner.to_string()),
		},
		None if kind == io::ErrorKind::UnexpectedEof => {
			Error::custom(ErrorKind::ArchiveCorrupt, "the archive ends early")
		}
		None => Error::custom(ErrorKind::ArchiveCorrupt, kind.to_string()),
	}
}

#[cfg(test)]
mod tests;
