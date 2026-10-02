//! What becomes of each entry a format reader finds, decided in one place for every format:
//! whether a partial extraction chose it, whether it is skipped (for its kind, its path, or as
//! macOS metadata), and what a listing says of it.

use std::{
	cmp::Reverse,
	io::{self, Read},
};

use chrono::{DateTime, Utc};
use filen_types::fs::Uuid;

use crate::{
	Error, ErrorKind,
	fs::{
		archive::{
			bytes::read_full,
			entry_path::{ArchivePath, PathRejection, entry_path},
			error::read_failure,
			extract::{
				ExtractSkipReason,
				download::EntryDownloadError,
				list::{ArchiveEntry, ArchiveEntryKind, EntryAccess, ListedPath, ListedSkipReason},
				report::ArchiveEntryId,
			},
			limits::display_path,
			worker::{EntryHead, EntryKind, SkippedMember, WorkerEvent, WorkerPort},
		},
		name::{ValidatedName, keep_both::collision_key},
	},
	util::SeededSet,
};

/// What the codec does with the entries it reads.
#[derive(Debug, Clone)]
pub(crate) enum Task {
	/// Sends the entries with their data: every one, or those a [`Selection`] chooses.
	Extract(Option<Selection>),
	/// Sends what every entry of the archive `archive` is, and none of their data.
	List { archive: Uuid },
	/// Sends one file entry with its data, as a partial extraction of it alone does, without
	/// checking the password on an entry up front: nothing is created that a wrong password
	/// would have to undo, and the entry proves the password as it is read. A 7z entry its
	/// solid block stores after more than `max_solid_skip` bytes of other files is refused
	/// before any of the block is read; `None` allows any.
	Download {
		selection: Selection,
		max_solid_skip: Option<u64>,
	},
}

/// The entries a partial extraction takes, and the directory they are extracted relative to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
	/// The chosen entries' ordinals, sorted, each once. A chosen directory brings everything
	/// below it.
	ordinals: Vec<u64>,
	/// The directory, as drive names, whose contents the chosen entries are extracted as: an
	/// entry at `base/x` lands at `x`. Empty for the archive's top.
	base: Vec<ValidatedName>,
}

impl Selection {
	/// The directory the chosen entries land relative to.
	pub(crate) fn base(&self) -> &[ValidatedName] {
		&self.base
	}

	pub(crate) fn new(ordinals: impl IntoIterator<Item = u64>, base: Vec<ValidatedName>) -> Self {
		let mut ordinals: Vec<u64> = ordinals.into_iter().collect();
		ordinals.sort_unstable();
		ordinals.dedup();
		Self { ordinals, base }
	}
}

/// The directory Finder's zips keep macOS metadata in, beside the files it belongs to.
const MAC_METADATA_DIR: &str = "__MACOSX";

/// What an AppleDouble file starts with: the resource fork and attributes macOS keeps for a
/// file `x`, stored beside it as `._x` where the file system cannot hold them. Its magic, then
/// the only version macOS writes: 8 bytes, so a file of the user's is not taken for one (and
/// left out) by 4 bytes that happen to match.
const APPLE_DOUBLE_HEAD: [u8; 8] = [0x00, 0x05, 0x16, 0x07, 0x00, 0x02, 0x00, 0x00];

/// How an entry's path marks it as macOS metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MacShape {
	/// A `__MACOSX` folder, or anything but a file or a hard link in one: its path says it all.
	/// A hard link is judged as any, and left out only when the file it copies is.
	InMacFolder,
	/// A file in a `__MACOSX` folder or named `._name`, which an ordinary file may be too: an
	/// AppleDouble file when its first bytes say so. A file is never left out by its path alone,
	/// so an archive that is removed once extracted takes no ordinary file with it.
	AppleDoubleName,
}

