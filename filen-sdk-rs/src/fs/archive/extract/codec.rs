//! The codec side of extracting an archive (a tar, a compressed tar, one compressed file, a zip or
//! 7z): runs on the codec worker, reads the archive through the driver chunk by chunk, and hands
//! the driver its entries in archive order. A listing runs the same readers, sending what each
//! entry is instead of its data.

mod entries;

use std::io::{self, Cursor, Read, Seek};

use chrono::DateTime;

use crate::{Error, ErrorKind, util::SeededMap};

use super::{
	super::{
		decode::{CodecError, StreamCheck, StreamDecoder, Trailing, codec_error, open_stream},
		entry_path::{ArchivePath, entry_path},
		format::{
			ArchiveFormat, DETECT_HEAD_LEN, Detected, archive_stem, detect, is_end_marker,
			is_tar_header,
		},
		limits::MAX_ARCHIVE_PATH_BYTES,
		limits::display_path,
		password::ArchivePassword,
		sevenz::{
			SevenZError, from_source,
			read::{
				FOLDER_ENDS_EARLY, FolderCursor, Keys, SevenZEntry, SevenZIndex, SevenZKind,
				SevenZLimits, read_error, read_index as read_sevenz_index, windows_link_target,
				wrong_key,
			},
		},
		tar_iter::{MemberKind, TAR_BLOCK, TarError, TarReader},
		worker::{
			ChunkInput, EntryHead, EntryKind, JobEnded, LinkHead, SeekInput, SkippedMember,
			SourceFailed, WorkerEvent, WorkerPort, read_full, send_file_data,
		},
		zip::{
			METHOD_BZIP2, METHOD_DEFLATE, METHOD_DEFLATE64, METHOD_LZMA, METHOD_PPMD,
			METHOD_STORED, METHOD_XZ, METHOD_ZSTD,
			crypto::{AES_AUTH_CODE_LEN, AES_VERIFIER_LEN, CryptoError, ZIP_CRYPTO_HEADER_LEN},
			method_supported,
			read::{
				EntryLimits, ZipEncryption, ZipEntry, ZipError, ZipIndex, ZipKind, ZipLimits,
				open_entry, read_index, unaccounted_after,
			},
		},
	},
	DuplicateEntries, ExpansionLimit, ExtractSkipReason,
	list::{ArchiveEntryKind, PasswordCheck},
	storage_exceeded,
};
use entries::{Found, MacShape, Verdict, Walk, apple_double};
pub(crate) use entries::{Selection, Task, link_key};

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
			let mut block = [0u8; TAR_BLOCK as usize];
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

/// The `files` a stream decoded to that are unchecked: all of them when its codec verified
/// nothing (see [`StreamCheck`]).
fn unchecked(check: StreamCheck, files: u64) -> u64 {
	match check {
		StreamCheck::Verified => 0,
		StreamCheck::Unverifiable => files,
	}
}

/// How a zip entry's data is compressed, for display.
fn zip_method(entry: &ZipEntry) -> Option<String> {
	if entry.kind == ZipKind::Dir {
		return None;
	}
	Some(match entry.method {
		METHOD_STORED => "Stored".to_owned(),
		METHOD_DEFLATE => "Deflate".to_owned(),
		METHOD_DEFLATE64 => "Deflate64".to_owned(),
		METHOD_BZIP2 => "BZip2".to_owned(),
		METHOD_LZMA => "LZMA".to_owned(),
		METHOD_ZSTD => "Zstd".to_owned(),
		METHOD_XZ => "XZ".to_owned(),
		METHOD_PPMD => "PPMd".to_owned(),
		other => format!("method {other}"),
	})
}

/// What a zip entry is, before anything is read of it but its record. A symlink's target is
/// only read for a listing, which reports it with the entry's kind; an extraction reads it when
/// it reports the link skipped.
fn zip_found<'e>(entry: &'e ZipEntry, overlapping: bool, target: String) -> Found<'e> {
	let (kind, unreadable) = match entry.kind {
		ZipKind::Symlink => (
			ArchiveEntryKind::Symlink {
				target: target.clone(),
			},
			Some(ExtractSkipReason::Symlink { target }),
		),
		ZipKind::Dir => (ArchiveEntryKind::Dir, None),
		ZipKind::File => (
			ArchiveEntryKind::File,
			(!zip_supported(entry)).then_some(ExtractSkipReason::UnsupportedMethod),
		),
	};
	Found {
		ordinal: entry.ordinal,
		stored: &entry.name,
		path: entry_path(&entry.name).map(|mut path| {
			path.rewritten |= entry.name_rewritten;
			path
		}),
		kind,
		unreadable: if overlapping {
			Some(ExtractSkipReason::OverlappingData)
		} else {
			unreadable
		},
		size: entry.size,
		modified: entry.modified,
		encrypted: entry.encryption != ZipEncryption::None,
		method: zip_method(entry),
	}
}

