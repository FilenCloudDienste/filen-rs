//! Which remote file each recently seen path held: what tells an atomic save from a new file.
//!
//! Editors save by writing a new file and putting it in the old one's place: a rename over it
//! (which raises no callback at all), a rename of the old one aside first (Office, `ReplaceFileW`),
//! or a delete and a create. Each leaves an ordinary file where a placeholder was, and that file is
//! a new version of the item the placeholder stood for, not a new item. The core remembers, per
//! path, the file placeholder it last saw there: from the paths its jobs look at, from the delete
//! and rename requests, and from every walk of the root, so a placeholder nobody opened this
//! session is known too.
//!
//! A path a placeholder was renamed away from claims its file for a new file there only while the
//! rename is recent ([`SAVE_WINDOW`]) or the renamed placeholder is gone (an editor deleted its
//! backup): a file created at an old name long after a rename is a new file.
//!
//! With a data directory, the table of file placeholders is saved too ([`super::records`]), and
//! a start-up reads it back: a save made while the provider was not running, or that a stop cut
//! short, leaves an ordinary file where a placeholder was, which the walk alone cannot tell from a
//! new file. Such a path's saved entry claims the file only when the walk found no placeholder of
//! its remote file anywhere in the root, and only while the remote file is still where the path,
//! or the backup it was renamed to, had it ([`Seen::restored`]).

use std::{
	collections::{HashMap, HashSet, hash_map::Entry},
	time::Duration,
};

use filen_types::fs::Uuid;
use tokio::time::Instant;

use super::records::SavedSeen;
use crate::root_path::{CaseKey, RootPath};

/// How many paths are remembered at most; past it, the quarter seen longest ago is forgotten.
const CAP: usize = 65_536;
/// How long a rename away from a path lets a new file there be the renamed file's new version
/// while the renamed placeholder is still there: an editor's save renames the old file aside and
/// puts the new one in its place within moments.
pub(super) const SAVE_WINDOW: Duration = Duration::from_secs(60);

/// The new files this run uploaded from ordinary files in the root, by stable id: an editor's
/// temporary file is one, uploaded as a file of its own when the editor takes longer than the
/// settle time to put it in the saved file's place. One renamed there within [`SAVE_WINDOW`] of
/// its upload, still at the version uploaded, is the save's temporary file, never a file the user
/// keeps: the saved file takes its bytes as a new version. Kept in memory only: after a restart,
/// such a rename is the user moving one file over another.
#[derive(Debug, Default)]
pub(super) struct FreshUploads(HashMap<Uuid, (Uuid, Instant)>);

impl FreshUploads {
	/// The file `stable` was just uploaded as the version `version` of a new file.
	pub(super) fn uploaded(&mut self, stable: Uuid, version: Uuid, now: Instant) {
		self.0
			.retain(|_, (_, at)| now.saturating_duration_since(*at) <= SAVE_WINDOW);
		self.0.insert(stable, (version, now));
	}

	/// Whether the file `stable` is a new file this run uploaded within [`SAVE_WINDOW`] before
	/// `now`, at the version `version`: no other version was uploaded since.
	pub(super) fn is_fresh(&self, stable: Uuid, version: Uuid, now: Instant) -> bool {
		self.0.get(&stable).is_some_and(|(uploaded, at)| {
			*uploaded == version && now.saturating_duration_since(*at) <= SAVE_WINDOW
		})
	}
}

/// The file placeholder last seen at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Seen {
	path: RootPath,
	pub(super) stable: Uuid,
	pub(super) version: Uuid,
	/// Where the placeholder was renamed to, when the user renamed it away: an editor keeping the
	/// old file as a backup while it saves.
	pub(super) moved_to: Option<RootPath>,
	/// When it was seen, or renamed away.
	pub(super) at: Instant,
	/// The file was uploaded from an ordinary file at this path that is not its placeholder yet: a
	/// delete of that file is the remote file's delete.
	pub(super) unconverted: bool,
	/// Read back from the records a previous run saved, not seen in this one.
	pub(super) restored: bool,
}