/// The `__MACOSX` folders an extraction leaving metadata out holds back until every entry was
/// judged: a folder that ends up holding nothing extracted is left out as metadata, but one
/// that holds a file of the user's is created by that file's path anyway, and an empty one may
/// be the user's own. Paths are told by [`prefix_digests`] of their collision keys.
///
/// The folders an entry is below are counted as directories are: past the member cap, the
/// archive is too large, before a crafted one (a million deep paths in `__MACOSX`) can fill
/// memory with them.
struct MacFolders {
	/// The member cap, which also caps the folders noted.
	max: u64,
	held: Vec<HeldMacFolder>,
	/// The folders (by digest) some entry judged is below. Seeded: the digests are unkeyed, so
	/// an archive could pick paths that collide in a fixed hasher.
	occupied: SeededSet<u128>,
	/// The folders something extracted is below.
	created: SeededSet<u128>,
}

struct HeldMacFolder {
	/// Its path as the archive stores it, which its digests are of; created at it less the
	/// base (see [`Walk::within_base`]).
	stored: ArchivePath,
	sent: HeldAs,
}

/// What a held folder is sent as once decided.
enum HeldAs {
	/// An extraction's: created, or left out as `skipped`.
	Extracted {
		skipped: SkippedMember,
		modified: Option<DateTime<Utc>>,
	},
	/// A listing's entry, its skip decided with the folder.
	Listed(Box<ArchiveEntry>),
}

impl MacFolders {
	fn new(max: u64) -> Self {
		Self {
			max,
			held: Vec::new(),
			occupied: SeededSet::default(),
			created: SeededSet::default(),
		}
	}

	/// Holds back folder `found`, which an extraction is about to create.
	fn hold(&mut self, found: &Found) {
		let Ok(stored) = &found.path else {
			return;
		};
		self.held.push(HeldMacFolder {
			// a copy: judging only borrows `found`
			stored: stored.clone(),
			sent: HeldAs::Extracted {
				skipped: found.skip_record(ExtractSkipReason::MacMetadata),
				modified: found.modified,
			},
		});
	}

	/// Notes an entry at `path` judged, and whether it is `created`, in the folders above it
	/// when they are in a `__MACOSX` folder. Created ones were noted judged first, so only
	/// those judged can pass the cap.
	fn below(
		&mut self,
		path: &Result<ArchivePath, PathRejection>,
		created: bool,
	) -> Result<(), Error> {
		let Ok(path) = path else {
			return Ok(());
		};
		if path
			.segments()
			.first()
			.is_none_or(|first| first.as_ref() != MAC_METADATA_DIR)
		{
			return Ok(());
		}
		let keys = collision_keys(path);
		let above = prefix_digests(&keys[..keys.len() - 1]);
		if created {
			self.created.extend(above);
			return Ok(());
		}
		self.occupied.extend(above);
		if self.occupied.len() as u64 > self.max {
			return Err(Error::custom(
				ErrorKind::ArchiveTooLarge,
				format!(
					"the archive's __MACOSX folders hold more than {} directories",
					self.max
				),
			));
		}
		Ok(())
	}
}

/// The collision keys of `path`'s segments.
fn collision_keys(path: &ArchivePath) -> Vec<String> {
	path.segments()
		.iter()
		.map(|segment| collision_key(segment.as_ref()))
		.collect()
}

/// The path of an entry stored at `stored` as drive names, `rewritten` already when its reader
/// had to change the name to read it.
pub(super) fn found_path(stored: &str, rewritten: bool) -> Result<ArchivePath, PathRejection> {
	entry_path(stored).map(|mut path| {
		path.rewritten |= rewritten;
		path
	})
}

/// What a symlink to `target`, as shown, is listed as, and why an extraction skips it. A target
/// not read (`None`) is shown empty.
pub(super) fn symlink(target: Option<String>) -> (ArchiveEntryKind, Option<ExtractSkipReason>) {
	let target = target.unwrap_or_default();
	(
		// both the kind and the skip show the target
		ArchiveEntryKind::Symlink {
			target: target.clone(),
		},
		Some(ExtractSkipReason::Symlink { target }),
	)
}