/// What checking the password up front finds for a zip: whether it needs one, and whether
/// `password` opens the encrypted entry quickest to read in full, when one is small enough.
fn check_zip_password<R: Read + Seek>(
	source: &mut R,
	index: &ZipIndex,
	password: Option<&[u8]>,
	limits: EntryLimits,
) -> Result<PasswordCheck, Error> {
	let mut encrypted = index
		.entries
		.iter()
		.filter(|entry| entry.kind == ZipKind::File && entry.encryption != ZipEncryption::None)
		.peekable();
	if encrypted.peek().is_none() {
		return Ok(PasswordCheck::NotNeeded);
	}
	let Some(password) = password else {
		return Ok(PasswordCheck::Required);
	};
	// a password verifier alone lets a wrong password through now and then; reading the smallest
	// entry in full checks it against the CRC-32 or authentication code too. An empty entry
	// proves little (ZipCrypto's check byte lets 1 in 256 wrong passwords through, and its CRC
	// matches whatever the key), so one with data goes first
	let Some(probe) = encrypted
		.filter(|entry| zip_supported(entry))
		.min_by_key(|entry| (entry.size == 0, entry.compressed_size))
		.filter(|probe| probe.compressed_size <= PASSWORD_PROBE_BYTES)
	else {
		return Ok(PasswordCheck::Unchecked);
	};
	match open_entry(source, index.shift, probe, Some(password), limits) {
		Ok(mut reader) => match io::copy(&mut reader, &mut io::sink()).map_err(zip_io_failure) {
			Ok(_) => Ok(PasswordCheck::Right),
			// damage in ZipCrypto data is likelier a wrong password than a damaged archive
			Err(error) if key_unproven(probe) && error.kind() == ErrorKind::ArchiveCorrupt => {
				Ok(PasswordCheck::Wrong)
			}
			Err(error) if error.kind() == ErrorKind::ArchiveWrongPassword => {
				Ok(PasswordCheck::Wrong)
			}
			Err(error) => Err(error),
		},
		// skipped when its turn comes; the password is checked on the entries read
		Err(ZipError::Overlapping) => Ok(PasswordCheck::Unchecked),
		Err(ZipError::WrongPassword) => Ok(PasswordCheck::Wrong),
		Err(error) => Err(zip_failure(error)),
	}
}