impl Seen {
	/// The path it was seen at.
	pub(super) fn path(&self) -> &RootPath {
		&self.path
	}

	/// Whether `other` says what this says of its path, whenever either was seen.
	fn same_as(&self, other: &Self) -> bool {
		self.stable == other.stable
			&& self.version == other.version
			&& self.moved_to == other.moved_to
			&& self.unconverted == other.unconverted
	}
}

/// The table, bounded.
#[derive(Debug, Default)]
pub(super) struct Lineage {
	by_path: HashMap<CaseKey, Seen>,
	/// The paths of `by_path` holding each file: [`Lineage::others`] is asked for every file a
	/// reconcile places, under the engine's state lock, and never scans the table.
	by_stable: HashMap<Uuid, HashSet<CaseKey>>,
	/// The files a path read back from a previous run claims ([`Seen::restored`]).
	restored: HashSet<Uuid>,
	/// Whether it changed since it was last saved.
	changed: bool,
}

impl Lineage {
	/// Remembers that `path` held the version `version` of the file `stable`.
	pub(super) fn remember(&mut self, path: &RootPath, stable: Uuid, version: Uuid, now: Instant) {
		self.insert(path, stable, version, now, false);
	}

	/// Remembers that the ordinary file at `path` was uploaded as the version `version` of the new
	/// file `stable`, and is not its placeholder yet.
	pub(super) fn remember_upload(
		&mut self,
		path: &RootPath,
		stable: Uuid,
		version: Uuid,
		now: Instant,
	) {
		self.insert(path, stable, version, now, true);
	}

	fn insert(
		&mut self,
		path: &RootPath,
		stable: Uuid,
		version: Uuid,
		now: Instant,
		unconverted: bool,
	) {
		let key = path.case_key();
		if self.by_path.len() >= CAP && !self.by_path.contains_key(&key) {
			self.forget_oldest(CAP / 4);
		}
		let seen = Seen {
			path: path.clone(),
			stable,
			version,
			moved_to: None,
			at: now,
			unconverted,
			restored: false,
		};
		self.put(key, seen);
	}

	/// Puts `seen` under `key`, noting a change.
	fn put(&mut self, key: CaseKey, seen: Seen) {
		if self
			.by_path
			.get(&key)
			.is_none_or(|held| !held.same_as(&seen))
		{
			self.changed = true;
		}
		if let Some(held) = self.link(key, seen) {
			self.unclaim(&held);
		}
	}

	/// Puts `seen` under `key` in both maps, returning what `key` held.
	fn link(&mut self, key: CaseKey, seen: Seen) -> Option<Seen> {
		self.by_stable
			.entry(seen.stable)
			.or_default()
			.insert(key.clone());
		let stable = seen.stable;
		let held = self.by_path.insert(key.clone(), seen)?;
		if held.stable != stable {
			self.unindex(held.stable, &key);
		}
		Some(held)
	}

	/// Removes what `key` holds from both maps.
	fn unlink(&mut self, key: &CaseKey) -> Option<Seen> {
		let seen = self.by_path.remove(key)?;
		self.unindex(seen.stable, key);
		Some(seen)
	}

	/// Drops `key` from the paths holding the file `stable`.
	fn unindex(&mut self, stable: Uuid, key: &CaseKey) {
		if let Entry::Occupied(mut keys) = self.by_stable.entry(stable) {
			keys.get_mut().remove(key);
			if keys.get().is_empty() {
				keys.remove();
			}
		}
	}

	/// `seen` no longer stands in the table: a claim it read back goes with it.
	fn unclaim(&mut self, seen: &Seen) {
		if seen.restored {
			self.restored.remove(&seen.stable);
		}
	}

	/// Whether a path read back from a previous run claims an ordinary file as a save of the file
	/// `stable`: the remote side does not place that file meanwhile, as the save takes its place.
	pub(super) fn claims_restored(&self, stable: Uuid) -> bool {
		self.restored.contains(&stable)
	}