/// An entry as its format's reader tells of it.
pub(super) struct Found<'a> {
	pub(super) ordinal: u64,
	/// Its path as stored.
	pub(super) stored: &'a str,
	/// Its path as drive names, or why it has none.
	pub(super) path: Result<ArchivePath, PathRejection>,
	pub(super) kind: ArchiveEntryKind,
	/// Why its kind or data keeps it from being extracted, whatever its path.
	pub(super) unreadable: Option<ExtractSkipReason>,
	/// The bytes it holds, as the archive states them.
	pub(super) size: u64,
	pub(super) modified: Option<DateTime<Utc>>,
	pub(super) encrypted: bool,
	pub(super) method: Option<String>,
}

impl Found<'_> {
	fn is_dir(&self) -> bool {
		self.kind == ArchiveEntryKind::Dir
	}

	/// How its path marks it as macOS metadata, if it does.
	pub(super) fn mac_shape(&self) -> Option<MacShape> {
		let path = self.path.as_ref().ok()?;
		let in_mac_folder = path
			.segments()
			.first()
			.is_some_and(|first| first.as_ref() == MAC_METADATA_DIR);
		match self.kind {
			ArchiveEntryKind::File => {}
			ArchiveEntryKind::Hardlink { .. } => return None,
			_ => return in_mac_folder.then_some(MacShape::InMacFolder),
		}
		let apple_double = in_mac_folder || {
			let name: &str = path.split_last().0.as_ref();
			name.len() > 2 && name.starts_with("._")
		};
		apple_double.then_some(MacShape::AppleDoubleName)
	}

	/// The skip record for it.
	pub(super) fn skip_record(&self, reason: ExtractSkipReason) -> SkippedMember {
		let (path, path_truncated) = display_path(self.stored);
		SkippedMember {
			ordinal: self.ordinal,
			path: path.to_owned(),
			path_truncated,
			bytes: self.size,
			reason,
		}
	}

	/// It sent as skipped.
	pub(super) fn skipped(&self, reason: ExtractSkipReason) -> WorkerEvent {
		WorkerEvent::Skipped(self.skip_record(reason))
	}

	/// It sent taken at `path`, as `kind`.
	pub(super) fn head(&self, path: ArchivePath, kind: EntryKind) -> WorkerEvent {
		WorkerEvent::Entry(EntryHead {
			ordinal: self.ordinal,
			path,
			modified: self.modified,
			kind,
		})
	}
}

/// What a listing says an extraction does with an entry.
#[derive(Debug)]
pub(super) enum Listed {
	Extracted,
	Skipped(ListedSkipReason),
	/// Nothing: the directory is the root the others land in.
	Root,
	/// A folder in a `__MACOSX` folder, listed once every entry was
	/// ([`Walk::send_mac_folders`]).
	Held,
}

/// What an extraction does with an entry.
#[derive(Debug)]
pub(super) enum Verdict {
	/// Nothing: a partial extraction did not choose the entry, and nothing of it is read.
	Ignore,
	/// Nothing to create: the directory is the root the others land in.
	Root,
	Skip(ExtractSkipReason),
	/// A folder in a `__MACOSX` folder, or that folder, held back to be sent once every entry
	/// was judged ([`Walk::send_mac_folders`]): nothing to send now.
	Held,
	/// Extracted at `path`, below the extraction's root. `apple_double` when its name makes it
	/// one, which its first bytes decide (see [`apple_double`]).
	Take {
		path: ArchivePath,
		apple_double: bool,
	},
}

/// The one place the task, the selection and the macOS metadata option apply to the entries of
/// every format.
pub(super) struct Walk<'p> {
	pub(super) port: &'p WorkerPort,
	skip_mac_metadata: bool,
	mode: Mode,
	mac_folders: MacFolders,
}

/// What a [`Walk`] is for.
enum Mode {
	/// An extraction: of every entry, or of those a partial one chose.
	Extract { chooser: Option<Chooser> },
	/// A listing of `archive`.
	List { archive: Uuid },
	/// A download of the one file entry chosen (see [`Task::Download`]).
	Download {
		chooser: Chooser,
		max_solid_skip: Option<u64>,
	},
}

