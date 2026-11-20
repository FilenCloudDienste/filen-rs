//! The remote side: applying what changes on the drive to the placeholders of the sync root.
//!
//! The cache tells of changes through its feed ([`feed`]): each changed item is a hint, which
//! queues a remote job on the item's placeholder ([`apply`]) and a reconcile of the populated
//! directory it is now in ([`children`]). Hints decide nothing: each job reads the local state and
//! the cache's current row, and acts from those two alone, so a hint read twice, or the provider's
//! own change read back, does nothing. Every start, every expired anchor, every overflow of the
//! queue and every safety-net walk reconciles every populated directory afresh.
//!
//! The remote side changes clean placeholders only: a file in sync with nothing written since and
//! no upload pending, a directory in sync. Anything else holds what the drive may not have, and is
//! handed to the local side on the same path, which keeps both versions when they conflict. So a
//! remote change never destroys local bytes the drive does not have.

mod apply;
mod children;
mod feed;
mod index;
mod recent;

use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
	time::Duration,
};

use filen_types::fs::Uuid;
use tokio::{
	sync::Notify,
	time::{Instant, sleep},
};
use tracing::{debug, warn};

pub(crate) use self::recent::Recent;
pub(super) use self::{
	apply::{Step, apply, detached_as_extra, may_hold_edit, remove_gone_dir, upload_pending},
	children::{children, populate_pinned},
	feed::{run_feed, start_anchor},
	index::Index,
};
use super::{Inner, lock, queue::Delay, queue::Kind};
use crate::{
	drive::Drive,
	identity::{PlaceholderIdentity, stable_id},
	log_safe::{self},
	os::LocalFs,
	root_path::RootPath,
};

/// How long after the populated directories change they are reported to the cache, so a burst of
/// populations is one report.
const POPULATED_REPORT_DELAY: Duration = Duration::from_secs(2);

/// The uuid the index keeps a placeholder of `identity` under: a file's stable id, a directory's
/// or the root's uuid; `None` for a detached placeholder.
pub(super) fn populated_uuid(identity: PlaceholderIdentity) -> Option<Uuid> {
	match identity {
		PlaceholderIdentity::Root { root } => Some(root),
		PlaceholderIdentity::Dir { uuid } => Some(uuid),
		PlaceholderIdentity::File { stable, .. } => Some(stable),
		PlaceholderIdentity::Detached { .. } => None,
	}
}

/// The most files whose last read the hints remember; past it the oldest is forgotten, and a walk
/// finds the file again if it was left partly on disk.
const MAX_LAST_READS: usize = 4096;
/// The most file versions remembered as verified; past it the least recently checked is forgotten,
/// which costs one more hash of it.
const MAX_VERIFIED: usize = 65_536;
/// The most file versions remembered as damaged; past it the oldest is forgotten.
const MAX_DAMAGED: usize = 4096;
/// The most stable ids a hint named that the index did not place, kept for the next walk.
const MAX_UNPLACED: usize = 4096;
/// The most files whose upload by this run is remembered; past it the least recently uploaded is
/// forgotten, and an update of it is applied without asking the server first.
const MAX_UPLOADED: usize = 65_536;

/// One version of one file: its stable id and its content version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FileVersion {
	pub(crate) stable: Uuid,
	pub(crate) version: Uuid,
}

impl FileVersion {
	/// The version a file placeholder of `identity` holds; `None` for any other placeholder.
	pub(crate) fn of(identity: PlaceholderIdentity) -> Option<Self> {
		match identity {
			PlaceholderIdentity::File { stable, version } => Some(Self { stable, version }),
			PlaceholderIdentity::Root { .. }
			| PlaceholderIdentity::Dir { .. }
			| PlaceholderIdentity::Detached { .. } => None,
		}
	}
}

/// What the dispatcher tells the local engine, from [`LocalEngine::hints`](super::LocalEngine::hints):
/// a directory the platform populated, which the remote side then reconciles and reports to the
/// cache; a read refused because its placeholder is out of date, whose update is then applied at
/// once; the reads in flight, during which a file is never dehydrated, and when the last of them
/// ended, which starts the hydration job's quiet window; the versions a whole-file read checked
/// against their declared hash; and the versions whose content was found damaged, which are never
/// served again.
#[derive(Debug)]
pub struct RemoteHints {
	/// Reads in flight, by the stable id of their file. A std Mutex, held only to count.
	reads: Mutex<HashMap<Uuid, u32>>,
	/// What the hints remember of reads, verifications and damage. A std Mutex, held only to
	/// read or change it.
	memory: Mutex<Memory>,
	/// Hints not yet taken. A std Mutex, held only to push or take.
	pending: Mutex<Vec<Hint>>,
	wake: Notify,
}

