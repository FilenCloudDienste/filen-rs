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
	util::{SeededMap, SeededSet},
};

use super::{
	super::{ExtractSkipReason, list::ArchiveEntryKind},
	Refused,
	entries::{Found, LinkKeys, MacShape, Verdict, Walk, apple_double},
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
	// an extraction's driver resolves hard links against the files it created
	let mut listed_files = ListedFiles::default();
	let mut left_out = LeftOut::default();
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
			list_member(
				walk,
				&mut tar,
				found,
				link_target,
				&mut listed_files,
				&mut left_out,
			)?;
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
			let event = link_event(walk, &member, &found, path, target, &left_out);
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
		let sent = take_file(
			walk,
			&found,
			path,
			Some(found.size),
			apple_double,
			&mut TarBody(&mut tar),
		)
		.map_err(failure)?;
		left_out.note(&found.path, sent == 0);
		files += sent;
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

/// What a listing resolves hard links against: the files it says are extracted (hard links
/// resolved included, which later links may name), by [`LinkKeys`] of their paths, with their
/// sizes and ordinals.
#[derive(Default)]
struct ListedFiles {
	keys: LinkKeys,
	by_key: SeededMap<u64, (u64, u64)>,
}

/// The files left out as macOS metadata, by [`LinkKeys`] of their paths: a hard link to one is
/// left out as metadata too, rather than skipped for want of its file, which would keep the
/// archive from being removed. Only a tar has hard links, and only AppleDouble files are noted,
/// so this holds next to nothing.
#[derive(Default)]
struct LeftOut {
	keys: LinkKeys,
	files: SeededSet<u64>,
}

impl LeftOut {
	/// A file at `path` was left out as metadata, or extracted: the last file at a path is the
	/// one a link names.
	fn note(&mut self, path: &Result<ArchivePath, PathRejection>, left_out: bool) {
		let Ok(path) = path else {
			return;
		};
		if left_out {
			self.files.insert(self.keys.of(path));
		} else if !self.files.is_empty() {
			self.files.remove(&self.keys.of(path));
		}
	}

	/// Whether the last file at `path` was left out as metadata.
	fn holds(&self, path: &Result<ArchivePath, PathRejection>) -> bool {
		!self.files.is_empty()
			&& path
				.as_ref()
				.is_ok_and(|path| self.files.contains(&self.keys.of(path)))
	}
}

/// Lists the member `found`, a hard link to `link_target` resolved against `listed_files`, the
/// files listed before it as extracted, which it joins when it is one.
fn list_member<R: Read>(
	walk: &mut Walk,
	tar: &mut TarReader<R>,
	mut found: Found,
	link_target: Option<HardlinkTarget>,
	listed_files: &mut ListedFiles,
	left_out: &mut LeftOut,
) -> Result<(), Error> {
	if let Some((target, shown)) = link_target {
		let linked = target
			.as_ref()
			.ok()
			.and_then(|target| listed_files.by_key.get(&listed_files.keys.of(target)));
		match linked {
			Some(&(size, target)) => {
				found.size = size;
				found.kind = ArchiveEntryKind::Hardlink {
					target: shown,
					target_id: Some(walk.listed_id(target)),
				};
			}
			None if left_out.holds(&target) => {
				found.unreadable = Some(ExtractSkipReason::MacMetadata);
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
	let key = found
		.path
		.as_ref()
		.ok()
		.map(|path| listed_files.keys.of(path));
	let size = found.size;
	let is_file = found.kind == ArchiveEntryKind::File;
	if is_file {
		left_out.note(
			&found.path,
			apple_double == Some(true) && walk.skips_mac_metadata(),
		);
	}
	let is_dir = found.kind == ArchiveEntryKind::Dir;
	if walk.list(found, apple_double).map_err(failure)?
		&& !is_dir
		&& let Some(key) = key
	{
		listed_files.by_key.insert(key, (size, ordinal));
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
	left_out: &LeftOut,
) -> WorkerEvent {
	// a copy of metadata left out is metadata left out, whatever path the link is at
	let reason = if left_out.holds(&target) {
		ExtractSkipReason::MacMetadata
	} else {
		ExtractSkipReason::Hardlink { target: shown }
	};
	let unresolved = SkippedMember {
		ordinal: found.ordinal,
		path: display_path(&member.path).0.to_owned(),
		path_truncated: display_path(&member.path).1,
		bytes: 0,
		reason,
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