impl<'p> Walk<'p> {
	pub(super) fn new(
		port: &'p WorkerPort,
		task: &Task,
		skip_mac_metadata: bool,
		max_members: u64,
	) -> Self {
		let mode = match task {
			// a copy: the zip and 7z readers borrow the whole job the task is part of, so it
			// cannot be moved out, and choosing advances the chooser
			Task::Extract(selection) => Mode::Extract {
				chooser: selection.clone().map(Chooser::new),
			},
			Task::List { archive } => Mode::List { archive: *archive },
			Task::Download {
				selection,
				max_solid_skip,
			} => Mode::Download {
				chooser: Chooser::new(selection.clone()),
				max_solid_skip: *max_solid_skip,
			},
		};
		Self {
			port,
			skip_mac_metadata,
			mode,
			mac_folders: MacFolders::new(max_members),
		}
	}

	pub(super) fn listing(&self) -> bool {
		matches!(self.mode, Mode::List { .. })
	}

	/// What a partial extraction, or a download, chose.
	fn chooser(&self) -> Option<&Chooser> {
		match &self.mode {
			Mode::Extract { chooser } => chooser.as_ref(),
			Mode::Download { chooser, .. } => Some(chooser),
			Mode::List { .. } => None,
		}
	}

	fn chooser_mut(&mut self) -> Option<&mut Chooser> {
		match &mut self.mode {
			Mode::Extract { chooser } => chooser.as_mut(),
			Mode::Download { chooser, .. } => Some(chooser),
			Mode::List { .. } => None,
		}
	}

	/// Whether the password is checked on an entry up front, before any is sent: not for a
	/// download, whose one entry proves it as it is read.
	pub(super) fn probes_password(&self) -> bool {
		!matches!(self.mode, Mode::Download { .. })
	}

	/// Most a download's 7z solid block may decode and throw away before its entry; `None` when
	/// any skip is allowed, and for anything but a download.
	pub(super) fn max_solid_skip(&self) -> Option<u64> {
		match self.mode {
			Mode::Download { max_solid_skip, .. } => max_solid_skip,
			Mode::Extract { .. } | Mode::List { .. } => None,
		}
	}

	/// Whether entry `ordinal` was chosen (by a partial extraction or a download).
	pub(super) fn chose(&self, ordinal: u64) -> bool {
		self.chooser()
			.is_some_and(|chooser| chooser.chosen(ordinal))
	}

	/// Whether AppleDouble files are left out (see [`Verdict::Take`]).
	pub(super) fn skips_mac_metadata(&self) -> bool {
		self.skip_mac_metadata
	}

	/// The id of the listed archive's entry `ordinal`.
	pub(super) fn listed_id(&self, ordinal: u64) -> ArchiveEntryId {
		match self.mode {
			Mode::List { archive } => ArchiveEntryId::of(archive, ordinal),
			Mode::Extract { .. } | Mode::Download { .. } => unreachable!("only a listing lists"),
		}
	}

	/// Checks, before anything is created, a partial extraction of an archive whose entries are
	/// all known up front (a zip's or 7z's): that it holds every entry chosen, each below the
	/// base. Directories chosen are noted, so what is below one is chosen wherever it is stored;
	/// a download refuses one, as it takes a file alone.
	pub(super) fn check_selection<'e>(
		&mut self,
		entries: impl Iterator<Item = (u64, &'e str, bool)>,
	) -> Result<(), Error> {
		let downloading = matches!(self.mode, Mode::Download { .. });
		let Some(chooser) = self.chooser_mut() else {
			return Ok(());
		};
		let mut found = 0;
		for (ordinal, stored, is_dir) in entries {
			if !chooser.chosen(ordinal) {
				continue;
			}
			found += 1;
			// refused here rather than at its first event: what is below it would be sent first,
			// and a 7z would decode a solid block up to that
			if downloading && is_dir {
				return Err(EntryDownloadError::NotAFile.into());
			}
			if let Ok(path) = entry_path(stored) {
				let keys = chooser.below_base(&path)?;
				if is_dir {
					chooser.dirs.insert(path_digest(&keys));
				} else if keys.len() == chooser.base.len() {
					// a file where the base is: nothing to extract it into
					return Err(not_held());
				}
			}
		}
		if found < chooser.selection.ordinals.len() {
			return Err(not_held());
		}
		Ok(())
	}

