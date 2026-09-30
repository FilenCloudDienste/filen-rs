//! Reading a zip: its central directory first, then its entries in the order of their local
//! headers.

use std::io::{self, Read, Seek};

use crate::{
	Error, ErrorKind,
	fs::archive::{
		entry_path::ArchivePath,
		error::read_failure,
		format::ArchiveFormat,
		limits::MAX_ARCHIVE_PATH_BYTES,
		password::ArchivePassword,
		worker::{EntryKind, SeekInput, WorkerEvent, from_source},
		zip::{
			METHOD_BZIP2, METHOD_DEFLATE, METHOD_DEFLATE64, METHOD_LZMA, METHOD_PPMD,
			METHOD_STORED, METHOD_XZ, METHOD_ZSTD,
			crypto::{AES_AUTH_CODE_LEN_U64, AES_VERIFIER_LEN, ZIP_CRYPTO_HEADER_LEN_U64},
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
	ArchiveEnd, LIST_READ_BYTES, PASSWORD_PROBE_BYTES, StreamJob, Taken, check_stated_size,
	entries::{Found, MacShape, Verdict, Walk, apple_double, found_path, symlink},
	likely_wrong_password, link_target, take_file,
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
		ZipKind::Symlink => symlink(target),
		ZipKind::Dir => (ArchiveEntryKind::Dir, None),
		ZipKind::File => (
			ArchiveEntryKind::File,
			(!zip_supported(entry)).then_some(ExtractSkipReason::UnsupportedMethod),
		),
	};
	Found {
		ordinal: entry.ordinal,
		stored: &entry.name,
		path: found_path(&entry.name, entry.name_rewritten),
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
	password: Option<&ArchivePassword>,
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
		Ok(mut reader) => match io::copy(&mut reader, &mut io::sink()).map_err(read_failure) {
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
		Err(error) => Err(Error::from(error)),
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
	let index = read_index(&mut source, job.len, limits).map_err(Error::from)?;
	let entry_limits = EntryLimits {
		decoder_memory: job.limits.decoder_memory,
	};
	let password = job.password.as_ref();
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
	let mut verified = check_zip_password(&mut source, &index, password, entry_limits)?
		.verified(ZipError::PasswordRequired, ZipError::WrongPassword)
		.map_err(Error::from)?;

	walk.port
		.send(WorkerEvent::Opened(ArchiveFormat::Zip))
		.map_err(read_failure)?;
	for entry in &index.overlapping {
		let found = zip_found(entry, true, String::new());
		if let Verdict::Skip(reason) = walk.judge(&found)? {
			walk.port
				.send(found.skipped(reason))
				.map_err(read_failure)?;
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
			Verdict::Ignore | Verdict::Root | Verdict::Held => continue,
			Verdict::Skip(ExtractSkipReason::Symlink { .. }) => {
				let target =
					zip_symlink_target(&mut source, index.shift, entry, password, entry_limits)
						.map_err(read_failure)?;
				walk.port
					.send(found.skipped(ExtractSkipReason::Symlink { target }))
					.map_err(read_failure)?;
				continue;
			}
			Verdict::Skip(reason) => {
				walk.port
					.send(found.skipped(reason))
					.map_err(read_failure)?;
				continue;
			}
			Verdict::Take { path, apple_double } => (path, apple_double),
		};
		if entry.kind == ZipKind::Dir {
			walk.port
				.send(found.head(path, EntryKind::Dir))
				.map_err(read_failure)?;
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
					.map_err(read_failure)?;
			}
			Err(error) => return Err(Error::from(error)),
		}
	}
	walk.send_mac_folders().map_err(read_failure)?;
	Ok(ArchiveEnd {
		unaccounted_bytes,
		duplicates,
		unchecked_entries: 0,
		password: PasswordCheck::NotNeeded,
	})
}

/// A zip listed from its index alone: what each entry is, with the symlink targets and what
/// tells an AppleDouble file read front to back.
fn list_zip(
	walk: &mut Walk,
	source: &mut SeekInput<'_>,
	index: &ZipIndex,
	password: Option<&ArchivePassword>,
	limits: EntryLimits,
	duplicates: Option<DuplicateEntries>,
) -> Result<ArchiveEnd, Error> {
	let checked = check_zip_password(source, index, password, limits)?;
	walk.port
		.send(WorkerEvent::Opened(ArchiveFormat::Zip))
		.map_err(read_failure)?;
	let mut read = zip_pre_read(walk, source, index, password, limits)?;
	let mut listed: Vec<(&ZipEntry, bool)> = index
		.entries
		.iter()
		.map(|entry| (entry, false))
		.chain(index.overlapping.iter().map(|entry| (entry, true)))
		.collect();
	listed.sort_unstable_by_key(|(entry, _)| entry.ordinal);
	for (entry, overlapping) in listed {
		let read = read
			.binary_search_by_key(&entry.ordinal, |(ordinal, _)| *ordinal)
			.ok()
			.map(|at| std::mem::replace(&mut read[at].1, PreRead::Unread));
		let (target, apple_double) = match read {
			Some(PreRead::Target(target)) => (target, None),
			Some(PreRead::AppleDouble(apple_double)) => (String::new(), Some(apple_double)),
			Some(PreRead::Unread) | None => (String::new(), None),
		};
		walk.list(zip_found(entry, overlapping, target), apple_double)?;
	}
	walk.send_mac_folders().map_err(read_failure)?;
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
	password: Option<&ArchivePassword>,
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
	walk: &mut Walk,
	found: &Found,
	path: ArchivePath,
	apple_double: bool,
	entry: &ZipEntry,
	reader: &mut dyn Read,
	verified: bool,
) -> Result<bool, Error> {
	let encrypted = entry.encryption != ZipEncryption::None;
	match take_file(walk, found, path, Some(entry.size), apple_double, reader).map_err(read_failure)
	{
		// an AppleDouble file left out was not read to its end: it proves nothing
		Ok(Taken::LeftOut) => Ok(false),
		// an empty ZipCrypto entry matches its CRC-32 under any key; AES's authentication code
		// rejects a wrong one even over nothing
		Ok(Taken::File) => {
			Ok(encrypted
				&& (entry.size > 0 || matches!(entry.encryption, ZipEncryption::Aes { .. })))
		}
		// while no entry proved the password, damage in ZipCrypto data is likelier a wrong
		// password than a damaged archive (its check byte passes 1 wrong one in 256; AES's
		// verifier, which already passed, 1 in 65536)
		Err(error)
			if key_unproven(entry) && !verified && error.kind() == ErrorKind::ArchiveCorrupt =>
		{
			Err(likely_wrong_password())
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
	password: Option<&ArchivePassword>,
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
		ZipEncryption::ZipCrypto { .. } => ZIP_CRYPTO_HEADER_LEN_U64,
		ZipEncryption::Aes { strength, .. } => {
			strength.salt_len() as u64 + AES_VERIFIER_LEN + AES_AUTH_CODE_LEN_U64
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

/// What a zip listing read of an entry's data: a symlink's target, or whether a file that
/// may be AppleDouble is.
enum PreRead {
	Target(String),
	AppleDouble(bool),
	Unread,
}

/// What a zip listing reads of its entries' data, by ordinal: its symlinks' targets and, when
/// the listing leaves macOS metadata out, whether each file that may be AppleDouble (see
/// [`MacShape::AppleDoubleName`]) is, as an extraction tells it. They are read in the order
/// their data is stored, so the archive is read front to back once, however its index orders
/// them, rather than a chunk fetched again for each; and only until reading them has fetched
/// [`LIST_READ_BYTES`] of the archive or they hold that much: the entries past it are listed
/// unread, as is one whose data would take the reading past it (a file that may be AppleDouble
/// as metadata it may be, unskipped). An overlapping or encrypted entry's data is never read.
/// Fails on the first read the archive's source failed (a fetch, or the job ending), which no
/// later one would get past.
fn zip_pre_read(
	walk: &Walk,
	source: &mut SeekInput<'_>,
	index: &ZipIndex,
	password: Option<&ArchivePassword>,
	limits: EntryLimits,
) -> Result<Vec<(u64, PreRead)>, Error> {
	let port = walk.port;
	let first_read = port.shared().input_bytes();
	let mut held = 0;
	let mut read = Vec::new();
	for entry in &index.entries {
		let apple_double = walk.skips_mac_metadata()
			&& entry.kind == ZipKind::File
			&& entry.encryption == ZipEncryption::None
			&& zip_supported(entry)
			&& zip_found(entry, false, String::new()).mac_shape()
				== Some(MacShape::AppleDoubleName);
		if entry.kind != ZipKind::Symlink && !apple_double {
			continue;
		}
		let fetched = port.shared().input_bytes() - first_read;
		if fetched > LIST_READ_BYTES || held > LIST_READ_BYTES {
			break;
		}
		// its stated compressed size is what reading it may fetch, however little it holds
		if fetched.saturating_add(entry.compressed_size) > LIST_READ_BYTES {
			continue;
		}
		let entry_read = if apple_double {
			zip_apple_double(source, index.shift, entry, limits).map(PreRead::AppleDouble)
		} else {
			zip_symlink_target(source, index.shift, entry, password, limits).map(PreRead::Target)
		};
		match entry_read.map_err(read_failure)? {
			PreRead::Target(target) if target.is_empty() => {}
			entry_read => {
				held += (size_of::<(u64, PreRead)>()
					+ match &entry_read {
						PreRead::Target(target) => target.len(),
						_ => 0,
					}) as u64;
				read.push((entry.ordinal, entry_read));
			}
		}
	}
	read.sort_unstable_by_key(|(ordinal, _)| *ordinal);
	Ok(read)
}

/// Whether an unencrypted file entry is AppleDouble, by its first bytes, as an extraction
/// tells it; `false` when its data cannot be read. Fails only on an error of the archive's
/// source.
fn zip_apple_double<R: Read + std::io::Seek>(
	source: &mut R,
	shift: u64,
	entry: &ZipEntry,
	limits: EntryLimits,
) -> io::Result<bool> {
	let read = match open_entry(source, shift, entry, None, limits) {
		Ok(mut reader) => apple_double(&mut reader).map(|(apple_double, _)| apple_double),
		Err(ZipError::Read(error)) => Err(error),
		Err(_) => return Ok(false),
	};
	match read {
		Ok(apple_double) => Ok(apple_double),
		Err(error) if from_source(&error) => Err(error),
		// damaged data is no AppleDouble file: an extraction would fail on it, not leave it out
		Err(_) => Ok(false),
	}
}

/// Most compressed data a symlink's target is read from: twice the longest path, room for any
/// method's overhead on a real one. A stream padded out (with empty deflate blocks, say) past it
/// states a short target in as much of the archive as it likes.
const MAX_TARGET_COMPRESSED: u64 = 2 * MAX_ARCHIVE_PATH_BYTES as u64;

/// A symlink entry's target, for reporting: its data, when small and readable; empty when not.
/// Fails only on an error of the archive's source, not of the entry's data.
fn zip_symlink_target<R: Read + std::io::Seek>(
	source: &mut R,
	shift: u64,
	entry: &ZipEntry,
	password: Option<&ArchivePassword>,
	limits: EntryLimits,
) -> io::Result<String> {
	// an encrypted target is left unread: each one would cost a key derivation, and an archive
	// of nothing but encrypted links would spend minutes on them creating nothing
	if entry.size > MAX_ARCHIVE_PATH_BYTES as u64
		|| entry.compressed_size > MAX_TARGET_COMPRESSED
		|| !zip_supported(entry)
		|| entry.encryption != ZipEncryption::None
	{
		return Ok(String::new());
	}
	let data = match open_entry(source, shift, entry, password, limits) {
		Ok(reader) => Ok(reader),
		Err(ZipError::Read(error)) => Err(error),
		Err(_) => return Ok(String::new()),
	};
	link_target(data)
}
