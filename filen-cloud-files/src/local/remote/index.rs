//! Where each remote item has its placeholder: the change feed names items by uuid and carries no
//! old name or parent, so the remote side finds an item's local path here. Every answer is a hint:
//! the job that acts on it reads the item's state first, and a mismatch makes the index incomplete
//! until the next walk rebuilds it.
//!
//! It covers the placeholders inside populated directories (the root's children once the root is
//! populated): a file by its stable id, a directory by its uuid, and the root by its uuid. Memory:
//! one path and one uuid per such placeholder, about 150 bytes each, an accepted cost.

use std::collections::{HashMap, HashSet};

use filen_types::fs::Uuid;

use crate::root_path::{CaseKey, RootPath};

/// The index.
#[derive(Debug, Default)]
pub(in crate::local) struct Index {
	by_id: HashMap<Uuid, RootPath>,
	by_path: HashMap<CaseKey, Uuid>,
	/// The directories the platform populated, the root included, by uuid.
	populated: HashSet<Uuid>,
	/// Whether a walk built the index and nothing found it wrong since. While it is not, the
	/// remote side creates no placeholder: an item it cannot find may be local under another path.
	complete: bool,
	/// Counts the removals and moves, so what was read of the sync root before one is not
	/// written into the index after it.
	epoch: u64,
}

impl Index {
	/// Records that the placeholder of `uuid` is at `path`, replacing what either held.
	pub(in crate::local) fn insert(&mut self, path: &RootPath, uuid: Uuid) {
		if let Some(old) = self.by_id.insert(uuid, path.clone())
			&& old.case_key() != path.case_key()
		{
			self.by_path.remove(&old.case_key());
		}
		if let Some(previous) = self.by_path.insert(path.case_key(), uuid)
			&& previous != uuid
		{
			self.by_id.remove(&previous);
		}
	}

	/// Records that the placeholder of `uuid` is at `path` when the directory it is in is
	/// populated, so the index keeps to the populated directories.
	pub(in crate::local) fn insert_if_populated(&mut self, path: &RootPath, uuid: Uuid) {
		if path
			.parent()
			.is_some_and(|parent| self.is_populated_path(&parent))
		{
			self.insert(path, uuid);
		}
	}

	/// Forgets `path` and everything under it.
	pub(in crate::local) fn remove_under(&mut self, path: &RootPath) {
		self.epoch += 1;
		let gone: Vec<CaseKey> = self
			.by_id
			.values()
			.filter(|at| at.starts_with(path))
			.map(RootPath::case_key)
			.collect();
		for key in gone {
			if let Some(uuid) = self.by_path.remove(&key) {
				self.by_id.remove(&uuid);
				self.populated.remove(&uuid);
			}
		}
	}

	/// Moves every path at or under `from` to the same place under `to`; an item the index had at
	/// one of the new paths is forgotten.
	pub(in crate::local) fn rebase(&mut self, from: &RootPath, to: &RootPath) {
		self.epoch += 1;
		let moved: Vec<(Uuid, RootPath)> = self
			.by_id
			.iter()
			.filter_map(|(uuid, at)| Some((*uuid, at.rebase(from, to)?)))
			.collect();
		for (uuid, _) in &moved {
			if let Some(old) = self.by_id.remove(uuid) {
				self.by_path.remove(&old.case_key());
			}
		}
		for (uuid, path) in moved {
			self.insert(&path, uuid);
		}
	}

	/// Forgets the item `uuid`, wherever the index had it.
	pub(in crate::local) fn remove_id(&mut self, uuid: Uuid) {
		self.epoch += 1;
		if let Some(old) = self.by_id.remove(&uuid) {
			self.by_path.remove(&old.case_key());
		}
	}

	/// The count of removals and moves so far.
	pub(in crate::local) fn epoch(&self) -> u64 {
		self.epoch
	}

	/// Where the placeholder of `uuid` is.
	pub(in crate::local) fn path_of(&self, uuid: Uuid) -> Option<&RootPath> {
		self.by_id.get(&uuid)
	}

	/// The uuid of the placeholder at `path`.
	pub(in crate::local) fn uuid_at(&self, path: &RootPath) -> Option<Uuid> {
		self.by_path.get(&path.case_key()).copied()
	}

