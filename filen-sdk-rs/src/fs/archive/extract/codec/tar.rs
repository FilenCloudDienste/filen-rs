//! Reading a tar, bare or decoded from a compressed stream, member by member.

use std::{
	cell::Cell,
	io::{self, Read},
	num::NonZeroU64,
	rc::Rc,
};

use chrono::DateTime;

use crate::{
	Error,
	fs::{
		archive::{
			entry_path::{ArchivePath, PathRejection, entry_path},
			error::read_failure,
			extract::{
				ExpansionLimit, ExtractSkipReason, SolidSkipExceeded,
				list::{ArchiveEntryKind, EntryAccess, ListedSkipReason},
			},
			limits::display_path,
			tar_iter::{MemberKind, TarError, TarMember, TarReader, TarSource},
			worker::{EntryKind, LinkHead, WorkerEvent},
		},
		drive_job::exceeds_limit,
	},
	util::SeededMap,
};

use super::{
	ExpansionExceeded, Taken, TarSpan,
	entries::{
		Found, LinkKeys, Listed, MacShape, Verdict, Walk, apple_double, found_path, symlink,
	},
	take_file,
};

/// What [`walk_tar`] leaves.
pub(super) struct Walked<R> {
	/// What follows the end-of-archive marker.
	pub(super) rest: R,
	/// Bytes stored under directory members, which nothing extracts.
	pub(super) unread: u64,
	/// Files sent.
	pub(super) files: u64,
	/// Where each member's data lies, by ordinal, when a listing was asked to record it.
	pub(super) spans: Vec<TarSpan>,
	/// It ended at a download's last entry, before the end of the tar: `rest` is not after
	/// the end marker.
	pub(super) stopped: bool,
}

/// Sends every member of the tar `tar` reads, or what each is when listing, recording where each
/// one's data lies when `record`. A listing lists a member from its header: a bare tar's at once,
/// a compressed tar's file sent as [`WorkerEvent::Reading`] while its data is decoded, then listed
/// once that data was passed, which tells what reaching it fetches.
pub(super) fn walk_tar<R: TarSource>(
	walk: &mut Walk,
	mut tar: TarReader<R>,
	record: bool,
	skip_limit: Option<SkipLimit>,
	stored: Stored,
) -> Result<Walked<R>, Error> {
	let mut spans = Vec::new();
	// data of members passed over, which a compressed tar decodes all the same
	let mut thrown = 0u64;
	let mut pass_over = |size: u64| -> Result<(), Error> {
		let Some(SkipLimit { limit, archive_len }) = skip_limit else {
			return Ok(());
		};
		thrown = thrown.saturating_add(size);
		if exceeds_limit(thrown, limit) {
			return Err(SolidSkipExceeded {
				skipped_bytes: thrown,
				limit,
				estimated_packed_bytes: archive_len,
			}
			.into());
		}
		Ok(())
	};
	let mut ordinal = 0;
	let mut unread = 0u64;
	let mut files = 0u64;
	// an extraction's driver resolves hard links against the files it created
	let mut listed_files = SeededMap::default();
	let mut shadowed = Shadowed::default();
	let mut stopped = false;
	// a listing sent the member before as `Reading`: listed once its data was passed
	let mut reading = false;
	loop {
		// a download ends with its last entry, the rest of the tar unread
		if walk.met_all_chosen() {
			stopped = true;
			break;
		}
		if reading {
			// passed only once all of its data was there: one that ends early is not listed
			tar.skip_rest().map_err(Error::from)?;
		}
		let next = tar.next_member().map_err(Error::from);
		// what the decoder had taken once past the member's data
		let mut passed = match stored {
			Stored::Compressed(compressed) if reading => Some(compressed.taken()),
			_ => None,
		};
		reading = false;
		// the member before is whole once its data was passed, even when the header after it is
		// damaged: listed before that ends the listing
		if !matches!(next, Ok(Some(_))) {
			send_passed(walk, &mut passed)?;
		}
		let Some(member) = next? else {
			break;
		};
		let this = ordinal;
		ordinal += 1;
		if member.kind == MemberKind::Dir {
			unread = unread.saturating_add(member.size);
		}
		let found = member_found(&member, this);
		// a hard link names an earlier file, the same whatever else it is stored with
		let link_target = match (&member.kind, &found.kind) {
			(MemberKind::Hardlink { target }, ArchiveEntryKind::Hardlink { target: shown, .. }) => {
				// a copy: `found` keeps the target it shows
				Some((entry_path(target), shown.clone()))
			}
			_ => None,
		};
		if walk.listing() {
			if record {
				let holds_data = match member.kind {
					MemberKind::File => true,
					MemberKind::Hardlink { .. } => member.size > 0,
					_ => false,
				};
				spans.push(TarSpan {
					data: NonZeroU64::new(member.data_offset).filter(|_| holds_data),
					size: member.size,
				});
			}
			drop(found);
			let listed = list_member(
				walk,
				&mut tar,
				&member,
				Heard {
					ordinal: this,
					link_target,
					passed: &mut passed,
				},
				stored,
				&mut listed_files,
				&mut shadowed,
			);
			// the member before is whole all the same: listed before what failed with this one
			// ends the listing
			if listed.is_err() {
				send_passed(walk, &mut passed)?;
			}
			reading = listed?;
			continue;
		}
		let key = shadowed.key(&found.path);
		let (path, apple_double) = match walk.judge(&found)? {
			Verdict::Ignore | Verdict::Root | Verdict::Held => {
				shadowed.note(key, Some(Shadow::Other));
				pass_over(member.size)?;
				continue;
			}
			Verdict::Skip(reason) => {
				pass_over(member.size)?;
				shadowed.note(key, Some(Shadow::of((&reason).into())));
				walk.port
					.send(found.skipped(reason))
					.map_err(read_failure)?;
				continue;
			}
			Verdict::Take { path, apple_double } => (path, apple_double),
		};
		if let Some(target) = link_target {
			let (event, shadow) = link_event(walk, &found, path, target, &shadowed);
			shadowed.note(key, shadow);
			walk.port.send(event).map_err(read_failure)?;
			continue;
		}
		if found.kind == ArchiveEntryKind::Dir {
			shadowed.note(key, Some(Shadow::Other));
			walk.port
				.send(found.head(path, EntryKind::Dir))
				.map_err(read_failure)?;
			continue;
		}
		let taken = take_file(
			walk,
			&found,
			path,
			Some(found.size),
			apple_double,
			&mut TarBody(&mut tar),
		)
		.map_err(read_failure)?;
		// a file sent is taken, or left out as metadata
		shadowed.note(
			key,
			(taken == Taken::LeftOut).then_some(Shadow::MacMetadata),
		);
		files += taken.files();
	}
	walk.send_mac_folders().map_err(read_failure)?;
	walk.finish()?;
	Ok(Walked {
		rest: tar.into_inner(),
		unread,
		files,
		spans,
		stopped,
	})
}

