//! What becomes of each entry a format reader finds, decided in one place for every format:
//! whether a partial extraction chose it, whether it is skipped (for its kind, its path, or as
//! macOS metadata), and what a listing says of it.

use std::{
	collections::HashSet,
	io::{self, Read},
};

use chrono::{DateTime, Utc};
use filen_types::fs::Uuid;

use crate::{
	Error, ErrorKind,
	fs::{
		archive::{
			entry_path::{ArchivePath, PathRejection, entry_path},
			limits::display_path,
			worker::{SkippedMember, WorkerEvent, WorkerPort, read_full},
		},
		name::{ValidatedName, keep_both::collision_key},
	},
};

use super::super::{
	ExtractSkipReason,
	list::{ArchiveEntry, ArchiveEntryKind},
	report::ArchiveEntryId,
};

/// What the codec does with the entries it reads.
#[derive(Debug, Clone)]
pub(crate) enum Task {
	/// Sends the entries with their data: every one, or those a [`Selection`] chooses.
	Extract(Option<Selection>),
	/// Sends what every entry of the archive `archive` is, and none of their data.
	List { archive: Uuid },
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
/// file `x`, stored beside it as `._x` where the file system cannot hold them.
const APPLE_DOUBLE_MAGIC: [u8; 4] = [0x00, 0x05, 0x16, 0x07];

/// How an entry's path marks it as macOS metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MacShape {
	/// A `__MACOSX` folder, or anything but a file in one: its path says it all.
	InMacFolder,
	/// A file in a `__MACOSX` folder or named `._name`, which an ordinary file may be too: an
	/// AppleDouble file when its first bytes say so. A file is never left out by its path alone,
	/// so an archive that is removed once extracted takes no ordinary file with it.
	AppleDoubleName,
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
			.segments
			.first()
			.is_some_and(|first| first.as_ref() == MAC_METADATA_DIR);
		if self.kind != ArchiveEntryKind::File {
			return in_mac_folder.then_some(MacShape::InMacFolder);
		}
		let apple_double = in_mac_folder
			|| path.segments.last().is_some_and(|name| {
				let name: &str = name.as_ref();
				name.len() > 2 && name.starts_with("._")
			});
		apple_double.then_some(MacShape::AppleDoubleName)
	}

	/// The skip record for it.
	pub(super) fn skipped(&self, reason: ExtractSkipReason) -> WorkerEvent {
		let (path, path_truncated) = display_path(self.stored);
		WorkerEvent::Skipped(SkippedMember {
			ordinal: self.ordinal,
			path: path.to_owned(),
			path_truncated,
			bytes: self.size,
			reason,
		})
	}
}

/// What an extraction does with an entry.
#[derive(Debug)]
pub(super) enum Verdict {
	/// Nothing: a partial extraction did not choose the entry, and nothing of it is read.
	Ignore,
	/// Nothing to create: the directory is the root the others land in.
	Root,
	Skip(ExtractSkipReason),
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
	/// The archive, when listing it.
	listing: Option<Uuid>,
	chooser: Option<Chooser>,
}

impl<'p> Walk<'p> {
	pub(super) fn new(port: &'p WorkerPort, task: &Task, skip_mac_metadata: bool) -> Self {
		let (listing, chooser) = match task {
			Task::Extract(selection) => (None, selection.clone().map(Chooser::new)),
			Task::List { archive } => (Some(*archive), None),
		};
		Self {
			port,
			skip_mac_metadata,
			listing,
			chooser,
		}
	}

	pub(super) fn listing(&self) -> bool {
		self.listing.is_some()
	}

	/// The id of the listed archive's entry `ordinal`.
	pub(super) fn listed_id(&self, ordinal: u64) -> ArchiveEntryId {
		ArchiveEntryId {
			archive: self.listing.expect("only a listing lists"),
			// the member cap keeps ordinals far below u32::MAX
			index: u32::try_from(ordinal).unwrap_or(u32::MAX),
		}
	}