	/// Marks the directory `uuid` populated; whether it was not before.
	pub(in crate::local) fn mark_populated(&mut self, uuid: Uuid) -> bool {
		self.populated.insert(uuid)
	}

	/// Whether the directory `uuid` is populated.
	pub(in crate::local) fn is_populated(&self, uuid: Uuid) -> bool {
		self.populated.contains(&uuid)
	}

	/// Whether the directory at `path` is populated.
	pub(in crate::local) fn is_populated_path(&self, path: &RootPath) -> bool {
		self.uuid_at(path)
			.is_some_and(|uuid| self.is_populated(uuid))
	}

	/// The populated directories.
	pub(in crate::local) fn populated(&self) -> impl Iterator<Item = Uuid> + '_ {
		self.populated.iter().copied()
	}

	/// Whether the index can be trusted to say an item has no placeholder.
	pub(in crate::local) fn is_complete(&self) -> bool {
		self.complete
	}

	/// Marks the index wrong: a job found an item elsewhere than it said.
	pub(in crate::local) fn mark_incomplete(&mut self) {
		self.complete = false;
	}

	/// Replaces the index with `built`, which a walk just finished, now complete.
	pub(in crate::local) fn replace(&mut self, built: Self) {
		let epoch = self.epoch + 1;
		*self = Self {
			complete: true,
			epoch,
			..built
		};
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test_support::root_path;

	const A: Uuid = Uuid::from_u128(1);
	const B: Uuid = Uuid::from_u128(2);
	const D: Uuid = Uuid::from_u128(3);

	#[test]
	fn a_moved_directory_moves_what_it_holds() {
		let mut index = Index::default();
		index.insert(&root_path("d"), D);
		index.insert(&root_path(r"d\a.txt"), A);
		index.insert(&root_path("b.txt"), B);
		index.rebase(&root_path("d"), &root_path("e"));
		assert_eq!(index.path_of(A), Some(&root_path(r"e\a.txt")));
		assert_eq!(index.path_of(D), Some(&root_path("e")));
		assert_eq!(index.uuid_at(&root_path(r"d\a.txt")), None);
		assert_eq!(index.uuid_at(&root_path(r"E\A.TXT")), Some(A));
		assert_eq!(index.path_of(B), Some(&root_path("b.txt")));
	}

	#[test]
	fn a_move_onto_an_indexed_path_forgets_the_item_there() {
		let mut index = Index::default();
		index.insert(&root_path("a.txt"), A);
		index.insert(&root_path("b.txt"), B);
		let epoch = index.epoch();
		index.rebase(&root_path("a.txt"), &root_path("b.txt"));
		assert_eq!(index.path_of(A), Some(&root_path("b.txt")));
		assert_eq!(index.path_of(B), None);
		assert!(index.epoch() > epoch);
	}

	#[test]
	fn a_path_taken_by_another_item_forgets_the_old_one() {
		let mut index = Index::default();
		index.insert(&root_path("a.txt"), A);
		index.insert(&root_path("A.TXT"), B);
		assert_eq!(index.path_of(A), None);
		assert_eq!(index.uuid_at(&root_path("a.txt")), Some(B));
		index.insert(&root_path("c.txt"), B);
		assert_eq!(index.uuid_at(&root_path("a.txt")), None);
	}

	#[test]
	fn removing_a_directory_forgets_its_subtree_and_its_population() {
		let mut index = Index::default();
		index.insert(&root_path("d"), D);
		index.mark_populated(D);
		index.insert(&root_path(r"d\a.txt"), A);
		index.remove_under(&root_path("d"));
		assert_eq!(index.path_of(A), None);
		assert!(!index.is_populated(D));
	}

	#[test]
	fn only_a_populated_directory_takes_new_entries() {
		let mut index = Index::default();
		index.insert(&root_path("d"), D);
		index.insert_if_populated(&root_path(r"d\a.txt"), A);
		assert_eq!(index.path_of(A), None);
		index.mark_populated(D);
		index.insert_if_populated(&root_path(r"d\a.txt"), A);
		assert_eq!(index.path_of(A), Some(&root_path(r"d\a.txt")));
	}
}