	/// Whether an extraction takes entry `ordinal`, stored at `stored`, as [`Self::judge`] decides
	/// it: every entry, or those a partial one chose and what is below a directory it chose. For
	/// a zip or a 7z, once [`Self::check_selection`] noted the directories chosen; nothing is
	/// noted here.
	pub(super) fn takes(&self, ordinal: u64, stored: &str) -> bool {
		self.chooser()
			.is_none_or(|chooser| chooser.takes(ordinal, &entry_path(stored)))
	}

	/// What an extraction does with `found`.
	pub(super) fn judge(&mut self, found: &Found) -> Result<Verdict, Error> {
		let is_dir = found.is_dir();
		let path = match self.chooser_mut() {
			None => found.path.clone(),
			Some(chooser) => match chooser.choose(found.ordinal, &found.path, is_dir)? {
				Some(path) => path,
				None => return Ok(Verdict::Ignore),
			},
		};
		if let Some(reason) = &found.unreadable {
			return Ok(Verdict::Skip(reason.clone()));
		}
		let path = match path {
			Ok(path) => path,
			Err(PathRejection::Empty) if is_dir => return Ok(Verdict::Root),
			Err(rejection) => return Ok(Verdict::Skip(path_skip_reason(rejection))),
		};
		if self.skip_mac_metadata {
			self.mac_folders.below(&found.path, false)?;
		}
		let apple_double = match found.mac_shape() {
			Some(MacShape::InMacFolder) if self.skip_mac_metadata => {
				if is_dir {
					self.mac_folders.hold(found);
					return Ok(Verdict::Held);
				}
				return Ok(Verdict::Skip(ExtractSkipReason::MacMetadata));
			}
			Some(MacShape::AppleDoubleName) => self.skip_mac_metadata,
			_ => false,
		};
		if self.skip_mac_metadata && !apple_double {
			self.mac_folders.below(&found.path, true)?;
		}
		Ok(Verdict::Take { path, apple_double })
	}

	/// Notes `found`, which may have been AppleDouble, extracted after all: its data said it is
	/// not.
	pub(super) fn taken_after_all(&mut self, found: &Found) {
		if self.skip_mac_metadata {
			// judged first, so nothing new is noted
			let _ = self.mac_folders.below(&found.path, true);
		}
	}

	/// Once every entry was judged, sends the `__MACOSX` folders held back: each created (sent
	/// as a directory) when something below it was created, which creates it anyway, or when
	/// nothing is below it at all, as an empty folder of the user's is; left out as metadata
	/// only when everything below it was left out. A folder is reported as what became of it,
	/// and none is dropped unreported for an archive removed afterwards to take along. A
	/// listing sends each as the entry its extraction would be.
	pub(super) fn send_mac_folders(&mut self) -> io::Result<()> {
		let MacFolders {
			held,
			occupied,
			mut created,
			..
		} = std::mem::replace(&mut self.mac_folders, MacFolders::new(0));
		// deepest first: a folder created creates the ones above it too
		let mut deepest: Vec<usize> = (0..held.len()).collect();
		deepest.sort_unstable_by_key(|&at| Reverse(held[at].stored.segments().len()));
		let mut creates = vec![false; held.len()];
		for at in deepest {
			let prefixes: Vec<u128> = prefix_digests(&collision_keys(&held[at].stored)).collect();
			let own = prefixes.last().expect("a folder's path has a segment");
			if created.contains(own) || !occupied.contains(own) {
				creates[at] = true;
				created.extend(prefixes);
			}
		}
		for (HeldMacFolder { stored, sent }, create) in held.into_iter().zip(creates) {
			self.port.send(match sent {
				HeldAs::Extracted { skipped, modified } if create => {
					WorkerEvent::Entry(EntryHead {
						ordinal: skipped.ordinal,
						path: self
							.within_base(stored)
							.expect("a folder held is below the base"),
						modified,
						kind: EntryKind::Dir,
					})
				}
				HeldAs::Extracted { skipped, .. } => WorkerEvent::Skipped(skipped),
				HeldAs::Listed(mut entry) => {
					entry.skip = (!create).then_some(ListedSkipReason::MacMetadata);
					WorkerEvent::Listed(entry)
				}
			})?;
		}
		Ok(())
	}