/// How much a download may decode and throw away of a compressed tar to reach its entries
/// ([`Walk::max_solid_skip`]), and the archive's length, which estimates what that fetches.
#[derive(Clone, Copy)]
pub(super) struct SkipLimit {
	pub(super) limit: u64,
	pub(super) archive_len: u64,
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
		MemberKind::Symlink { target } => symlink(Some(display_path(target).0.to_owned())),
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
		path: found_path(&member.path, member.path_rewritten),
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
/// resolved included, which later links may name), by [`Shadowed::key`] of their paths, with
/// their sizes and ordinals.
type ListedFiles = SeededMap<u64, (u64, u64)>;

/// The paths, by [`LinkKeys`], whose last member so far is not extracted as a file: left out,
/// skipped, a directory, or not chosen. A hard link names the last member at its target's path,
/// so one of these keeps a link from copying an earlier file stored there, which the driver
/// (or a listing's [`ListedFiles`]) still holds under the path. A file extracted at a path
/// takes it back.
#[derive(Default)]
struct Shadowed {
	keys: LinkKeys,
	by_key: SeededMap<u64, Shadow>,
}

/// What stands at a [`Shadowed`] path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shadow {
	/// A file left out as macOS metadata, or a hard link to one: a link to it is left out as
	/// metadata too, rather than skipped for want of its file, which would keep the archive
	/// from being removed.
	MacMetadata,
	/// Anything else not extracted as a file.
	Other,
}

impl Shadow {
	/// What a member skipped for `reason` leaves at its path.
	fn of(reason: ListedSkipReason) -> Self {
		if reason == ListedSkipReason::MacMetadata {
			Self::MacMetadata
		} else {
			Self::Other
		}
	}

	/// Why a hard link to a path standing so, shown as `shown`, is skipped.
	fn link_skip(self, shown: String) -> ExtractSkipReason {
		match self {
			Self::MacMetadata => ExtractSkipReason::MacMetadata,
			Self::Other => ExtractSkipReason::Hardlink { target: shown },
		}
	}
}

impl Shadowed {
	/// What `path` is looked up by, here and in [`ListedFiles`].
	fn key(&self, path: &Result<ArchivePath, PathRejection>) -> Option<u64> {
		path.as_ref().ok().map(|path| self.keys.of(path))
	}