/// A zip read from `source`: its entries in local-header order, each checked against its CRC-32
/// or authentication code.
fn extract_zip(
	walk: &mut Walk,
	mut source: SeekInput<'_>,
	job: &StreamJob,
) -> Result<ArchiveEnd, Error> {
	let limits = ZipLimits {
		max_index_bytes: job.limits.max_index_bytes,
		max_entries: job.limits.max_members,
	};
	let index = read_index(&mut source, job.len, limits).map_err(zip_failure)?;
	let entry_limits = EntryLimits {
		decoder_memory: job.limits.decoder_memory,
	};
	let password = job.password.as_ref().map(ArchivePassword::as_bytes);
	let duplicates = (index.duplicate_count > 0).then(|| DuplicateEntries {
		names: index.duplicate_names.clone(),
		count: index.duplicate_count,
	});
	if walk.listing() {
		let checked = check_zip_password(&mut source, &index, password, entry_limits)?;
		walk.port
			.send(WorkerEvent::Opened(ArchiveFormat::Zip))
			.map_err(failure)?;
		let mut targets =
			zip_symlink_targets(walk.port, &mut source, &index, password, entry_limits);
		let mut listed: Vec<(&ZipEntry, bool)> = index
			.entries
			.iter()
			.map(|entry| (entry, false))
			.chain(index.overlapping.iter().map(|entry| (entry, true)))
			.collect();
		listed.sort_unstable_by_key(|(entry, _)| entry.ordinal);
		for (entry, overlapping) in listed {
			let target = targets
				.binary_search_by_key(&entry.ordinal, |(ordinal, _)| *ordinal)
				.map(|at| std::mem::take(&mut targets[at].1))
				.unwrap_or_default();
			walk.list(zip_found(entry, overlapping, target), None)
				.map_err(failure)?;
		}
		// what the index shows: the bytes around and between entries would take reading every
		// entry's local header, the whole archive
		let unaccounted_bytes = index
			.prefix_bytes
			.saturating_add(index.directory_slack)
			.saturating_add(index.trailing_bytes);
		return Ok(ArchiveEnd {
			unaccounted_bytes,
			duplicates,
			unchecked_entries: 0,
			password: checked,
		});
	}
	walk.check_selection(index.entries.iter().chain(&index.overlapping).map(|entry| {
		(
			entry.ordinal,
			entry.name.as_str(),
			entry.kind == ZipKind::Dir,
		)
	}))?;
	if let Some(limit) = job.limits.expansion {
		// the sizes a zip states are known up front, so a bomb is refused before it is decoded
		let stated = index
			.entries
			.iter()
			.fold(0u64, |total, entry| total.saturating_add(entry.size));
		if stated > limit.floor.max(job.len.saturating_mul(limit.ratio)) {
			return Err(refused(Refused::Expansion(limit.ratio)));
		}
	}
	let extracted = walk.extracted_bytes(
		index
			.entries
			.iter()
			.filter(|entry| entry.kind == ZipKind::File && zip_supported(entry))
			.map(|entry| (entry.ordinal, entry.name.as_str(), entry.size)),
	);
	if let Some(error) = storage_exceeded(job.limits.max_bytes, extracted) {
		return Err(error);
	}
	// set once an encrypted entry read back whole against its CRC-32 or authentication code
	let mut verified = match check_zip_password(&mut source, &index, password, entry_limits)? {
		PasswordCheck::NotNeeded | PasswordCheck::Right => true,
		PasswordCheck::Unchecked => false,
		PasswordCheck::Required => return Err(zip_failure(ZipError::PasswordRequired)),
		PasswordCheck::Wrong => return Err(zip_failure(ZipError::WrongPassword)),
	};

	walk.port
		.send(WorkerEvent::Opened(ArchiveFormat::Zip))
		.map_err(failure)?;
	for entry in &index.overlapping {
		let found = zip_found(entry, true, String::new());
		if let Verdict::Skip(reason) = walk.judge(&found)? {
			walk.port.send(found.skipped(reason)).map_err(failure)?;
		}
	}
	let mut unaccounted_bytes = index
		.prefix_bytes
		.saturating_add(index.directory_slack)
		.saturating_add(index.trailing_bytes);
	for entry in &index.entries {
		let found = zip_found(entry, false, String::new());
		let verdict = walk.judge(&found)?;
		// what a partial extraction leaves out is not read at all, its local header included: a
		// zip extracted in part reports the bytes around what it read only
		if matches!(verdict, Verdict::Ignore) {
			continue;
		}
		unaccounted_bytes =
			unaccounted_bytes.saturating_add(unaccounted_after(&mut source, index.shift, entry));
		if entry.kind == ZipKind::Dir
			&& entry.compressed_size > 0
			&& !decodes_to_nothing(&mut source, index.shift, entry, password, entry_limits)
		{
			// a directory holds no data: anything stored under one is extracted nowhere
			unaccounted_bytes = unaccounted_bytes.saturating_add(entry.compressed_size);
		}
		let (path, apple_double) = match verdict {
			Verdict::Ignore | Verdict::Root => continue,
			Verdict::Skip(ExtractSkipReason::Symlink { .. }) => {
				let target =
					zip_symlink_target(&mut source, index.shift, entry, password, entry_limits);
				walk.port
					.send(found.skipped(ExtractSkipReason::Symlink { target }))
					.map_err(failure)?;
				continue;
			}
			Verdict::Skip(reason) => {
				walk.port.send(found.skipped(reason)).map_err(failure)?;
				continue;
			}
			Verdict::Take { path, apple_double } => (path, apple_double),
		};
		if entry.kind == ZipKind::Dir {
			walk.port
				.send(WorkerEvent::Entry(EntryHead {
					ordinal: entry.ordinal,
					path,
					modified: entry.modified,
					kind: EntryKind::Dir,
				}))
				.map_err(failure)?;
			continue;
		}
		// opened before it is announced: its local header may show it overlapping the next
		let mut reader = match open_entry(&mut source, index.shift, entry, password, entry_limits) {
			Ok(reader) => reader,
			Err(ZipError::Overlapping) => {
				walk.port
					.send(found.skipped(ExtractSkipReason::OverlappingData))
					.map_err(failure)?;
				continue;
			}
			Err(error) => return Err(zip_failure(error)),
		};
		let encrypted = entry.encryption != ZipEncryption::None;
		match take_file(
			walk,
			&found,
			path,
			Some(entry.size),
			apple_double,
			&mut reader,
		)
		.map_err(zip_io_failure)
		{
			// an AppleDouble file left out was not read to its end: it proves nothing
			Ok(0) => {}
			// an empty ZipCrypto entry matches its CRC-32 under any key; AES's authentication
			// code rejects a wrong one even over nothing
			Ok(_) => {
				verified |= encrypted
					&& (entry.size > 0 || matches!(entry.encryption, ZipEncryption::Aes { .. }))
			}
			// while no entry proved the password, damage in ZipCrypto data is likelier a wrong
			// password than a damaged archive (its check byte passes 1 wrong one in 256; AES's
			// verifier, which already passed, 1 in 65536)
			Err(error)
				if key_unproven(entry)
					&& !verified && error.kind() == ErrorKind::ArchiveCorrupt =>
			{
				return Err(Error::custom(
					ErrorKind::ArchiveWrongPassword,
					"the password is likely wrong",
				));
			}
			Err(error) => return Err(error),
		}
	}
	Ok(ArchiveEnd {
		unaccounted_bytes,
		duplicates,
		unchecked_entries: 0,
		password: PasswordCheck::NotNeeded,
	})
}

/// How a 7z entry's data is compressed, for display: its folder's coders, outermost first.
fn sevenz_method(index: &SevenZIndex, entry: &SevenZEntry) -> Option<String> {
	let stream = entry.stream?;
	Some(
		index.folders[stream.folder]
			.coders
			.iter()
			.map(|coder| coder.method.map_or("unknown", |method| method.name()))
			.collect::<Vec<&str>>()
			.join("+"),
	)
}