	/// Once every entry was read: a partial extraction of a tar has met every entry it chose.
	pub(super) fn finish(&self) -> Result<(), Error> {
		match self.chooser() {
			Some(chooser) if chooser.met < chooser.selection.ordinals.len() => Err(not_held()),
			_ => Ok(()),
		}
	}

	/// Sends what a listing says of `found`; what an extraction does with it. `apple_double` is
	/// what its data told, when read; otherwise its name decides. `access` is what reading it
	/// alone costs, listed for a file only.
	pub(super) fn list(
		&mut self,
		found: Found,
		apple_double: Option<bool>,
		access: EntryAccess,
	) -> Result<Listed, Error> {
		if found.is_dir() && matches!(found.path, Err(PathRejection::Empty)) {
			// the archive's own root, which no extraction creates
			return Ok(Listed::Root);
		}
		// the `__MACOSX` folders are decided as an extraction decides them: noted as `judge`
		// notes an entry (a tar's hard link is taken there before it is resolved)
		let mut held = false;
		if self.skip_mac_metadata
			&& (found.unreadable.is_none()
				|| matches!(found.kind, ArchiveEntryKind::Hardlink { .. }))
		{
			self.mac_folders.below(&found.path, false)?;
			match found.mac_shape() {
				Some(MacShape::InMacFolder) => held = found.is_dir(),
				Some(MacShape::AppleDoubleName) if apple_double == Some(true) => {}
				_ => self.mac_folders.below(&found.path, true)?,
			}
		}
		// an extraction reads a file's data to tell whether it is AppleDouble: where a listing
		// did not, it marks the entry by its path, and does not say it is skipped
		let (mac_metadata, left_out) = match found.mac_shape() {
			Some(MacShape::InMacFolder) => (true, true),
			Some(MacShape::AppleDoubleName) => {
				(apple_double.unwrap_or(true), apple_double == Some(true))
			}
			// a hard link to metadata left out
			None => {
				let linked = found.unreadable == Some(ExtractSkipReason::MacMetadata);
				(linked, false)
			}
		};
		let skip = found
			.unreadable
			.as_ref()
			.map(ListedSkipReason::from)
			.or_else(|| {
				let rejection = *found.path.as_ref().err()?;
				Some((&path_skip_reason(rejection)).into())
			})
			.or((left_out && self.skip_mac_metadata).then_some(ListedSkipReason::MacMetadata));
		let (stored_path, stored_path_truncated) = display_path(found.stored);
		let path = found.path.as_ref().ok();
		let entry = ArchiveEntry {
			id: self.listed_id(found.ordinal),
			stored_path: stored_path.to_owned(),
			stored_path_truncated,
			path: path.map(|path| ListedPath {
				path: path.joined(),
				rewritten: path.rewritten,
				misleading: path.suspicious,
			}),
			size: (found.kind != ArchiveEntryKind::Dir).then_some(found.size),
			modified: found.modified,
			encrypted: found.encrypted,
			method: found.method,
			skip,
			mac_metadata,
			access: (found.kind == ArchiveEntryKind::File).then_some(access),
			kind: found.kind,
		};
		if held && let Ok(stored) = found.path {
			self.mac_folders.held.push(HeldMacFolder {
				stored,
				sent: HeldAs::Listed(Box::new(entry)),
			});
			return Ok(Listed::Held);
		}
		let listed = match &entry.skip {
			None => Listed::Extracted,
			Some(reason) => Listed::Skipped(*reason),
		};
		self.port
			.send(WorkerEvent::Listed(Box::new(entry)))
			.map_err(read_failure)?;
		Ok(listed)
	}