	/// The member at `key` is the last one there so far: extracted as a file (`None`), or
	/// standing as `shadow`.
	fn note(&mut self, key: Option<u64>, shadow: Option<Shadow>) {
		let Some(key) = key else {
			return;
		};
		match shadow {
			Some(shadow) => {
				self.by_key.insert(key, shadow);
			}
			None => {
				self.by_key.remove(&key);
			}
		}
	}

	/// What stands at `target`, when it is no file extracted.
	fn at(&self, target: &Result<ArchivePath, PathRejection>) -> Option<Shadow> {
		self.key(target)
			.and_then(|key| self.by_key.get(&key).copied())
	}
}

/// How a tar is stored, which a listing tells a member's cost by and checks its header against.
#[derive(Clone, Copy)]
pub(super) enum Stored<'c> {
	/// As it is, in an archive of `len` bytes.
	Bare {
		len: u64,
	},
	Compressed(&'c Compressed),
}

/// A compressed tar's archive bytes, and how many of them its decoder has taken so far.
pub(super) struct Compressed {
	pub(super) len: u64,
	pub(super) taken: Rc<Cell<u64>>,
	/// The expansion limit, when there is one, and the bytes it counts what the tar decodes to
	/// against (see [`CodecLimits::expansion_base`](super::CodecLimits::expansion_base)).
	pub(super) expansion: Option<(ExpansionLimit, u64)>,
}

impl Compressed {
	/// The archive bytes the decoder has taken so far.
	fn taken(&self) -> u64 {
		self.taken.get().min(self.len)
	}
}

/// A compressed tar's stored bytes, counting those its decoder takes.
pub(super) struct Counted<R> {
	pub(super) inner: R,
	pub(super) taken: Rc<Cell<u64>>,
}

impl<R: Read> Read for Counted<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let read = self.inner.read(buf)?;
		self.taken.set(self.taken.get().saturating_add(read as u64));
		Ok(read)
	}
}

/// What a listing knows of a member besides its header.
struct Heard<'p> {
	ordinal: u64,
	link_target: Option<HardlinkTarget>,
	/// What the decoder had taken once past the data of the member sent as
	/// [`WorkerEvent::Reading`] before it, which is listed first: taken once it was sent.
	passed: &'p mut Option<u64>,
}

/// Lists the member sent as [`WorkerEvent::Reading`] last, whose data was `passed`, unless that
/// was told already.
fn send_passed(walk: &Walk, passed: &mut Option<u64>) -> Result<(), Error> {
	match passed.take() {
		Some(taken) => walk
			.port
			.send(WorkerEvent::Passed(taken))
			.map_err(read_failure),
		None => Ok(()),
	}
}

/// Lists `member` from its header, `tar` at its data, a hard link resolved against
/// `listed_files`, the files listed before it as extracted, which it joins when it is one. A bare
/// tar's member is listed at once, once its data is known to lie within the archive. A
/// `Compressed` tar's file that stores data is sent as [`WorkerEvent::Reading`], and listed
/// once the walk passed that data. A file's cost is what the tar stores before its data, decoded
/// and thrown away, and the archive bytes the decoder will have taken once past its data, which
/// approximates what a read of it alone fetches (see [`EntryAccess::SolidBlock`]'s
/// `estimated_packed_bytes`) and is told once passed. Whether it was sent as read.
fn list_member<R: TarSource>(
	walk: &mut Walk,
	tar: &mut TarReader<R>,
	member: &TarMember,
	heard: Heard,
	stored: Stored,
	listed_files: &mut ListedFiles,
	shadowed: &mut Shadowed,
) -> Result<bool, Error> {
	let Heard {
		ordinal,
		link_target,
		passed,
	} = heard;
	let mut found = member_found(member, ordinal);
	// a listing reads a tar through: an AppleDouble member is told by its data here
	let apple_double = match found.mac_shape() {
		Some(MacShape::AppleDoubleName) => {
			Some(apple_double(&mut TarBody(tar)).map_err(read_failure)?.0)
		}
		_ => None,
	};
	if let Some((target, shown)) = link_target {
		let linked = shadowed.key(&target).and_then(|key| listed_files.get(&key));
		match (shadowed.at(&target), linked) {
			(Some(shadow), _) => found.unreadable = Some(shadow.link_skip(shown)),
			(None, Some(&(size, target))) => {
				found.size = size;
				found.kind = ArchiveEntryKind::Hardlink {
					target: shown,
					target_id: Some(walk.listed_id(target)),
				};
			}
			(None, None) => {
				found.unreadable = Some(ExtractSkipReason::Hardlink { target: shown });
			}
		}
	}
	let costed: Result<_, Error> = match stored {
		// a header stating more data than the archive holds is not listed: passing it fails
		Stored::Bare { len } if member.stored_end().is_none_or(|end| end > len) => {
			Err(TarError::Corrupt("a member's data ends early").into())
		}
		Stored::Bare { .. } => Ok((EntryAccess::Sequential, false)),
		// nor one stating more than the tar may decode to, which decoding it would fail on
		Stored::Compressed(Compressed {
			expansion: Some((limit, base)),
			..
		}) if member
			.data_offset
			.checked_add(member.size)
			.is_none_or(|end| end > limit.max_decoded(*base)) =>
		{
			Err(ExpansionExceeded(limit.ratio).into())
		}
		Stored::Compressed(compressed) => {
			let reading = found.kind == ArchiveEntryKind::File && member.size > 0;
			// a file without data is passed already
			let estimated_packed_bytes = if reading {
				compressed.len
			} else {
				compressed.taken()
			};
			let access = EntryAccess::SolidBlock {
				skipped_bytes: member.data_offset,
				estimated_packed_bytes,
				block_packed_bytes: compressed.len,
			};
			Ok((access, reading))
		}
	};
	let (access, reading) = costed?;
	// the member before, unless this one tells it with its own event
	if !reading {
		send_passed(walk, passed)?;
	}
	let key = shadowed.key(&found.path);
	let size = found.size;
	let is_dir = found.kind == ArchiveEntryKind::Dir;
	let listed = if reading {
		walk.list_as(found, apple_double, access, |entry| WorkerEvent::Reading {
			entry,
			passed: passed.take(),
		})?
	} else {
		walk.list(found, apple_double, access)?
	};
	// a directory held back, or the root, sent nothing that told the member before
	send_passed(walk, passed)?;
	let shadow = match listed {
		_ if is_dir => Some(Shadow::Other),
		Listed::Extracted => {
			if let Some(key) = key {
				listed_files.insert(key, (size, ordinal));
			}
			None
		}
		Listed::Skipped(reason) => Some(Shadow::of(reason)),
		Listed::Root | Listed::Held => Some(Shadow::Other),
	};
	shadowed.note(key, shadow);
	// only a directory is held back, or is the root
	Ok(reading && matches!(listed, Listed::Extracted | Listed::Skipped(_)))
}