	/// Checks, before anything is created, a partial extraction of an archive whose entries are
	/// all known up front (a zip's or 7z's): that it holds every entry chosen, each below the
	/// base. Directories chosen are noted, so what is below one is chosen wherever it is stored.
	pub(super) fn check_selection<'e>(
		&mut self,
		entries: impl Iterator<Item = (u64, &'e str, bool)>,
	) -> Result<(), Error> {
		let Some(chooser) = &mut self.chooser else {
			return Ok(());
		};
		let mut found = 0;
		for (ordinal, stored, is_dir) in entries {
			if chooser.selection.ordinals.binary_search(&ordinal).is_err() {
				continue;
			}
			found += 1;
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

	/// What an extraction does with `found`.
	pub(super) fn judge(&mut self, found: &Found) -> Result<Verdict, Error> {
		let is_dir = found.is_dir();
		let path = match &mut self.chooser {
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
		let apple_double = match found.mac_shape() {
			Some(MacShape::InMacFolder) if self.skip_mac_metadata => {
				return Ok(Verdict::Skip(ExtractSkipReason::MacMetadata));
			}
			Some(MacShape::AppleDoubleName) => self.skip_mac_metadata,
			_ => false,
		};
		Ok(Verdict::Take { path, apple_double })
	}

	/// Once every entry was read: a partial extraction of a tar has met every entry it chose.
	pub(super) fn finish(&self) -> Result<(), Error> {
		match &self.chooser {
			Some(chooser) if chooser.met < chooser.selection.ordinals.len() => Err(not_held()),
			_ => Ok(()),
		}
	}

	/// Sends what a listing says of `found`; whether an extraction creates it. `apple_double` is
	/// what its data told, when read; otherwise its name decides.
	pub(super) fn list(&self, found: Found, apple_double: Option<bool>) -> io::Result<bool> {
		if found.is_dir() && matches!(found.path, Err(PathRejection::Empty)) {
			// the archive's own root, which no extraction creates
			return Ok(false);
		}
		// an extraction reads a file's data to tell whether it is AppleDouble: where a listing
		// did not, it marks the entry by its path, and does not say it is skipped
		let (mac_metadata, left_out) = match found.mac_shape() {
			Some(MacShape::InMacFolder) => (true, true),
			Some(MacShape::AppleDoubleName) => {
				(apple_double.unwrap_or(true), apple_double == Some(true))
			}
			None => (false, false),
		};
		let skip = match &found.unreadable {
			// the target is the kind's already
			Some(ExtractSkipReason::Symlink { .. }) => Some(ExtractSkipReason::Symlink {
				target: String::new(),
			}),
			Some(ExtractSkipReason::Hardlink { .. }) => Some(ExtractSkipReason::Hardlink {
				target: String::new(),
			}),
			other => other.clone(),
		}
		.or_else(|| found.path.as_ref().err().map(|e| path_skip_reason(*e)))
		.or((left_out && self.skip_mac_metadata).then_some(ExtractSkipReason::MacMetadata));
		let (stored_path, stored_path_truncated) = display_path(found.stored);
		let path = found.path.as_ref().ok();
		let entry = ArchiveEntry {
			id: self.listed_id(found.ordinal),
			stored_path: stored_path.to_owned(),
			stored_path_truncated,
			path: path.map(ArchivePath::joined),
			size: (found.kind != ArchiveEntryKind::Dir).then_some(found.size),
			modified: found.modified,
			encrypted: found.encrypted,
			method: found.method,
			skip,
			path_rewritten: path.is_some_and(|path| path.rewritten),
			misleading_name: path.is_some_and(|path| path.suspicious),
			mac_metadata,
			kind: found.kind,
		};
		let extracted = entry.skip.is_none();
		self.port.send(WorkerEvent::Listed(Box::new(entry)))?;
		Ok(extracted)
	}

	/// The verdict on an entry `first` took into the job, once its data told more of it than its
	/// header did (a 7z link's target, or whether a reparse point is a link at all).
	pub(super) fn judge_again(&self, found: &Found, first: Verdict) -> Verdict {
		match (first, &found.unreadable) {
			(first @ (Verdict::Ignore | Verdict::Root), _) => first,
			// what its kind skips it for comes first, as in `judge`
			(_, Some(reason)) => Verdict::Skip(reason.clone()),
			(first, None) => first,
		}
	}

	/// `path`, of the file a hard link names, below the base: where the job extracts it; `None`
	/// when it is not below the base, so not extracted.
	pub(super) fn within_base(&self, path: ArchivePath) -> Option<ArchivePath> {
		let Some(chooser) = &self.chooser else {
			return Some(path);
		};
		let keys = chooser.below_base(&path).ok()?;
		(keys.len() > chooser.base.len()).then(|| ArchivePath {
			segments: path.segments[chooser.base.len()..].to_vec(),
			..path
		})
	}

	/// Bytes of the `files` (by ordinal, path as stored and size) the job extracts: every one
	/// with a usable path, or those a partial extraction chose. A file that may be macOS
	/// metadata counts: only its data tells.
	pub(super) fn extracted_bytes<'e>(
		&self,
		files: impl Iterator<Item = (u64, &'e str, u64)>,
	) -> u64 {
		let mut chooser = self.chooser.clone();
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
	let mut head = vec![0u8; APPLE_DOUBLE_MAGIC.len()];
	let read = read_full(data, &mut head)?;
	head.truncate(read);
	Ok((head == APPLE_DOUBLE_MAGIC, head))
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
	dirs: HashSet<u128>,
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
			dirs: HashSet::new(),
			met: 0,
		}
	}

	/// The collision keys of `path`, which has to be below the base: two spellings of a directory
	/// that differ only in case are one directory to an extraction.
	fn below_base(&self, path: &ArchivePath) -> Result<Vec<String>, Error> {
		let keys: Vec<String> = path
			.segments
			.iter()
			.map(|segment| collision_key(segment.as_ref()))
			.collect();
		if keys.starts_with(&self.base) {
			Ok(keys)
		} else {
			Err(not_held())
		}
	}

	/// The path below the base that entry `ordinal`, at `path`, is extracted at when it was
	/// chosen or is below a directory that was; `None` when it is not extracted.
	fn choose(
		&mut self,
		ordinal: u64,
		path: &Result<ArchivePath, PathRejection>,
		is_dir: bool,
	) -> Result<Option<Result<ArchivePath, PathRejection>>, Error> {
		let chosen = self.selection.ordinals.binary_search(&ordinal).is_ok();
		if chosen {
			self.met += 1;
		}
		let path = match path {
			Ok(path) => path,
			// chosen, it is skipped for its path; otherwise nothing tells where it would be
			Err(rejection) => return Ok(chosen.then_some(Err(*rejection))),
		};
		let keys: Vec<String> = path
			.segments
			.iter()
			.map(|segment| collision_key(segment.as_ref()))
			.collect();
		let in_chosen_dir = prefix_digests(&keys).any(|digest| self.dirs.contains(&digest));
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
		Ok(Some(Ok(ArchivePath {
			segments: path.segments[self.base.len()..].to_vec(),
			..path.clone()
		})))
	}
}