/// What checking the password up front finds for a 7z: an encrypted header only decodes with the
/// right password; encrypted data is checked on the entry cheapest to reach that has a CRC-32,
/// when that takes at most [`PASSWORD_PROBE_BYTES`]. An empty entry proves nothing: decoding
/// nothing matches its CRC-32 under any key.
fn check_sevenz_password<'s, R: Read + Seek + 's>(
	cursor: &mut FolderCursor<'s, R>,
	index: &SevenZIndex,
	keys: &mut Keys<'_>,
	has_password: bool,
) -> Result<PasswordCheck, Error> {
	let encrypted = |entry: &SevenZEntry| {
		entry
			.stream
			.is_some_and(|stream| index.folders[stream.folder].encrypted())
	};
	if index.headers_encrypted {
		return Ok(PasswordCheck::Right);
	}
	if !index.entries.iter().any(encrypted) {
		return Ok(PasswordCheck::NotNeeded);
	}
	if !has_password {
		return Ok(PasswordCheck::Required);
	}
	let Some(probe) = index
		.entries
		.iter()
		.filter(|entry| {
			encrypted(entry)
				&& entry.size > 0
				&& entry.crc.is_some()
				&& index.folders[entry.stream.expect("encrypted").folder].supported()
		})
		.min_by_key(|entry| entry.stream.expect("encrypted").offset + entry.size)
		.filter(|entry| {
			entry.stream.expect("encrypted").offset + entry.size <= PASSWORD_PROBE_BYTES
		})
	else {
		return Ok(PasswordCheck::Unchecked);
	};
	// setting the folder up fails for the archive's reasons; what decodes wrong under the key
	// (skipping to the entry, or the entry itself) is the key's
	let probed = cursor
		.open(index, probe, keys)
		.map_err(|error| match error {
			SevenZError::Read(error) if !from_source(&error) => SevenZError::WrongPassword,
			SevenZError::Corrupt(FOLDER_ENDS_EARLY) => SevenZError::WrongPassword,
			error => error,
		})
		.and_then(|mut data| {
			io::copy(&mut data, &mut io::sink()).map_err(|error| wrong_key(read_error(error)))
		});
	match probed {
		Ok(_) => Ok(PasswordCheck::Right),
		Err(SevenZError::WrongPassword) => Ok(PasswordCheck::Wrong),
		Err(error) => Err(sevenz_failure(error)),
	}
}

/// What a 7z entry is. A symlink's target, and a reparse point's data (which says whether it is
/// a link), are read from the archive; a reparse point that is a file's data comes back with it.
fn sevenz_found<'e, 's, R: Read + Seek + 's>(
	cursor: &mut FolderCursor<'s, R>,
	index: &SevenZIndex,
	entry: &'e SevenZEntry,
	keys: &mut Keys<'_>,
) -> Result<(Found<'e>, Option<Vec<u8>>), SevenZError> {
	let supported = entry
		.stream
		.is_none_or(|stream| index.folders[stream.folder].supported());
	let mut held = None;
	let (kind, unreadable) = match entry.kind {
		SevenZKind::Anti => (ArchiveEntryKind::Other, Some(ExtractSkipReason::AntiItem)),
		SevenZKind::Symlink => {
			let target = sevenz_symlink_target(cursor, index, entry, keys);
			(
				ArchiveEntryKind::Symlink {
					target: target.clone(),
				},
				Some(ExtractSkipReason::Symlink { target }),
			)
		}
		SevenZKind::Dir => (ArchiveEntryKind::Dir, None),
		_ if !supported => (
			ArchiveEntryKind::File,
			Some(ExtractSkipReason::UnsupportedMethod),
		),
		SevenZKind::Reparse => {
			let mut data = Vec::new();
			cursor
				.open(index, entry, keys)?
				.read_to_end(&mut data)
				.map_err(read_error)?;
			match windows_link_target(&data) {
				Some(target) => {
					let target = display_path(&target).0.to_owned();
					(
						ArchiveEntryKind::Symlink {
							target: target.clone(),
						},
						Some(ExtractSkipReason::Symlink { target }),
					)
				}
				None => {
					held = Some(data);
					(ArchiveEntryKind::File, None)
				}
			}
		}
		SevenZKind::File => (ArchiveEntryKind::File, None),
	};
	let found = Found {
		ordinal: entry.ordinal,
		stored: &entry.name,
		path: entry_path(&entry.name).map(|mut path| {
			path.rewritten |= entry.name_rewritten;
			path
		}),
		kind,
		unreadable,
		size: entry.size,
		modified: entry.modified,
		encrypted: entry
			.stream
			.is_some_and(|stream| index.folders[stream.folder].encrypted()),
		method: sevenz_method(index, entry),
	};
	Ok((found, held))
}