	/// The verdict on an entry `first` took into the job, once its data told more of it than its
	/// header did (a 7z link's target, or whether a reparse point is a link at all).
	pub(super) fn judge_again(&self, found: &Found, first: Verdict) -> Verdict {
		match (first, &found.unreadable) {
			(first @ (Verdict::Ignore | Verdict::Root | Verdict::Held), _) => first,
			// what its kind skips it for comes first, as in `judge`
			(_, Some(reason)) => Verdict::Skip(reason.clone()),
			(first, None) => first,
		}
	}

	/// `path`, of the file a hard link names, below the base: where the job extracts it; `None`
	/// when it is not below the base, so not extracted.
	pub(super) fn within_base(&self, path: ArchivePath) -> Option<ArchivePath> {
		let Some(chooser) = self.chooser() else {
			return Some(path);
		};
		chooser.below_base(&path).ok()?;
		path.below(chooser.base.len())
	}

	/// Bytes of the `files` (by ordinal, path as stored and size) the job extracts: every one
	/// with a usable path, or those a partial extraction chose. A file that may be macOS
	/// metadata counts: only its data tells.
	pub(super) fn extracted_bytes<'e>(
		&self,
		files: impl Iterator<Item = (u64, &'e str, u64)>,
	) -> u64 {
		// a copy: choosing advances a chooser, and the walk's own must stay unused for the walk
		let mut chooser = self.chooser().cloned();
		files
			.filter(|&(ordinal, stored, _)| {
				let path = entry_path(stored);
				match &mut chooser {
					None => path.is_ok(),
					Some(chooser) => chooser
						.choose(ordinal, &path, false)
						.is_ok_and(|path| path.is_some_and(|path| path.is_ok())),
				}
			})
			.fold(0u64, |total, (_, _, size)| total.saturating_add(size))
	}
}

/// Whether an entry that may be AppleDouble (see [`Verdict::Take`]) is one, by the first bytes
/// of its `data`; those bytes, which the data sent has to start with.
pub(super) fn apple_double(data: &mut dyn Read) -> io::Result<(bool, Vec<u8>)> {
	let mut head = vec![0u8; APPLE_DOUBLE_HEAD.len()];
	let read = read_full(data, &mut head)?;
	head.truncate(read);
	Ok((head == APPLE_DOUBLE_HEAD, head))
}

/// What a hard link's target is looked up by among the files extracted (or listed) before it:
/// its path, as the archive stores the file's (case and all), in the first 8 bytes of its
/// BLAKE3, so a million files cost 8 MB of keys rather than their paths. Two paths of a million
/// share a key about once in 36 billion archives, and then a link copies another file of the
/// same archive.
///
/// That holds only for paths the archive's author could not pick to collide: 8 bytes of a fixed
/// hash collide after about 2^32 tries, minutes of work, and would let a crafted tar's link copy
/// a file other than the one it names, or the listing show one. The hash is keyed with a key
/// drawn for each job (or listing), which the archive cannot know; one job keeps one key, so a
/// link and its target always meet.
pub(crate) struct LinkKeys([u8; blake3::KEY_LEN]);

impl LinkKeys {
	pub(crate) fn new() -> Self {
		Self(rand::random())
	}

	pub(crate) fn of(&self, path: &ArchivePath) -> u64 {
		let digest = blake3::keyed_hash(&self.0, path.joined().as_bytes());
		u64::from_le_bytes(digest.as_bytes()[..8].try_into().expect("8 bytes"))
	}
}

impl Default for LinkKeys {
	fn default() -> Self {
		Self::new()
	}
}

pub(super) fn path_skip_reason(rejection: PathRejection) -> ExtractSkipReason {
	match rejection {
		PathRejection::TooLong => ExtractSkipReason::PathTooLong,
		PathRejection::TooDeep => ExtractSkipReason::PathTooDeep,
		PathRejection::Unsafe | PathRejection::Empty => ExtractSkipReason::UnsafePath,
	}
}

fn not_held() -> Error {
	Error::custom(
		ErrorKind::InvalidState,
		"an entry chosen to extract is not in the archive, or not below the base",
	)
}

