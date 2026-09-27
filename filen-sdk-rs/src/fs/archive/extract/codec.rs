//! The codec side of extracting an archive (a tar, a compressed tar, one compressed file, a zip or
//! 7z): runs on the codec worker, reads the archive through the driver chunk by chunk, and hands
//! the driver its entries in archive order.

use std::io::{self, Cursor, Read};

use chrono::DateTime;

use crate::{Error, ErrorKind};

use super::{
	super::{
		decode::{CodecError, StreamDecoder, Trailing, codec_error, open_stream},
		entry_path::{PathRejection, entry_path},
		format::{
			DETECT_HEAD_LEN, Detected, ExtensionFormat, archive_default_name, detect,
			extension_format, is_end_marker, is_tar_header,
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
		tar_iter::{MemberKind, TAR_BLOCK, TarError, TarMember, TarReader},
		worker::{
			ChunkInput, EntryHead, EntryKind, JobEnded, SeekInput, SkippedMember, SourceFailed,
			StreamLayout, WorkerEvent, WorkerPort, read_full, send_file_data,
		},
		zip::{
			crypto::{AES_AUTH_CODE_LEN, AES_VERIFIER_LEN, CryptoError, ZIP_CRYPTO_HEADER_LEN},
			read::{
				EntryLimits, ZipEncryption, ZipEntry, ZipError, ZipKind, ZipLimits, open_entry,
				read_index, unaccounted_after,
			},
		},
	},
	DuplicateEntries, ExpansionLimit, ExtractSkipReason,
};

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
	/// Entries extracted with no checksum in the archive to check them against.
	pub(crate) unchecked_entries: u64,
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
			let (rest, unread) = walk_tar(port, source, job.limits.max_members)?;
			Ok(ArchiveEnd {
				unaccounted_bytes: unread + drain_trailing(rest).map_err(failure)?,
				duplicates: None,
				unchecked_entries: 0,
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
			let mut block = [0u8; TAR_BLOCK as usize];
			let block_len = read_full(&mut decoded, &mut block).map_err(failure)?;
			let block = &block[..block_len];
			// an empty tar decodes to its end-of-archive marker alone, so only its name tells it
			// from a file of zeros
			let tar = block_len == block.len()
				&& (is_tar_header(block)
					|| is_end_marker(block)
						&& matches!(
							extension_format(&job.name),
							Some(ExtensionFormat::CompressedTar(_))
						));
			if tar {
				port.send(WorkerEvent::Opened(StreamLayout::Tar {
					codec: Some(codec),
				}))
				.map_err(failure)?;
				let (rest, unread) = walk_tar(
					port,
					Cursor::new(block).chain(decoded),
					job.limits.max_members,
				)?;
				let (_, mut decoded) = rest.into_inner();
				// zero blocks after the end-of-archive marker are the usual record padding
				let tar_trailing = drain_trailing(&mut decoded).map_err(failure)?;
				let end = decoded.inner.end().expect("drained to the end");
				Ok(ArchiveEnd {
					unaccounted_bytes: unread + tar_trailing + end.unaccounted_bytes,
					duplicates: None,
					unchecked_entries: 0,
				})
			} else {
				port.send(WorkerEvent::Opened(StreamLayout::Single { codec }))
					.map_err(failure)?;
				extract_single(port, &job.name, Cursor::new(block).chain(decoded))
			}
		}
		Some(Detected::Zip) => extract_zip(port, &job),
		Some(Detected::SevenZ) => extract_sevenz(port, &job),
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
		unchecked_entries: 0,
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
		let stated = index
			.entries
			.iter()
			.fold(0u64, |total, entry| total.saturating_add(entry.size));
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
	// set once an encrypted entry read back whole against its CRC-32 or authentication code
	let mut verified = encrypted().next().is_none();
	if !verified {
		let Some(password) = password else {
			return Err(zip_failure(ZipError::PasswordRequired));
		};
		// a password verifier alone lets a wrong password through now and then; reading the
		// smallest entry in full checks it against the CRC-32 or authentication code too. An
		// empty entry proves little (ZipCrypto's check byte lets 1 in 256 wrong passwords
		// through, and its CRC matches whatever the key), so one with data goes first
		if let Some(probe) = encrypted()
			.filter(|entry| zip_supported(entry))
			.min_by_key(|entry| (entry.size == 0, entry.compressed_size))
			&& probe.compressed_size <= PASSWORD_PROBE_BYTES
		{
			match open_entry(
				&mut source,
				index.shift,
				probe,
				Some(password),
				entry_limits,
			) {
				Ok(mut reader) => {
					io::copy(&mut reader, &mut io::sink()).map_err(
						|error| match zip_io_failure(error) {
							error
								if key_unproven(probe)
									&& error.kind() == ErrorKind::ArchiveCorrupt =>
							{
								Error::custom(
									ErrorKind::ArchiveWrongPassword,
									"the password is likely wrong",
								)
							}
							error => error,
						},
					)?;
					verified = true;
				}
				// skipped when its turn comes; the password is checked on the entries read
				Err(ZipError::Overlapping) => {}
				Err(error) => return Err(zip_failure(error)),
			}
		}
	}

	port.send(WorkerEvent::Opened(StreamLayout::Zip))
		.map_err(failure)?;
	for entry in &index.overlapping {
		port.send(zip_skipped(entry, ExtractSkipReason::OverlappingData))
			.map_err(failure)?;
	}
	let mut unaccounted_bytes = index
		.prefix_bytes
		.saturating_add(index.directory_slack)
		.saturating_add(index.trailing_bytes);
	for entry in &index.entries {
		unaccounted_bytes =
			unaccounted_bytes.saturating_add(unaccounted_after(&mut source, index.shift, entry));
		if entry.kind == ZipKind::Dir
			&& entry.compressed_size > 0
			&& !decodes_to_nothing(&mut source, index.shift, entry, password, entry_limits)
		{
			// a directory holds no data: anything stored under one is extracted nowhere
			unaccounted_bytes = unaccounted_bytes.saturating_add(entry.compressed_size);
		}
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
		let head = EntryHead {
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
		};
		if is_dir {
			port.send(WorkerEvent::Entry(head)).map_err(failure)?;
			continue;
		}
		// opened before it is announced: its local header may show it overlapping the next
		let mut reader = match open_entry(&mut source, index.shift, entry, password, entry_limits) {
			Ok(reader) => reader,
			Err(ZipError::Overlapping) => {
				port.send(zip_skipped(entry, ExtractSkipReason::OverlappingData))
					.map_err(failure)?;
				continue;
			}
			Err(error) => return Err(zip_failure(error)),
		};
		port.send(WorkerEvent::Entry(head)).map_err(failure)?;
		let encrypted = entry.encryption != ZipEncryption::None;
		match send_file_data(port, &mut reader).map_err(zip_io_failure) {
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
		port.send(WorkerEvent::FileEnd).map_err(failure)?;
	}
	Ok(ArchiveEnd {
		unaccounted_bytes,
		duplicates: (index.duplicate_count > 0).then(|| DuplicateEntries {
			names: index.duplicate_names.clone(),
			count: index.duplicate_count,
		}),
		unchecked_entries: 0,
	})
}

/// A 7z: its entries in header order, folder by folder, each checked against its CRC-32 when the
/// header lists one.
fn extract_sevenz(port: &WorkerPort, job: &StreamJob) -> Result<ArchiveEnd, Error> {
	let mut source = SeekInput::new(port, 0, job.len);
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
	if let Some(limit) = job.limits.expansion {
		let stated = index
			.entries
			.iter()
			.fold(0u64, |total, entry| total.saturating_add(entry.size));
		if stated > limit.floor.max(job.len.saturating_mul(limit.ratio)) {
			return Err(refused(Refused::Expansion(limit.ratio)));
		}
	}
	let encrypted = |entry: &SevenZEntry| {
		entry
			.stream
			.is_some_and(|stream| index.folders[stream.folder].encrypted())
	};
	// an encrypted header only decodes with the right password; encrypted data is checked on
	// the entry cheapest to reach that has a CRC-32, before anything is created. An empty entry
	// proves nothing: decoding nothing matches its CRC-32 under any key
	let mut verified = index.headers_encrypted || !index.entries.iter().any(encrypted);
	let mut cursor = FolderCursor::new(source, limits.decoder_memory);
	if !verified {
		if password.is_none() {
			return Err(sevenz_failure(SevenZError::PasswordRequired));
		}
		let probe = index
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
			});
		if let Some(probe) = probe {
			// setting the folder up fails for the archive's reasons; what decodes wrong under
			// the key (skipping to the entry, or the entry itself) is the key's
			let mut data = cursor.open(&index, probe, &mut keys).map_err(|error| {
				sevenz_failure(match error {
					SevenZError::Read(error) if !from_source(&error) => SevenZError::WrongPassword,
					SevenZError::Corrupt(FOLDER_ENDS_EARLY) => SevenZError::WrongPassword,
					error => error,
				})
			})?;
			io::copy(&mut data, &mut io::sink())
				.map_err(|error| sevenz_failure(wrong_key(read_error(error))))?;
			verified = true;
		}
	}

	port.send(WorkerEvent::Opened(StreamLayout::SevenZ))
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
		let supported = entry
			.stream
			.is_none_or(|stream| index.folders[stream.folder].supported());
		// a reparse point's data says whether it is a link, so it is read before anything is
		// sent, and sent from here when it is a file's
		let mut held = None;
		let skip = match entry.kind {
			SevenZKind::Anti => Some(ExtractSkipReason::AntiItem),
			SevenZKind::Symlink => Some(ExtractSkipReason::Symlink {
				target: sevenz_symlink_target(&mut cursor, &index, entry, &mut keys),
			}),
			_ if !supported => Some(ExtractSkipReason::UnsupportedMethod),
			SevenZKind::Reparse => {
				let data = cursor
					.open(&index, entry, &mut keys)
					.map_err(sevenz_failure)
					.and_then(|mut data| {
						let mut bytes = Vec::new();
						data.read_to_end(&mut bytes).map_err(sevenz_io_failure)?;
						Ok(bytes)
					})
					.map_err(|error| judged(error, entry, verified))?;
				verified |= proves(entry);
				match windows_link_target(&data) {
					Some(target) => Some(ExtractSkipReason::Symlink {
						target: display_path(&target).0.to_owned(),
					}),
					None => {
						held = Some(data);
						None
					}
				}
			}
			SevenZKind::File | SevenZKind::Dir => None,
		};
		if let Some(reason) = skip {
			port.send(sevenz_skipped(entry, reason)).map_err(failure)?;
			continue;
		}
		let is_dir = entry.kind == SevenZKind::Dir;
		let mut path = match entry_path(&entry.name) {
			Ok(path) => path,
			Err(PathRejection::Empty) if is_dir => continue,
			Err(rejection) => {
				port.send(sevenz_skipped(entry, path_skip_reason(rejection)))
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
		if is_dir {
			continue;
		}
		if entry.stream.is_some() {
			let sent = match held {
				Some(data) => send_file_data(port, &mut data.as_slice()).map_err(failure),
				None => cursor
					.open(&index, entry, &mut keys)
					.map_err(sevenz_failure)
					.and_then(|mut data| {
						send_file_data(port, &mut data).map_err(sevenz_io_failure)
					}),
			};
			sent.map_err(|error| judged(error, entry, verified))?;
			verified |= proves(entry);
			if entry.crc.is_none() {
				unchecked_entries += 1;
			}
		}
		port.send(WorkerEvent::FileEnd).map_err(failure)?;
	}
	Ok(ArchiveEnd {
		unaccounted_bytes: index.unaccounted_bytes,
		duplicates: None,
		unchecked_entries,
	})
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

fn sevenz_skipped(entry: &SevenZEntry, reason: ExtractSkipReason) -> WorkerEvent {
	let (path, path_truncated) = display_path(&entry.name);
	WorkerEvent::Skipped(SkippedMember {
		ordinal: entry.ordinal,
		path: path.to_owned(),
		path_truncated,
		bytes: entry.size,
		reason,
	})
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
	match entry.method {
		0 | 8 | 9 | 12 => true,
		14 | 95 => entry.encryption == ZipEncryption::None,
		_ => false,
	}
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
		0 => data == Some(0),
		// an empty deflate (or deflate64) stream takes 2 bytes, too few for any literal and its
		// block's end
		8 | 9 => data.is_some_and(|data| data <= 2),
		// an empty bzip2 stream is its 4-byte header and 10-byte end: no room for a block
		12 => data.is_some_and(|data| data <= 14),
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

/// Sends every member of the tar in `reader`; returns what follows its end-of-archive marker,
/// and the bytes stored under directory members, which nothing extracts.
fn walk_tar<R: Read>(port: &WorkerPort, reader: R, max_members: u64) -> Result<(R, u64), Error> {
	let mut tar = TarReader::new(reader, max_members);
	let mut ordinal = 0;
	let mut unread = 0u64;
	while let Some(member) = tar.next_member().map_err(tar_failure)? {
		let this = ordinal;
		ordinal += 1;
		let is_dir = match &member.kind {
			// a hard link with data of its own holds the file, as for libarchive
			MemberKind::File => false,
			MemberKind::Hardlink { .. } if member.size > 0 => false,
			MemberKind::Dir => {
				unread = unread.saturating_add(member.size);
				true
			}
			MemberKind::Symlink { target } => {
				let reason = ExtractSkipReason::Symlink {
					target: display_path(target).0.to_owned(),
				};
				port.send(skipped(this, &member, reason)).map_err(failure)?;
				continue;
			}
			other => {
				let reason = match other {
					MemberKind::Hardlink { target } => ExtractSkipReason::Hardlink {
						target: display_path(target).0.to_owned(),
					},
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
	Ok((tar.into_inner(), unread))
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