/// A 7z read from `source`: its entries in header order, folder by folder, each checked against
/// its CRC-32 when the header lists one.
fn extract_sevenz(
	walk: &mut Walk,
	mut source: SeekInput<'_>,
	job: &StreamJob,
) -> Result<ArchiveEnd, Error> {
	let port = walk.port;
	let password = job.password.as_ref().map(ArchivePassword::utf16le);
	// a derivation exchanges nothing with the driver for up to a minute: without this it would
	// be given up on as a dead codec, and a cancel would wait it out
	let keep_alive = || port.keep_alive();
	let mut keys = Keys::new(password.as_ref().map(|password| &password[..])).on_round(&keep_alive);
	let limits = SevenZLimits {
		max_index_bytes: job.limits.max_index_bytes,
		max_entries: job.limits.max_members,
		decoder_memory: job.limits.decoder_memory,
	};
	let index =
		read_sevenz_index(&mut source, job.len, limits, &mut keys).map_err(sevenz_failure)?;
	// a folder's packed streams are read in turns (BCJ2 has four)
	// (at most 5 chunks: a folder of several BCJ2 coders refetches rather than hold more)
	source.set_slots((index.max_packed_streams() + 1).min(5));
	let mut cursor = FolderCursor::new(source, limits.decoder_memory);
	if walk.listing() {
		let checked = check_sevenz_password(&mut cursor, &index, &mut keys, password.is_some())?;
		port.send(WorkerEvent::Opened(ArchiveFormat::SevenZ))
			.map_err(failure)?;
		// a listing reads the index: a link's data is read only where little has to be decoded
		// to reach it, and never when the archive states more than it may decode to
		let stated = index
			.entries
			.iter()
			.fold(0u64, |total, entry| total.saturating_add(entry.size));
		let within_limit = job
			.limits
			.expansion
			.is_none_or(|limit| stated <= limit.floor.max(job.len.saturating_mul(limit.ratio)));
		for entry in &index.entries {
			let cheap = entry
				.stream
				.is_some_and(|stream| stream.offset.saturating_add(entry.size) <= LIST_READ_BYTES);
			if !(within_limit && cheap) {
				walk.list(sevenz_unread(&index, entry), None)
					.map_err(failure)?;
				continue;
			}
			// a link's data unread for a wrong password leaves it listed as a link without its
			// target
			let found = match sevenz_found(&mut cursor, &index, entry, &mut keys) {
				Ok((found, _)) => found,
				Err(_) if checked == PasswordCheck::Wrong || password.is_none() => {
					sevenz_unread(&index, entry)
				}
				Err(error) => return Err(sevenz_failure(error)),
			};
			walk.list(found, None).map_err(failure)?;
		}
		return Ok(ArchiveEnd {
			unaccounted_bytes: index.unaccounted_bytes,
			duplicates: None,
			unchecked_entries: 0,
			password: checked,
		});
	}
	walk.check_selection(index.entries.iter().map(|entry| {
		(
			entry.ordinal,
			entry.name.as_str(),
			entry.kind == SevenZKind::Dir,
		)
	}))?;
	if let Some(limit) = job.limits.expansion {
		let stated = index
			.entries
			.iter()
			.fold(0u64, |total, entry| total.saturating_add(entry.size));
		if stated > limit.floor.max(job.len.saturating_mul(limit.ratio)) {
			return Err(refused(Refused::Expansion(limit.ratio)));
		}
	}
	let extracted = walk.extracted_bytes(
		index
			.entries
			.iter()
			.filter(|entry| {
				entry.kind == SevenZKind::File
					&& entry
						.stream
						.is_none_or(|stream| index.folders[stream.folder].supported())
			})
			.map(|entry| (entry.ordinal, entry.name.as_str(), entry.size)),
	);
	if let Some(error) = storage_exceeded(job.limits.max_bytes, extracted) {
		return Err(error);
	}
	let encrypted = |entry: &SevenZEntry| {
		entry
			.stream
			.is_some_and(|stream| index.folders[stream.folder].encrypted())
	};
	let mut verified =
		match check_sevenz_password(&mut cursor, &index, &mut keys, password.is_some())? {
			PasswordCheck::NotNeeded | PasswordCheck::Right => true,
			PasswordCheck::Unchecked => false,
			PasswordCheck::Required => return Err(sevenz_failure(SevenZError::PasswordRequired)),
			PasswordCheck::Wrong => return Err(sevenz_failure(SevenZError::WrongPassword)),
		};

	port.send(WorkerEvent::Opened(ArchiveFormat::SevenZ))
		.map_err(failure)?;
	let mut unchecked_entries = 0;
	// while the password is unchecked, damage in encrypted data is likelier a wrong password
	// than a damaged archive
	let judged = |error: Error, entry: &SevenZEntry, verified: bool| {
		if !verified && encrypted(entry) && error.kind() == ErrorKind::ArchiveCorrupt {
			Error::custom(
				ErrorKind::ArchiveWrongPassword,
				"the password is likely wrong",
			)
		} else {
			error
		}
	};
	// whether reading an entry whole against its CRC-32 proved the password
	let proves = |entry: &SevenZEntry| entry.crc.is_some() && encrypted(entry) && entry.size > 0;
	for entry in &index.entries {
		// what is left out is not read: its name decides whether it is chosen
		let verdict = walk.judge(&sevenz_unread(&index, entry))?;
		if matches!(verdict, Verdict::Ignore) {
			continue;
		}
		// a reparse point's data says whether it is a link, so it is read before anything is
		// sent, and sent from here when it is a file's
		let (found, held) = sevenz_found(&mut cursor, &index, entry, &mut keys)
			.map_err(|error| judged(sevenz_failure(error), entry, verified))?;
		if entry.kind == SevenZKind::Reparse {
			verified |= proves(entry);
		}
		let (path, apple_double) = match walk.judge_again(&found, verdict) {
			Verdict::Ignore | Verdict::Root => continue,
			Verdict::Skip(reason) => {
				port.send(found.skipped(reason)).map_err(failure)?;
				continue;
			}
			Verdict::Take { path, apple_double } => (path, apple_double),
		};
		if entry.kind == SevenZKind::Dir {
			port.send(WorkerEvent::Entry(EntryHead {
				ordinal: entry.ordinal,
				path,
				modified: entry.modified,
				kind: EntryKind::Dir,
			}))
			.map_err(failure)?;
			continue;
		}
		let sent = match (entry.stream, held) {
			(Some(_), Some(data)) => take_file(
				walk,
				&found,
				path,
				Some(entry.size),
				apple_double,
				&mut data.as_slice(),
			)
			.map_err(failure),
			(Some(_), None) => cursor
				.open(&index, entry, &mut keys)
				.map_err(sevenz_failure)
				.and_then(|mut data| {
					take_file(
						walk,
						&found,
						path,
						Some(entry.size),
						apple_double,
						&mut data,
					)
					.map_err(sevenz_io_failure)
				}),
			(None, _) => {
				take_file(walk, &found, path, Some(0), false, &mut io::empty()).map_err(failure)
			}
		};
		let sent = sent.map_err(|error| judged(error, entry, verified))?;
		if sent > 0 && entry.stream.is_some() {
			verified |= proves(entry);
			if entry.crc.is_none() {
				unchecked_entries += 1;
			}
		}
	}
	Ok(ArchiveEnd {
		unaccounted_bytes: index.unaccounted_bytes,
		duplicates: None,
		unchecked_entries,
		password: PasswordCheck::NotNeeded,
	})
}