/// The digest of a path of collision keys, in 16 bytes.
fn path_digest(keys: &[String]) -> u128 {
	prefix_digests(keys).last().unwrap_or_default()
}

/// The digests of every path from the first of `keys` to each one in turn, shortest first:
/// hashed as one pass over them, so looking up every ancestor of a path costs its length.
fn prefix_digests(keys: &[String]) -> impl Iterator<Item = u128> {
	let mut hasher = blake3::Hasher::new();
	keys.iter().map(move |key| {
		// a length before each key keeps `a/bc` and `ab/c` apart
		hasher.update(&(key.len() as u64).to_le_bytes());
		hasher.update(key.as_bytes());
		u128::from_le_bytes(
			hasher.finalize().as_bytes()[..16]
				.try_into()
				.expect("16 bytes"),
		)
	})
}

/// Which entries a partial extraction takes, and where they land.
#[derive(Clone)]
struct Chooser {
	selection: Selection,
	/// The collision keys of the base's segments.
	base: Vec<String>,
	/// The digests ([`path_digest`]) of the directories chosen: what is below one is chosen
	/// too, told by looking up each of its ancestors.
	dirs: SeededSet<u128>,
	/// Chosen entries met so far.
	met: usize,
}

impl Chooser {
	fn new(selection: Selection) -> Self {
		Self {
			base: selection
				.base
				.iter()
				.map(|segment| collision_key(segment.as_ref()))
				.collect(),
			selection,
			dirs: SeededSet::default(),
			met: 0,
		}
	}

	/// The collision keys of `path`, which has to be below the base: two spellings of a directory
	/// that differ only in case are one directory to an extraction.
	fn below_base(&self, path: &ArchivePath) -> Result<Vec<String>, Error> {
		let keys = collision_keys(path);
		if keys.starts_with(&self.base) {
			Ok(keys)
		} else {
			Err(not_held())
		}
	}

	/// Whether entry `ordinal` was chosen.
	fn chosen(&self, ordinal: u64) -> bool {
		self.selection.ordinals.binary_search(&ordinal).is_ok()
	}

	/// Whether the path of collision `keys` is a directory chosen, or below one.
	fn in_chosen_dir(&self, keys: &[String]) -> bool {
		prefix_digests(keys).any(|digest| self.dirs.contains(&digest))
	}

	/// Whether [`Self::choose`] takes entry `ordinal`, at `path` (or fails the job on it), by
	/// the directories chosen so far, which it does not note.
	fn takes(&self, ordinal: u64, path: &Result<ArchivePath, PathRejection>) -> bool {
		self.chosen(ordinal)
			|| path
				.as_ref()
				.is_ok_and(|path| self.in_chosen_dir(&collision_keys(path)))
	}

	/// The path below the base that entry `ordinal`, at `path`, is extracted at when it was
	/// chosen or is below a directory that was; `None` when it is not extracted.
	fn choose(
		&mut self,
		ordinal: u64,
		path: &Result<ArchivePath, PathRejection>,
		is_dir: bool,
	) -> Result<Option<Result<ArchivePath, PathRejection>>, Error> {
		let chosen = self.chosen(ordinal);
		if chosen {
			self.met += 1;
		}
		let path = match path {
			Ok(path) => path,
			// chosen, it is skipped for its path; otherwise nothing tells where it would be
			Err(rejection) => return Ok(chosen.then_some(Err(*rejection))),
		};
		let keys = collision_keys(path);
		let in_chosen_dir = self.in_chosen_dir(&keys);
		if !chosen && !in_chosen_dir {
			return Ok(None);
		}
		let keys = self.below_base(path)?;
		let at_base = keys.len() == self.base.len();
		if chosen && is_dir && !in_chosen_dir {
			self.dirs.insert(path_digest(&keys));
		}
		if at_base {
			// the base itself, which the entries below it land in
			return if is_dir {
				Ok(Some(Err(PathRejection::Empty)))
			} else {
				Err(not_held())
			};
		}
		let below = path
			.below(self.base.len())
			.expect("a path longer than the base it starts with");
		Ok(Some(Ok(below)))
	}
}
