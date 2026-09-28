//! Reading a tar, bare or decoded from a compressed stream, member by member.

use std::io::{self, Read};

use chrono::DateTime;

use crate::{
	Error,
	fs::archive::{
		entry_path::{ArchivePath, PathRejection, entry_path},
		limits::display_path,
		tar_iter::{MemberKind, TarError, TarMember, TarReader},
		worker::{EntryHead, EntryKind, LinkHead, SkippedMember, WorkerEvent},
	},
	util::SeededMap,
};

use super::{
	super::{ExtractSkipReason, list::ArchiveEntryKind},
	Refused,
	entries::{Found, MacShape, Verdict, Walk, apple_double, link_key},
	failure, take_file,
};

/// What [`walk_tar`] leaves.
pub(super) struct Walked<R> {
	/// What follows the end-of-archive marker.
	pub(super) rest: R,
	/// Bytes stored under directory members, which nothing extracts.
	pub(super) unread: u64,
	/// Files sent.
	pub(super) files: u64,
}

/// Sends every member of the tar in `reader`, or what each is when listing.
pub(super) fn walk_tar<R: Read>(
	walk: &mut Walk,
	reader: R,
	max_members: u64,
) -> Result<Walked<R>, Error> {
	let mut tar = TarReader::new(reader, max_members);
	let mut ordinal = 0;
	let mut unread = 0u64;
	let mut files = 0u64;
	// what a listing resolves hard links against: the files it says are extracted (hard links
	// resolved included, which later links may name), by path, with their sizes and ordinals. An
	// extraction's driver resolves them against the files it created
	let mut listed_files = SeededMap::<u64, (u64, u64)>::default();
	while let Some(member) = tar.next_member().map_err(tar_failure)? {
		let this = ordinal;
		ordinal += 1;
		if member.kind == MemberKind::Dir {
			unread = unread.saturating_add(member.size);
		}
		let found = member_found(&member, this);
		// a hard link names an earlier file, the same whatever else it is stored with
		let link_target = match (&member.kind, &found.kind) {
			(MemberKind::Hardlink { target }, ArchiveEntryKind::Hardlink { target: shown, .. }) => {
				Some((entry_path(target), shown.clone()))
			}
			_ => None,
		};
		if walk.listing() {
			list_member(walk, &mut tar, found, link_target, &mut listed_files)?;
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
		if let Some(target) = link_target {
			let event = link_event(walk, &member, &found, path, target);
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

/// The file a hard link names, as a path, and as its listing shows it.
type HardlinkTarget = (Result<ArchivePath, PathRejection>, String);

/// What `member`, the `ordinal`-th of its tar, is, before anything is read of it but its header.
fn member_found(member: &TarMember, ordinal: u64) -> Found<'_> {
	let (kind, unreadable) = match &member.kind {
		MemberKind::File => (ArchiveEntryKind::File, None),
		// a hard link with data of its own holds the file, as for libarchive
		MemberKind::Hardlink { .. } if member.size > 0 => (ArchiveEntryKind::File, None),
		MemberKind::Dir => (ArchiveEntryKind::Dir, None),
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
				target_id: None,
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
	Found {
		ordinal,
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
	}
}

/// Lists the member `found`, a hard link to `link_target` resolved against `listed_files`, the
/// files listed before it as extracted, which it joins when it is one.
fn list_member<R: Read>(
	walk: &mut Walk,
	tar: &mut TarReader<R>,
	mut found: Found,
	link_target: Option<HardlinkTarget>,
	listed_files: &mut SeededMap<u64, (u64, u64)>,
) -> Result<(), Error> {
	if let Some((target, shown)) = link_target {
		match target
			.ok()
			.and_then(|target| listed_files.get(&link_key(&target)))
		{
			Some(&(size, target)) => {
				found.size = size;
				found.kind = ArchiveEntryKind::Hardlink {
					target: shown,
					target_id: Some(walk.listed_id(target)),
				};
			}
			None => {
				found.unreadable = Some(ExtractSkipReason::Hardlink { target: shown });
			}
		}
	}
	// a listing reads a tar through: an AppleDouble member is told by its data here
	let apple_double = match found.mac_shape() {
		Some(MacShape::AppleDoubleName) => {
			Some(apple_double(&mut TarBody(tar)).map_err(failure)?.0)
		}
		_ => None,
	};
	let ordinal = found.ordinal;
	let key = found.path.as_ref().ok().map(link_key);
	let size = found.size;
	let is_dir = found.kind == ArchiveEntryKind::Dir;
	if walk.list(found, apple_double).map_err(failure)?
		&& !is_dir
		&& let Some(key) = key
	{
		listed_files.insert(key, (size, ordinal));
	}
	Ok(())
}

/// What the driver is sent for `member`, a hard link taken at `path`: the link to the file it
/// names, where this job extracts it, or the link skipped when this job extracts none there.
fn link_event(
	walk: &Walk,
	member: &TarMember,
	found: &Found,
	path: ArchivePath,
	(target, shown): HardlinkTarget,
) -> WorkerEvent {
	let unresolved = SkippedMember {
		ordinal: found.ordinal,
		path: display_path(&member.path).0.to_owned(),
		path_truncated: display_path(&member.path).1,
		bytes: 0,
		reason: ExtractSkipReason::Hardlink { target: shown },
	};
	match target.ok().and_then(|target| walk.within_base(target)) {
		Some(target) => WorkerEvent::Link(Box::new(LinkHead {
			ordinal: found.ordinal,
			path,
			modified: found.modified,
			target,
			unresolved,
		})),
		None => WorkerEvent::Skipped(unresolved),
	}
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
		Ok(refused) => super::refused(refused),
		Err(error) => failure(error),
	}
}
