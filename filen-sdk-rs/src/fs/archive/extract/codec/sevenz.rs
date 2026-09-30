//! Reading a 7z: its header first, then its entries folder by folder, in header order.

use std::io::{self, Read, Seek};

use crate::{
	Error, ErrorKind,
	fs::archive::{
		error::read_failure,
		extract::{
			ExtractSkipReason,
			list::{ArchiveEntryKind, PasswordCheck},
			storage_exceeded,
		},
		format::ArchiveFormat,
		limits::{MAX_ARCHIVE_PATH_BYTES, display_path},
		sevenz::{
			SevenZError,
			read::{
				FolderCursor, Keys, SevenZEntry, SevenZIndex, SevenZKind, SevenZLimits, read_error,
				read_index as read_sevenz_index, windows_link_target, wrong_key,
			},
		},
		worker::{EntryKind, SeekInput, WorkerEvent, from_source},
	},
};

use super::{
	ArchiveEnd, LIST_READ_BYTES, PASSWORD_PROBE_BYTES, StreamJob, Taken, check_stated_size,
	entries::{Found, MacShape, Verdict, Walk, apple_double, found_path, symlink},
	likely_wrong_password, link_target, take_file,
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
) -> Result<PasswordCheck, Error> {
	if index.headers_encrypted {
		return Ok(PasswordCheck::Right);
	}
	if !index.entries.iter().any(|entry| index.encrypted(entry)) {
		return Ok(PasswordCheck::NotNeeded);
	}
	if !keys.has_password() {
		return Ok(PasswordCheck::Required);
	}
	let Some(probe) = index
		.entries
		.iter()
		.filter_map(|entry| Some((entry, entry.stream?)))
		.filter(|(entry, stream)| {
			index.encrypted(entry)
				&& entry.size > 0
				&& stream.crc.is_some()
				&& index.supported(entry)
		})
		.min_by_key(|(entry, stream)| stream.offset + entry.size)
		.filter(|(entry, stream)| stream.offset + entry.size <= PASSWORD_PROBE_BYTES)
		.map(|(entry, _)| entry)
	else {
		return Ok(PasswordCheck::Unchecked);
	};
	// setting the folder up fails for the archive's reasons; what decodes wrong under the key
	// (skipping to the entry, or the entry itself) is the key's
	let probed = cursor
		.open(index, probe, keys)
		.map_err(|error| match error {
			SevenZError::Read(error) if !from_source(&error) => SevenZError::WrongPassword,
			SevenZError::FolderEndsEarly | SevenZError::Decode(_) => SevenZError::WrongPassword,
			error => error,
		})
		.and_then(|mut data| {
			io::copy(&mut data, &mut io::sink()).map_err(|error| wrong_key(read_error(error)))
		});
	match probed {
		Ok(_) => Ok(PasswordCheck::Right),
		Err(SevenZError::WrongPassword) => Ok(PasswordCheck::Wrong),
		Err(error) => Err(Error::from(error)),
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
	let mut found = sevenz_unread(index, entry);
	let target = match entry.kind {
		SevenZKind::Symlink => sevenz_symlink_target(cursor, index, entry, keys)?,
		SevenZKind::Reparse if index.supported(entry) => {
			let mut data = Vec::new();
			cursor
				.open(index, entry, keys)?
				.read_to_end(&mut data)
				.map_err(read_error)?;
			match windows_link_target(&data) {
				Some(target) => Some(display_path(&target).0.to_owned()),
				None => return Ok((found, Some(data))),
			}
		}
		_ => return Ok((found, None)),
	};
	(found.kind, found.unreadable) = symlink(target);
	Ok((found, None))
}

/// A 7z read from `source`: its entries in header order, folder by folder, each checked against
/// its CRC-32 when the header lists one.
pub(super) fn extract_sevenz(
	walk: &mut Walk,
	mut source: SeekInput<'_>,
	job: &StreamJob,
) -> Result<ArchiveEnd, Error> {
	let port = walk.port;
	// a derivation exchanges nothing with the driver for up to a minute: without this it would
	// be given up on as a dead codec, and a cancel would wait it out
	let keep_alive = || port.keep_alive();
	let mut keys = Keys::new(job.password.as_ref()).with_on_round(&keep_alive);
	let limits = SevenZLimits {
		max_index_bytes: job.limits.max_index_bytes,
		max_entries: job.limits.max_members,
		decoder_memory: job.limits.decoder_memory,
	};
	let index = read_sevenz_index(&mut source, job.len, limits, &mut keys).map_err(Error::from)?;
	// a folder's packed streams are read in turns (BCJ2 has four)
	// (at most 5 chunks: a folder of several BCJ2 coders refetches rather than hold more)
	source.set_slots((index.max_packed_streams() + 1).min(5));
	let mut cursor = FolderCursor::new(source, limits.decoder_memory);
	if walk.listing() {
		return list_sevenz(walk, &mut cursor, &index, &mut keys, job);
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
			.filter(|entry| entry.kind == SevenZKind::File && index.supported(entry))
			.map(|entry| (entry.ordinal, entry.name.as_str(), entry.size)),
	);
	if let Some(error) = storage_exceeded(job.limits.max_bytes, extracted) {
		return Err(error);
	}
	let mut verified = check_sevenz_password(&mut cursor, &index, &mut keys)?
		.verified(SevenZError::PasswordRequired, SevenZError::WrongPassword)
		.map_err(Error::from)?;

	port.send(WorkerEvent::Opened(ArchiveFormat::SevenZ))
		.map_err(read_failure)?;
	let mut unchecked_entries = 0;
	// while the password is unchecked, damage in encrypted data is likelier a wrong password
	// than a damaged archive
	let judged = |error: Error, entry: &SevenZEntry, verified: bool| {
		if !verified && index.encrypted(entry) && error.kind() == ErrorKind::ArchiveCorrupt {
			likely_wrong_password()
		} else {
			error
		}
	};
	// whether reading an entry whole against its CRC-32 proved the password
	let proves = |entry: &SevenZEntry| {
		entry.stream.is_some_and(|stream| stream.crc.is_some())
			&& index.encrypted(entry)
			&& entry.size > 0
	};
	for entry in &index.entries {
		// what is left out is not read: its name decides whether it is chosen
		let verdict = walk.judge(&sevenz_unread(&index, entry))?;
		if matches!(verdict, Verdict::Ignore) {
			continue;
		}
		// a reparse point's data says whether it is a link, so it is read before anything is
		// sent, and sent from here when it is a file's
		let (found, held) = sevenz_found(&mut cursor, &index, entry, &mut keys)
			.map_err(|error| judged(Error::from(error), entry, verified))?;
		if entry.kind == SevenZKind::Reparse {
			verified |= proves(entry);
		}
		let (path, apple_double) = match walk.judge_again(&found, verdict) {
			Verdict::Ignore | Verdict::Root | Verdict::Held => continue,
			Verdict::Skip(reason) => {
				port.send(found.skipped(reason)).map_err(read_failure)?;
				continue;
			}
			Verdict::Take { path, apple_double } => (path, apple_double),
		};
		if entry.kind == SevenZKind::Dir {
			port.send(found.head(path, EntryKind::Dir))
				.map_err(read_failure)?;
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
			.map_err(read_failure),
			(Some(_), None) => cursor
				.open(&index, entry, &mut keys)
				.map_err(Error::from)
				.and_then(|mut data| {
					take_file(
						walk,
						&found,
						path,
						Some(entry.size),
						apple_double,
						&mut data,
					)
					.map_err(read_failure)
				}),
			(None, _) => take_file(walk, &found, path, Some(0), false, &mut io::empty())
				.map_err(read_failure),
		};
		let sent = sent.map_err(|error| judged(error, entry, verified))?;
		if sent == Taken::File
			&& let Some(stream) = entry.stream
		{
			verified |= proves(entry);
			if stream.crc.is_none() {
				unchecked_entries += 1;
			}
		}
	}
	walk.send_mac_folders().map_err(read_failure)?;
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
	job: &StreamJob,
) -> Result<ArchiveEnd, Error> {
	let checked = check_sevenz_password(cursor, index, keys)?;
	walk.port
		.send(WorkerEvent::Opened(ArchiveFormat::SevenZ))
		.map_err(read_failure)?;
	let within_limit = check_stated_size(job, index.entries.iter().map(|entry| entry.size)).is_ok();
	for entry in &index.entries {
		let cheap = entry
			.stream
			.is_some_and(|stream| stream.offset.saturating_add(entry.size) <= LIST_READ_BYTES);
		if !(within_limit && cheap) {
			walk.list(sevenz_unread(index, entry), None)?;
			continue;
		}
		// an encrypted link's data unread for a wrong or missing password leaves it listed as
		// a link without its target; the archive's damage and its source's failures end the
		// listing, as they end an extraction
		let found = match sevenz_found(cursor, index, entry, keys) {
			Ok((found, _)) => found,
			Err(error)
				if index.encrypted(entry)
					&& !source_failed(&error)
					&& (checked == PasswordCheck::Wrong || !keys.has_password()) =>
			{
				sevenz_unread(index, entry)
			}
			Err(error) => return Err(Error::from(error)),
		};
		// told as an extraction tells it, when metadata is left out
		let apple_double = if walk.skips_mac_metadata()
			&& found.unreadable.is_none()
			&& found.mac_shape() == Some(MacShape::AppleDoubleName)
		{
			sevenz_apple_double(cursor, index, entry, keys)?
		} else {
			None
		};
		walk.list(found, apple_double)?;
	}
	walk.send_mac_folders().map_err(read_failure)?;
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
	let (kind, unreadable) = match entry.kind {
		SevenZKind::Anti => (ArchiveEntryKind::Other, Some(ExtractSkipReason::AntiItem)),
		SevenZKind::Symlink => symlink(None),
		SevenZKind::Dir => (ArchiveEntryKind::Dir, None),
		_ if !index.supported(entry) => (
			ArchiveEntryKind::File,
			Some(ExtractSkipReason::UnsupportedMethod),
		),
		SevenZKind::Reparse | SevenZKind::File => (ArchiveEntryKind::File, None),
	};
	Found {
		ordinal: entry.ordinal,
		stored: &entry.name,
		path: found_path(&entry.name, entry.name_rewritten),
		kind,
		unreadable,
		size: entry.size,
		modified: entry.modified,
		encrypted: index.encrypted(entry),
		method: sevenz_method(index, entry),
	}
}

/// Whether a file entry that may be AppleDouble is, by its first bytes; `None` when its data
/// cannot be read (a wrong password), so it may be. Fails only on an error of the archive's
/// source.
fn sevenz_apple_double<'s, R: Read + std::io::Seek + 's>(
	cursor: &mut FolderCursor<'s, R>,
	index: &SevenZIndex,
	entry: &SevenZEntry,
	keys: &mut Keys<'_>,
) -> Result<Option<bool>, Error> {
	let read = match cursor.open(index, entry, keys) {
		Ok(mut data) => apple_double(&mut data).map(|(apple_double, _)| apple_double),
		Err(SevenZError::Read(error)) => Err(error),
		Err(_) => return Ok(None),
	};
	match read {
		Ok(apple_double) => Ok(Some(apple_double)),
		Err(error) if from_source(&error) => Err(read_failure(error)),
		Err(_) => Ok(None),
	}
}

/// A symlink entry's target, for reporting: its data, when small and readable; `None` when it
/// is not. Fails only on an error of the archive's source.
fn sevenz_symlink_target<'s, R: Read + std::io::Seek + 's>(
	cursor: &mut FolderCursor<'s, R>,
	index: &SevenZIndex,
	entry: &SevenZEntry,
	keys: &mut Keys<'_>,
) -> Result<Option<String>, SevenZError> {
	if entry.stream.is_none()
		|| !index.supported(entry)
		|| entry.size > MAX_ARCHIVE_PATH_BYTES as u64
	{
		return Ok(None);
	}
	let data = match cursor.open(index, entry, keys) {
		Ok(data) => Ok(data),
		Err(SevenZError::Read(error)) => Err(error),
		Err(_) => return Ok(None),
	};
	link_target(data).map_err(SevenZError::Read)
}

/// Whether reading an entry failed for the archive's source rather than the archive.
fn source_failed(error: &SevenZError) -> bool {
	matches!(error, SevenZError::Read(error) if from_source(error))
}