/// What the hints remember, each part bounded.
#[derive(Debug)]
struct Memory {
	/// When the last read by another process of each file ended, with the version it read.
	last_reads: Recent<Uuid, (Uuid, Instant)>,
	/// Versions whose bytes on disk were checked against their declared hash, or that declare
	/// none to check against, forgotten when the provider dehydrates them.
	verified: Recent<FileVersion, ()>,
	/// Versions whose content did not match their declared hash.
	damaged: Recent<FileVersion, ()>,
	/// The version this run last uploaded of each file, by its stable id.
	uploaded: Recent<Uuid, Uuid>,
}

impl Default for RemoteHints {
	fn default() -> Self {
		Self {
			reads: Mutex::new(HashMap::new()),
			memory: Mutex::new(Memory {
				last_reads: Recent::new(MAX_LAST_READS, "last reads"),
				verified: Recent::new(MAX_VERIFIED, "verified versions"),
				damaged: Recent::new(MAX_DAMAGED, "damaged versions"),
				uploaded: Recent::new(MAX_UPLOADED, "uploaded versions"),
			}),
			pending: Mutex::new(Vec::new()),
			wake: Notify::new(),
		}
	}
}

/// One hint from the dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hint {
	/// The directory or root of this uuid was populated.
	Populated(Uuid),
	/// A read of the file of this stable id was refused as out of date.
	Stale(Uuid),
	/// The last read in flight of the file of this stable id by another process ended.
	Quiet(Uuid),
	/// The content of the file of this stable id was found damaged.
	Damaged(Uuid),
}

/// Counts a read of a file as in flight until dropped.
#[derive(Debug)]
pub(crate) struct ReadInFlight {
	// Reason for the Arc: the guard outlives the call that made it, in the read's task.
	hints: Arc<RemoteHints>,
	read: FileVersion,
	/// Whether the provider's own process raised the read: its own hydrations and hashes, which
	/// count as in flight but never start the quiet window.
	own: bool,
}

impl Drop for ReadInFlight {
	fn drop(&mut self) {
		let last = {
			let mut reads = lock(&self.hints.reads);
			match reads.get_mut(&self.read.stable) {
				Some(count) => {
					*count = count.saturating_sub(1);
					let last = *count == 0;
					if last {
						reads.remove(&self.read.stable);
					}
					last
				}
				None => true,
			}
		};
		if self.own {
			return;
		}
		lock(&self.hints.memory)
			.last_reads
			.insert(self.read.stable, (self.read.version, Instant::now()));
		if last {
			self.hints.push(Hint::Quiet(self.read.stable));
		}
	}
}

impl RemoteHints {
	fn push(&self, hint: Hint) {
		lock(&self.pending).push(hint);
		self.wake.notify_one();
	}

	/// The directory `identity` was populated.
	pub(crate) fn populated(&self, identity: PlaceholderIdentity) {
		match identity {
			PlaceholderIdentity::Root { root } => self.push(Hint::Populated(root)),
			PlaceholderIdentity::Dir { uuid } => self.push(Hint::Populated(uuid)),
			PlaceholderIdentity::File { .. } | PlaceholderIdentity::Detached { .. } => {}
		}
	}

	/// A read of the file `stable` was refused: its placeholder stands for an older version.
	pub(crate) fn stale(&self, stable: Uuid) {
		self.push(Hint::Stale(stable));
	}

	/// Counts a read of `read` as in flight until the guard drops; `own` when the provider's own
	/// process raised it.
	pub(crate) fn reading(self: &Arc<Self>, read: FileVersion, own: bool) -> ReadInFlight {
		*lock(&self.reads).entry(read.stable).or_default() += 1;
		ReadInFlight {
			hints: Arc::clone(self),
			read,
			own,
		}
	}

	/// A whole-file read of `version` matched its declared hash.
	pub(crate) fn verified(&self, version: FileVersion) {
		lock(&self.memory).verified.insert(version, ());
	}

	/// The provider dehydrated `version`: bytes fetched for it again are checked again.
	pub(super) fn forget_verified(&self, version: FileVersion) {
		lock(&self.memory).verified.remove(&version);
	}

	/// The content of `version` does not match its declared hash: it is never served again, and
	/// the hydration job dehydrates it.
	pub(crate) fn damaged(&self, version: FileVersion) {
		self.record_damaged(version);
		self.push(Hint::Damaged(version.stable));
	}

	/// Records `version` as damaged, with no hint: the hydration job that found it acts on it.
	pub(super) fn record_damaged(&self, version: FileVersion) {
		lock(&self.memory).damaged.insert(version, ());
	}

	/// Whether `version` was found damaged.
	pub(crate) fn is_damaged(&self, version: FileVersion) -> bool {
		lock(&self.memory).damaged.get(&version).is_some()
	}

	/// How many reads of the file `stable` are in flight.
	pub(crate) fn reads_in_flight(&self, stable: Uuid) -> u32 {
		lock(&self.reads).get(&stable).copied().unwrap_or(0)
	}