/// What a 7z entry is by its header alone: before its data is read, or when it cannot be (a
/// link's under a wrong password).
fn sevenz_unread<'e>(index: &SevenZIndex, entry: &'e SevenZEntry) -> Found<'e> {
	let supported = entry
		.stream
		.is_none_or(|stream| index.folders[stream.folder].supported());
	let (kind, unreadable) = match entry.kind {
		SevenZKind::Anti => (ArchiveEntryKind::Other, Some(ExtractSkipReason::AntiItem)),
		SevenZKind::Symlink => (
			ArchiveEntryKind::Symlink {
				target: String::new(),
			},
			Some(ExtractSkipReason::Symlink {
				target: String::new(),
			}),
		),
		SevenZKind::Dir => (ArchiveEntryKind::Dir, None),
		_ if !supported => (
			ArchiveEntryKind::File,
			Some(ExtractSkipReason::UnsupportedMethod),
		),
		SevenZKind::Reparse | SevenZKind::File => (ArchiveEntryKind::File, None),
	};
	Found {
		ordinal: entry.ordinal,
		stored: &entry.name,
		path: entry_path(&entry.name).map(|mut path| {
			path.rewritten |= entry.name_rewritten;
			path
		}),
		kind,
		unreadable,
		size: entry.size,
		modified: entry.modified,
		encrypted: entry
			.stream
			.is_some_and(|stream| index.folders[stream.folder].encrypted()),
		method: sevenz_method(index, entry),
	}
}

/// A symlink entry's target, for reporting: its data, when small and readable.
fn sevenz_symlink_target<'s, R: Read + std::io::Seek + 's>(
	cursor: &mut FolderCursor<'s, R>,
	index: &SevenZIndex,
	entry: &SevenZEntry,
	keys: &mut Keys<'_>,
) -> String {
	let readable = entry
		.stream
		.is_some_and(|stream| index.folders[stream.folder].supported());
	if !readable || entry.size > MAX_ARCHIVE_PATH_BYTES as u64 {
		return String::new();
	}
	let mut target = Vec::new();
	match cursor
		.open(index, entry, keys)
		.map(|mut data| data.read_to_end(&mut target))
	{
		Ok(Ok(_)) => display_path(&String::from_utf8_lossy(&target)).0.to_owned(),
		_ => String::new(),
	}
}

fn sevenz_failure(error: SevenZError) -> Error {
	let kind = match &error {
		SevenZError::Corrupt(_) => ErrorKind::ArchiveCorrupt,
		SevenZError::Unsupported(_) => ErrorKind::ArchiveUnsupported,
		SevenZError::TooLarge(_) => ErrorKind::ArchiveTooLarge,
		SevenZError::PasswordRequired => ErrorKind::ArchivePasswordRequired,
		SevenZError::WrongPassword => ErrorKind::ArchiveWrongPassword,
		SevenZError::Read(_) => {
			let SevenZError::Read(error) = error else {
				unreachable!("matched above")
			};
			return failure(error);
		}
	};
	Error::custom(kind, error.to_string())
}