	/// Takes back `saved`, which a previous run saved and the start-up walk found unclaimed by any
	/// placeholder: it claims an ordinary file at its path as a save of its file.
	pub(super) fn restore(&mut self, saved: SavedSeen, now: Instant) {
		self.restored.insert(saved.stable);
		self.link(
			saved.path.case_key(),
			Seen {
				path: saved.path,
				stable: saved.stable,
				version: saved.version,
				moved_to: saved.moved_to,
				at: now,
				unconverted: false,
				restored: true,
			},
		);
	}

	/// What to save: every path holding a placeholder, or claiming one renamed away; an ordinary
	/// file uploaded but not converted yet is left out, as the walk adopts it again by its name and
	/// content.
	pub(super) fn saved(&self) -> Vec<SavedSeen> {
		self.by_path
			.values()
			.filter(|seen| !seen.unconverted)
			.map(|seen| SavedSeen {
				path: seen.path.clone(),
				stable: seen.stable,
				version: seen.version,
				moved_to: seen.moved_to.clone(),
			})
			.collect()
	}

	/// Whether it changed since the last call.
	pub(super) fn take_changed(&mut self) -> bool {
		std::mem::take(&mut self.changed)
	}

	/// Marks it changed: a save of it failed.
	pub(super) fn mark_changed(&mut self) {
		self.changed = true;
	}

	/// Forgets the `count` paths seen longest ago.
	fn forget_oldest(&mut self, count: usize) {
		let mut by_age: Vec<(Instant, CaseKey)> = self
			.by_path
			.iter()
			.map(|(key, seen)| (seen.at, key.clone()))
			.collect();
		by_age.sort_unstable_by_key(|(at, _)| *at);
		for (_, key) in by_age.into_iter().take(count) {
			if let Some(seen) = self.unlink(&key) {
				self.unclaim(&seen);
			}
		}
		self.changed = true;
	}

	/// What `path` last held.
	pub(super) fn get(&self, path: &RootPath) -> Option<&Seen> {
		self.by_path.get(&path.case_key())
	}

	/// Forgets `path`.
	pub(super) fn forget(&mut self, path: &RootPath) {
		if let Some(seen) = self.unlink(&path.case_key()) {
			self.unclaim(&seen);
			self.changed = true;
		}
	}

	/// Records that the file at `from` was renamed to `to`: `to` holds it now, and `from` still
	/// says what it held and where it went. An ordinary file not converted yet holds no name of its
	/// own: its entry moves with it.
	pub(super) fn moved(&mut self, from: &RootPath, to: &RootPath, now: Instant) {
		let Some(seen) = self.by_path.get_mut(&from.case_key()) else {
			return;
		};
		let (stable, version, unconverted, restored) =
			(seen.stable, seen.version, seen.unconverted, seen.restored);
		if unconverted {
			self.unlink(&from.case_key());
		} else {
			seen.moved_to = Some(to.clone());
			seen.at = now;
			seen.restored = false;
		}
		if restored {
			self.restored.remove(&stable);
		}
		self.changed = true;
		self.insert(to, stable, version, now, unconverted);
	}

	/// Records that the provider moved the placeholder at `from` to `to`, following its remote
	/// item: `to` holds it now, and `from` claims nothing, as no editor's save moved it.
	pub(super) fn relocated(&mut self, from: &RootPath, to: &RootPath, now: Instant) {
		let Some(seen) = self.unlink(&from.case_key()) else {
			return;
		};
		self.unclaim(&seen);
		self.changed = true;
		self.insert(to, seen.stable, seen.version, now, seen.unconverted);
	}

	/// Records that the ordinary file at `from` was renamed to `to`, when it is an upload not
	/// converted yet; anything else an ordinary file's rename leaves alone. When `to` claims
	/// another file, a placeholder of it there just now or renamed away within [`SAVE_WINDOW`],
	/// the upload is a save's temporary file renamed into that file's place: `to` keeps its claim,
	/// and the upload, which no file holds any more, is returned as its stable id, to be trashed.
	pub(super) fn follow_upload(
		&mut self,
		from: &RootPath,
		to: &RootPath,
		now: Instant,
	) -> Option<Uuid> {
		let upload = self.get(from).filter(|seen| seen.unconverted)?.stable;
		let saves = self.get(to).is_some_and(|seen| {
			!seen.unconverted
				&& seen.stable != upload
				&& (seen.moved_to.is_none()
					|| now.saturating_duration_since(seen.at) <= SAVE_WINDOW)
		});
		if saves {
			self.forget(from);
			return Some(upload);
		}
		self.moved(from, to, now);
		None
	}