/// What the driver is sent for `member`, a hard link taken at `path`: the link to the file it
/// names, where this job extracts it, or the link skipped when this job extracts none there, or
/// the last member at the target's path is no file extracted; and what the link leaves at its
/// own path.
fn link_event(
	walk: &Walk,
	found: &Found,
	path: ArchivePath,
	(target, shown): HardlinkTarget,
	shadowed: &Shadowed,
) -> (WorkerEvent, Option<Shadow>) {
	// a copy of metadata left out is metadata left out, whatever path the link is at
	let shadow = shadowed.at(&target);
	// a link holds no data of its own: its size is 0
	let unresolved = found.skip_record(shadow.unwrap_or(Shadow::Other).link_skip(shown));
	let target = match shadow {
		Some(_) => None,
		None => target.ok().and_then(|target| walk.within_base(target)),
	};
	match target {
		Some(target) => (
			WorkerEvent::Link(Box::new(LinkHead {
				path,
				modified: found.modified,
				target,
				unresolved,
			})),
			None,
		),
		None => (
			WorkerEvent::Skipped(unresolved),
			Some(shadow.unwrap_or(Shadow::Other)),
		),
	}
}

/// The current member's data.
struct TarBody<'t, R>(&'t mut TarReader<R>);

impl<R: TarSource> Read for TarBody<'_, R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		self.0.read_body(buf).map_err(|error| match error {
			TarError::Read(error) => error,
			error => io::Error::new(io::ErrorKind::InvalidData, error),
		})
	}
}

#[cfg(test)]
mod tests {
	use super::{ListedFiles, Shadow, Shadowed};
	use crate::alloc_meter;

	#[test]
	fn a_tar_listings_link_bookkeeping_takes_at_most_96_bytes_a_member() {
		// the bound ArchiveConfig::max_members states
		const BYTES_A_MEMBER: u64 = 96;
		const MEMBERS: u64 = 1_000_000;
		// every member a file a later link may name, or every one standing at its path
		for listed in [true, false] {
			let ((), peak) = alloc_meter::peak_bytes(|| {
				let mut files = ListedFiles::default();
				let mut shadowed = Shadowed::default();
				for ordinal in 0..MEMBERS {
					// as spread as a hash's first bytes
					let key = ordinal.wrapping_mul(0x9E37_79B9_7F4A_7C15);
					if listed {
						files.insert(key, (ordinal, ordinal));
						shadowed.note(Some(key), None);
					} else {
						shadowed.note(Some(key), Some(Shadow::Other));
					}
				}
			});
			assert!(
				peak <= MEMBERS * BYTES_A_MEMBER,
				"listed {listed}: {} bytes a member",
				peak / MEMBERS
			);
		}
	}
}