/// The error an entry's read ended with: its checks', or its source's.
fn sevenz_io_failure(error: io::Error) -> Error {
	if error
		.get_ref()
		.is_some_and(|inner| inner.is::<SevenZError>())
	{
		return sevenz_failure(read_error(error));
	}
	failure(error)
}

/// Whether an entry that opened may still be under a wrong key: ZipCrypto's check byte lets 1
/// wrong password in 256 through, where the AES verifier that let it open passes 1 in 65536.
fn key_unproven(entry: &ZipEntry) -> bool {
	matches!(entry.encryption, ZipEncryption::ZipCrypto { .. })
}

/// Whether the SDK reads the entry's compression method under its encryption.
fn zip_supported(entry: &ZipEntry) -> bool {
	method_supported(entry.method, entry.encryption != ZipEncryption::None)
}

/// Whether a directory entry's stored bytes are an empty stream (as `java.util.zip` and Python
/// deflate directories: two bytes), checked against its size and CRC-32 to the end.
fn decodes_to_nothing<R: Read + std::io::Seek>(
	source: &mut R,
	shift: u64,
	entry: &ZipEntry,
	password: Option<&[u8]>,
	limits: EntryLimits,
) -> bool {
	if entry.size != 0 || !zip_supported(entry) {
		return false;
	}
	// an encrypted one is judged by its length: decrypting each would cost a key derivation per
	// directory, for directories that may never be created
	let overhead = match entry.encryption {
		ZipEncryption::None => {
			return open_entry(source, shift, entry, password, limits)
				.and_then(|mut data| io::copy(&mut data, &mut io::sink()).map_err(ZipError::Read))
				.is_ok_and(|read| read == 0);
		}
		ZipEncryption::ZipCrypto { .. } => ZIP_CRYPTO_HEADER_LEN,
		ZipEncryption::Aes { strength, .. } => {
			strength.salt_len() as u64 + AES_VERIFIER_LEN + AES_AUTH_CODE_LEN
		}
	};
	let data = entry.compressed_size.checked_sub(overhead);
	match entry.method {
		METHOD_STORED => data == Some(0),
		// an empty deflate (or deflate64) stream takes 2 bytes, too few for any literal and its
		// block's end
		METHOD_DEFLATE | METHOD_DEFLATE64 => data.is_some_and(|data| data <= 2),
		// an empty bzip2 stream is its 4-byte header and 10-byte end: no room for a block
		METHOD_BZIP2 => data.is_some_and(|data| data <= 14),
		_ => false,
	}
}

/// The targets of a zip's symlinks, for listing, by ordinal. They are read in the order their
/// data is stored, so the archive is read front to back once, however its index orders them,
/// rather than a chunk fetched again for each; and only until reading them has fetched
/// [`LIST_READ_BYTES`] of the archive or they hold that much: the links past it are listed
/// with their targets unread. An overlapping entry's data is never read.
fn zip_symlink_targets(
	port: &WorkerPort,
	source: &mut SeekInput<'_>,
	index: &ZipIndex,
	password: Option<&[u8]>,
	limits: EntryLimits,
) -> Vec<(u64, String)> {
	let first_read = port.shared().input_bytes();
	let mut held = 0;
	let mut targets = Vec::new();
	for entry in index
		.entries
		.iter()
		.filter(|entry| entry.kind == ZipKind::Symlink)
	{
		if port.shared().input_bytes() - first_read > LIST_READ_BYTES || held > LIST_READ_BYTES {
			break;
		}
		let target = zip_symlink_target(source, index.shift, entry, password, limits);
		if !target.is_empty() {
			held += (target.len() + size_of::<(u64, String)>()) as u64;
			targets.push((entry.ordinal, target));
		}
	}
	targets.sort_unstable_by_key(|(ordinal, _)| *ordinal);
	targets
}

