//! Reading a 7z: its header first, then its entries folder by folder, in header order.

use std::io::{self, Read, Seek};

use crate::{
	Error, ErrorKind,
	fs::archive::{
		entry_path::entry_path,
		format::ArchiveFormat,
		limits::{MAX_ARCHIVE_PATH_BYTES, display_path},
		password::ArchivePassword,
		sevenz::{
			SevenZError,
			read::{
				FOLDER_ENDS_EARLY, FolderCursor, Keys, SevenZEntry, SevenZIndex, SevenZKind,
				SevenZLimits, read_error, read_index as read_sevenz_index, windows_link_target,
				wrong_key,
			},
		},
		worker::{EntryHead, EntryKind, SeekInput, WorkerEvent, from_source},
	},
};

use super::{
	super::{
		ExtractSkipReason,
		list::{ArchiveEntryKind, PasswordCheck},
		storage_exceeded,
	},
	ArchiveEnd, LIST_READ_BYTES, PASSWORD_PROBE_BYTES, StreamJob, check_stated_size,
	entries::{Found, Verdict, Walk},
	failure, take_file,
};

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
pub(super) fn extract_sevenz(
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
		return list_sevenz(
			walk,
			&mut cursor,
			&index,
			&mut keys,
			password.is_some(),
			job,
		);
	}
	walk.check_selection(index.entries.iter().map(|entry| {
		(
			entry.ordinal,
			entry.name.as_str(),
			entry.kind == SevenZKind::Dir,
		)
	}))?;
	check_stated_size(job, index.entries.iter().map(|entry| entry.size))?;
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

/// A 7z listed from its index: what each entry is, a link's data read only where little has to
/// be decoded to reach it, and never when the archive states more than it may decode to.
fn list_sevenz<'s, R: Read + Seek + 's>(
	walk: &mut Walk,
	cursor: &mut FolderCursor<'s, R>,
	index: &SevenZIndex,
	keys: &mut Keys<'_>,
	has_password: bool,
	job: &StreamJob,
) -> Result<ArchiveEnd, Error> {
	let checked = check_sevenz_password(cursor, index, keys, has_password)?;
	walk.port
		.send(WorkerEvent::Opened(ArchiveFormat::SevenZ))
		.map_err(failure)?;
	let within_limit = check_stated_size(job, index.entries.iter().map(|entry| entry.size)).is_ok();
	for entry in &index.entries {
		let cheap = entry
			.stream
			.is_some_and(|stream| stream.offset.saturating_add(entry.size) <= LIST_READ_BYTES);
		if !(within_limit && cheap) {
			walk.list(sevenz_unread(index, entry), None)
				.map_err(failure)?;
			continue;
		}
		// a link's data unread for a wrong password leaves it listed as a link without its
		// target
		let found = match sevenz_found(cursor, index, entry, keys) {
			Ok((found, _)) => found,
			Err(_) if checked == PasswordCheck::Wrong || !has_password => {
				sevenz_unread(index, entry)
			}
			Err(error) => return Err(sevenz_failure(error)),
		};
		walk.list(found, None).map_err(failure)?;
	}
	Ok(ArchiveEnd {
		unaccounted_bytes: index.unaccounted_bytes,
		duplicates: None,
		unchecked_entries: 0,
		password: checked,
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