	/// Moves every path under the directory `from` to the same place under `to`.
	pub(super) fn rebase(&mut self, from: &RootPath, to: &RootPath) {
		let moved: Vec<CaseKey> = self
			.by_path
			.iter()
			.filter(|(_, seen)| seen.path.starts_with(from))
			.map(|(key, _)| key.clone())
			.collect();
		for key in moved {
			let Some(mut seen) = self.unlink(&key) else {
				continue;
			};
			let Some(path) = seen.path.rebase(from, to) else {
				continue;
			};
			seen.path = path;
			self.link(seen.path.case_key(), seen);
			self.changed = true;
		}
	}

	/// The paths remembered as holding the file `stable`, `except` left out.
	pub(super) fn others(&self, stable: Uuid, except: &RootPath) -> Vec<RootPath> {
		let except = except.case_key();
		self.by_stable
			.get(&stable)
			.into_iter()
			.flatten()
			.filter(|key| **key != except)
			.filter_map(|key| self.by_path.get(key))
			.map(|seen| seen.path.clone())
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test_support::root_path;

	fn sorted(mut paths: Vec<RootPath>) -> Vec<RootPath> {
		paths.sort_by_key(RootPath::case_key);
		paths
	}

	/// The paths of a file follow every change of the table: renames, relocations, a directory's
	/// move, forgetting, and a path that comes to hold another file.
	#[test]
	fn the_paths_of_a_file_follow_every_change() {
		let (file, other) = (Uuid::new_v4(), Uuid::new_v4());
		let now = Instant::now();
		let mut lineage = Lineage::default();
		lineage.remember(&root_path("a.txt"), file, Uuid::new_v4(), now);
		lineage.moved(&root_path("a.txt"), &root_path("a.bak"), now);
		assert_eq!(
			sorted(lineage.others(file, &root_path("x"))),
			[root_path("a.bak"), root_path("a.txt")]
		);
		lineage.relocated(&root_path("a.bak"), &root_path(r"d\a.bak"), now);
		lineage.rebase(&root_path("d"), &root_path("e"));
		assert_eq!(
			sorted(lineage.others(file, &root_path("a.txt"))),
			[root_path(r"e\a.bak")]
		);
		lineage.remember(&root_path("A.TXT"), other, Uuid::new_v4(), now);
		assert_eq!(
			lineage.others(file, &root_path("x")),
			[root_path(r"e\a.bak")]
		);
		assert_eq!(lineage.others(other, &root_path("x")), [root_path("A.TXT")]);
		lineage.forget(&root_path(r"E\A.BAK"));
		assert_eq!(
			lineage.others(file, &root_path("x")),
			Vec::<RootPath>::new()
		);
		assert!(
			!lineage.by_stable.contains_key(&file),
			"nothing left behind"
		);
	}

	/// Past its cap the table forgets its oldest paths, and the paths of each file with them.
	#[test]
	fn forgotten_paths_leave_their_files() {
		let mut lineage = Lineage::default();
		let start = Instant::now();
		let file = Uuid::new_v4();
		for n in 0..=CAP {
			let at = start + Duration::from_millis(u64::try_from(n).unwrap());
			let stable = if n == 0 { file } else { Uuid::new_v4() };
			lineage.remember(&root_path(&format!("f{n}")), stable, Uuid::new_v4(), at);
		}
		assert_eq!(
			lineage.others(file, &root_path("x")),
			Vec::<RootPath>::new()
		);
		assert_eq!(lineage.by_stable.len(), lineage.by_path.len());
	}
}