/// A symlink entry's target, for reporting: its data, when small and readable.
fn zip_symlink_target<R: Read + std::io::Seek>(
	source: &mut R,
	shift: u64,
	entry: &ZipEntry,
	password: Option<&[u8]>,
	limits: EntryLimits,
) -> String {
	// an encrypted target is left unread: each one would cost a key derivation, and an archive
	// of nothing but encrypted links would spend minutes on them creating nothing
	if entry.size > MAX_ARCHIVE_PATH_BYTES as u64
		|| !zip_supported(entry)
		|| entry.encryption != ZipEncryption::None
	{
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

fn zip_failure(error: ZipError) -> Error {
	let kind = match &error {
		ZipError::Corrupt(_) => ErrorKind::ArchiveCorrupt,
		ZipError::Unsupported(_) => ErrorKind::ArchiveUnsupported,
		ZipError::TooLarge(_) => ErrorKind::ArchiveTooLarge,
		ZipError::PasswordRequired => ErrorKind::ArchivePasswordRequired,
		ZipError::WrongPassword => ErrorKind::ArchiveWrongPassword,
		ZipError::Overlapping => ErrorKind::ArchiveCorrupt,
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

/// What [`walk_tar`] leaves.
struct Walked<R> {
	/// What follows the end-of-archive marker.
	rest: R,
	/// Bytes stored under directory members, which nothing extracts.
	unread: u64,
	/// Files sent.
	files: u64,
}

/// Sends every member of the tar in `reader`, or what each is when listing.
fn walk_tar<R: Read>(walk: &mut Walk, reader: R, max_members: u64) -> Result<Walked<R>, Error> {
	let mut tar = TarReader::new(reader, max_members);
	let mut ordinal = 0;
	let mut unread = 0u64;
	let mut files = 0u64;
	// what a listing resolves hard links against: the files it says are extracted, by path, with
	// their sizes. An extraction's driver resolves them against the files it created
	let mut listed_files = SeededMap::<u64, u64>::default();
	while let Some(member) = tar.next_member().map_err(tar_failure)? {
		let this = ordinal;
		ordinal += 1;
		let (kind, unreadable) = match &member.kind {
			MemberKind::File => (ArchiveEntryKind::File, None),
			// a hard link with data of its own holds the file, as for libarchive
			MemberKind::Hardlink { .. } if member.size > 0 => (ArchiveEntryKind::File, None),
			MemberKind::Dir => {
				unread = unread.saturating_add(member.size);
				(ArchiveEntryKind::Dir, None)
			}
			MemberKind::Symlink { target } => {
				let target = display_path(target).0.to_owned();
				(
					ArchiveEntryKind::Symlink {
						target: target.clone(),
					},
					Some(ExtractSkipReason::Symlink { target }),
				)
			}
			MemberKind::Hardlink { target } => (
				ArchiveEntryKind::Hardlink {
					target: display_path(target).0.to_owned(),
				},
				None,
			),
			MemberKind::Device | MemberKind::Fifo => {
				(ArchiveEntryKind::Device, Some(ExtractSkipReason::Device))
			}
			MemberKind::Sparse => (ArchiveEntryKind::File, Some(ExtractSkipReason::Sparse)),
			MemberKind::Unsupported(_) => (
				ArchiveEntryKind::Other,
				Some(ExtractSkipReason::UnsupportedType),
			),
		};
		let mut found = Found {
			ordinal: this,
			stored: &member.path,
			path: entry_path(&member.path).map(|mut path| {
				path.rewritten |= member.path_rewritten;
				path
			}),
			kind,
			unreadable,
			size: member.size,
			modified: member
				.modified
				.and_then(|time| DateTime::from_timestamp(time.secs, time.nanos)),
			encrypted: false,
			method: None,
		};
		// a hard link names an earlier file, the same whatever else it is stored with
		let link_target = match (&member.kind, &found.kind) {
			(MemberKind::Hardlink { target }, ArchiveEntryKind::Hardlink { target: shown }) => {
				Some((entry_path(target), shown.clone()))
			}
			_ => None,
		};
		if walk.listing() {
			if let Some((target, shown)) = link_target {
				match target
					.ok()
					.and_then(|target| listed_files.get(&link_key(&target)))
				{
					Some(&size) => found.size = size,
					None => {
						found.unreadable = Some(ExtractSkipReason::Hardlink { target: shown });
					}
				}
			}
			// a listing reads a tar through: an AppleDouble member is told by its data here
			let apple_double = match found.mac_shape() {
				Some(MacShape::AppleDoubleName) => {
					Some(apple_double(&mut TarBody(&mut tar)).map_err(failure)?.0)
				}
				_ => None,
			};
			let key = found.path.as_ref().ok().map(link_key);
			let size = found.size;
			let is_file = found.kind == ArchiveEntryKind::File;
			if walk.list(found, apple_double).map_err(failure)?
				&& is_file && let Some(key) = key
			{
				listed_files.insert(key, size);
			}
			continue;
		}
		let (path, apple_double) = match walk.judge(&found)? {
			Verdict::Ignore | Verdict::Root => continue,
			Verdict::Skip(reason) => {
				walk.port.send(found.skipped(reason)).map_err(failure)?;
				continue;
			}
			Verdict::Take { path, apple_double } => (path, apple_double),
		};
		if let Some((target, shown)) = link_target {
			let unresolved = SkippedMember {
				ordinal: this,
				path: display_path(&member.path).0.to_owned(),
				path_truncated: display_path(&member.path).1,
				bytes: 0,
				reason: ExtractSkipReason::Hardlink { target: shown },
			};
			// the file it names, where this job extracts it
			let event = match target.ok().and_then(|target| walk.within_base(target)) {
				Some(target) => WorkerEvent::Link(Box::new(LinkHead {
					ordinal: this,
					path,
					modified: found.modified,
					target,
					unresolved,
				})),
				None => WorkerEvent::Skipped(unresolved),
			};
			walk.port.send(event).map_err(failure)?;
			continue;
		}
		if found.kind == ArchiveEntryKind::Dir {
			walk.port
				.send(WorkerEvent::Entry(EntryHead {
					ordinal: this,
					path,
					modified: found.modified,
					kind: EntryKind::Dir,
				}))
				.map_err(failure)?;
			continue;
		}
		files += take_file(
			walk,
			&found,
			path,
			Some(found.size),
			apple_double,
			&mut TarBody(&mut tar),
		)
		.map_err(failure)?;
	}
	walk.finish()?;
	Ok(Walked {
		rest: tar.into_inner(),
		unread,
		files,
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