	/// When the last read of `version` by another process ended, if the hints remember one of
	/// that version.
	pub(crate) fn last_read(&self, version: FileVersion) -> Option<Instant> {
		lock(&self.memory)
			.last_reads
			.get(&version.stable)
			.filter(|(read, _)| *read == version.version)
			.map(|(_, at)| *at)
	}

	/// Whether `version` was verified, touching it.
	pub(crate) fn is_verified(&self, version: FileVersion) -> bool {
		lock(&self.memory).verified.touch(&version)
	}

	/// Forgets that `version` was damaged: the user changed the file since.
	pub(super) fn forget_damaged(&self, version: FileVersion) {
		lock(&self.memory).damaged.remove(&version);
	}

	/// This run uploaded `version`.
	pub(super) fn uploaded(&self, version: FileVersion) {
		lock(&self.memory)
			.uploaded
			.insert(version.stable, version.version);
	}

	/// Whether `version` is the version of its file this run last uploaded.
	pub(super) fn uploaded_here(&self, version: FileVersion) -> bool {
		lock(&self.memory).uploaded.get(&version.stable) == Some(&version.version)
	}

	fn take(&self) -> Vec<Hint> {
		std::mem::take(&mut *lock(&self.pending))
	}
}

/// Applies the dispatcher's hints as they come.
pub(super) async fn run_hints<D: Drive, L: LocalFs>(inner: Arc<Inner<D, L>>) {
	loop {
		inner.hints.wake.notified().await;
		for hint in inner.hints.take() {
			match hint {
				Hint::Populated(uuid) => {
					let (path, changed) = {
						let mut state = lock(&inner.state);
						let changed = state.index.mark_populated(uuid);
						(state.index.path_of(uuid).cloned(), changed)
					};
					match path {
						Some(path) => {
							inner.push_kind(&path, Delay::Now, Kind::Children);
							// A pinned directory populated only now hydrates what it got.
							inner.push_kind(&path, Delay::Now, Kind::Hydration);
						}
						// Not in the index: the walk finds it, populated now.
						None => Inner::request_walk(&inner),
					}
					if changed {
						inner.populated_changed();
					}
				}
				Hint::Stale(stable) => {
					let path = lock(&inner.state).index.path_of(stable).cloned();
					if let Some(path) = path {
						inner.push_kind(&path, Delay::Now, Kind::Remote);
					}
				}
				Hint::Quiet(stable) => {
					let quiet = inner.config.hydration_quiet;
					hydrate_later(&inner, stable, Delay::After(quiet), true);
				}
				Hint::Damaged(stable) => hydrate_later(&inner, stable, Delay::Now, false),
			}
		}
	}
}

/// Queues the hydration job on the placeholder of the file `stable` after `delay`, which retries a
/// job the queue parked; `activity` when a read ended, which gives a completion that stalled
/// another chance. A file the index does not place is kept for the next walk.
fn hydrate_later<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	stable: Uuid,
	delay: Delay,
	activity: bool,
) {
	let path = {
		let mut state = lock(&inner.state);
		let path = state.index.path_of(stable).cloned();
		match &path {
			Some(path) if activity => {
				state.hydration_stalls.remove(&path.case_key());
			}
			Some(_) => {}
			None => {
				if state.unplaced.len() < MAX_UNPLACED {
					state.unplaced.insert(stable);
				} else {
					debug!(%stable, "too many unplaced files hinted; the next walk finds this one");
				}
			}
		}
		path
	};
	if let Some(path) = path {
		inner.push_kind(&path, delay, Kind::Hydration);
	}
}

impl<D: Drive, L: LocalFs> Inner<D, L> {
	/// The populated directories changed: they are reported to the cache once they have been
	/// quiet a moment, wholesale, so the cache keeps them fresh after a gap in its live updates.
	pub(super) fn populated_changed(self: &Arc<Self>) {
		{
			let mut state = lock(&self.state);
			if state.populated_report_due {
				return;
			}
			state.populated_report_due = true;
		}
		let inner = Arc::clone(self);
		self.spawn(async move {
			sleep(POPULATED_REPORT_DELAY).await;
			let ids: Vec<String> = {
				let mut state = lock(&inner.state);
				state.populated_report_due = false;
				let root = state.index.uuid_at(&RootPath::root());
				state
					.index
					.populated()
					.map(|uuid| {
						if Some(uuid) == root {
							uuid.to_string()
						} else {
							stable_id(uuid).0
						}
					})
					.collect()
			};
			match inner.drive.set_populated_dirs(ids).await {
				Ok(()) => debug!("reported the populated directories to the cache"),
				Err(error) => {
					warn!(error = %log_safe::error(&error), "reporting the populated directories failed")
				}
			}
		});
	}
}

#[cfg(test)]
mod tests;
