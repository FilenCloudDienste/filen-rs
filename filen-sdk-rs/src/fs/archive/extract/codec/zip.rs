//! Reading a zip: its central directory first, then its entries in the order of their local
//! headers.

use std::io::{self, Read, Seek};

use crate::{
	Error, ErrorKind,
	fs::archive::{
		entry_path::{ArchivePath, entry_path},
		format::ArchiveFormat,
		limits::{MAX_ARCHIVE_PATH_BYTES, display_path},
		password::ArchivePassword,
		worker::{EntryHead, EntryKind, SeekInput, WorkerEvent, WorkerPort},
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
};

use super::{
	super::{
		DuplicateEntries, ExtractSkipReason,
		list::{ArchiveEntryKind, PasswordCheck},
		storage_exceeded,
	},
	ArchiveEnd, LIST_READ_BYTES, PASSWORD_PROBE_BYTES, StreamJob, check_stated_size,
	entries::{Found, Verdict, Walk},
	failure, take_file,
};

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
pub(super) fn extract_zip(
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
		return list_zip(
			walk,
			&mut source,
			&index,
			password,
			entry_limits,
			duplicates,
		);
	}
	walk.check_selection(index.entries.iter().chain(&index.overlapping).map(|entry| {
		(
			entry.ordinal,
			entry.name.as_str(),
			entry.kind == ZipKind::Dir,
		)
	}))?;
	// the sizes a zip states are known up front, so a bomb is refused before it is decoded
	check_stated_size(job, index.entries.iter().map(|entry| entry.size))?;
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
	let mut unaccounted_bytes = around_entries(&index);
	for entry in &index.entries {
		let found = zip_found(entry, false, String::new());
		let verdict = walk.judge(&found)?;
		// what a partial extraction leaves out is not read at all, its local header included: a
		// zip extracted in part reports the bytes around what it read only
		if matches!(verdict, Verdict::Ignore) {
			continue;
		}
		unaccounted_bytes = unaccounted_bytes.saturating_add(unaccounted_at(
			&mut source,
			index.shift,
			entry,
			password,
			entry_limits,
		));
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
		match open_entry(&mut source, index.shift, entry, password, entry_limits) {
			Ok(mut reader) => {
				verified |= take_zip_file(
					walk,
					&found,
					path,
					apple_double,
					entry,
					&mut reader,
					verified,
				)?;
			}
			Err(ZipError::Overlapping) => {
				walk.port
					.send(found.skipped(ExtractSkipReason::OverlappingData))
					.map_err(failure)?;
			}
			Err(error) => return Err(zip_failure(error)),
		}
	}
	Ok(ArchiveEnd {
		unaccounted_bytes,
		duplicates,
		unchecked_entries: 0,
		password: PasswordCheck::NotNeeded,
	})
}

/// A zip listed from its index alone: what each entry is, with the symlink targets read front
/// to back.
fn list_zip(
	walk: &mut Walk,
	source: &mut SeekInput<'_>,
	index: &ZipIndex,
	password: Option<&[u8]>,
	limits: EntryLimits,
	duplicates: Option<DuplicateEntries>,
) -> Result<ArchiveEnd, Error> {
	let checked = check_zip_password(source, index, password, limits)?;
	walk.port
		.send(WorkerEvent::Opened(ArchiveFormat::Zip))
		.map_err(failure)?;
	let mut targets = zip_symlink_targets(walk.port, source, index, password, limits);
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
	Ok(ArchiveEnd {
		unaccounted_bytes: around_entries(index),
		duplicates,
		unchecked_entries: 0,
		password: checked,
	})
}

/// The bytes around a zip's entries that its index shows belong to none: before the first, in
/// the central directory, and after its end record.
fn around_entries(index: &ZipIndex) -> u64 {
	index
		.prefix_bytes
		.saturating_add(index.directory_slack)
		.saturating_add(index.trailing_bytes)
}

/// The bytes of `entry` and after it that belong to nothing: past its data (see
/// [`unaccounted_after`]), and for a directory, whatever is stored under it.
fn unaccounted_at<R: Read + Seek>(
	source: &mut R,
	shift: u64,
	entry: &ZipEntry,
	password: Option<&[u8]>,
	limits: EntryLimits,
) -> u64 {
	let after = unaccounted_after(source, shift, entry);
	// a directory holds no data: anything stored under one is extracted nowhere
	if entry.kind == ZipKind::Dir
		&& entry.compressed_size > 0
		&& !decodes_to_nothing(source, shift, entry, password, limits)
	{
		return after.saturating_add(entry.compressed_size);
	}
	after
}

/// Sends the file `entry`, taken at `path`, its data read from `reader`; whether reading it
/// whole proved the password, which `verified` says an entry before it already did.
fn take_zip_file(
	walk: &Walk,
	found: &Found,
	path: ArchivePath,
	apple_double: bool,
	entry: &ZipEntry,
	reader: &mut dyn Read,
	verified: bool,
) -> Result<bool, Error> {
	let encrypted = entry.encryption != ZipEncryption::None;
	match take_file(walk, found, path, Some(entry.size), apple_double, reader)
		.map_err(zip_io_failure)
	{
		// an AppleDouble file left out was not read to its end: it proves nothing
		Ok(0) => Ok(false),
		// an empty ZipCrypto entry matches its CRC-32 under any key; AES's authentication code
		// rejects a wrong one even over nothing
		Ok(_) => {
			Ok(encrypted
				&& (entry.size > 0 || matches!(entry.encryption, ZipEncryption::Aes { .. })))
		}
		// while no entry proved the password, damage in ZipCrypto data is likelier a wrong
		// password than a damaged archive (its check byte passes 1 wrong one in 256; AES's
		// verifier, which already passed, 1 in 65536)
		Err(error)
			if key_unproven(entry) && !verified && error.kind() == ErrorKind::ArchiveCorrupt =>
		{
			Err(Error::custom(
				ErrorKind::ArchiveWrongPassword,
				"the password is likely wrong",
			))
		}
		Err(error) => Err(error),
	}
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
