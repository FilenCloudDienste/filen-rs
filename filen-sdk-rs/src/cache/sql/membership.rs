//! Sync-root membership. Sync roots are NOT separate `roots` rows — the cache has exactly one `roots`
//! row (the account root), and membership is computed by walking `items.parent` upward from a uuid and
//! intersecting the chain with the in-memory set of configured sync-root uuids. An item is "in" a sync
//! root iff the item itself, or any of its ancestors up to the account root, is a configured sync-root
//! key. Keeping the root set in memory (rather than a column) is what lets nested roots fall out for
//! free — a uuid can be inside several roots at once.
//!
//! This module is just the membership PRIMITIVES; the apply-time gate and the per-root dispatch that
//! consume them live in `state.rs`.

use std::collections::HashMap;

use rusqlite::params;
use uuid::Uuid;

use crate::cache::CacheState;

impl CacheState {
	/// The upward ancestor chain of `uuid` — the seed itself plus every ancestor up to (and including)
	/// the account root — by walking `items.parent`. Empty if `uuid` is not cached. Cycle-safe.
	pub(crate) fn ancestors_of(&self, uuid: Uuid) -> rusqlite::Result<Vec<Uuid>> {
		let mut stmt = self
			.db
			.prepare_cached(super::statements::ANCESTRY_OF_UUID)?;
		let rows = stmt.query_map(params![uuid], |row| row.get::<_, Uuid>(0))?;
		rows.collect()
	}

	/// Whether `uuid`, or any of its ancestors, is a configured sync root — i.e. the item belongs to at
	/// least one sync root. The seed key is checked in memory first (the common fast path: a direct child
	/// of a root); only on a miss do we walk the ancestry. Generic over the map value so it works
	/// directly against `CacheState::sync_roots` (a `HashMap<Uuid, RootRegistrations>`) with no per-event
	/// allocation.
	pub(crate) fn in_any_sync_root<V>(
		&self,
		uuid: Uuid,
		sync_roots: &HashMap<Uuid, V>,
	) -> rusqlite::Result<bool> {
		if sync_roots.contains_key(&uuid) {
			return Ok(true);
		}
		let mut stmt = self
			.db
			.prepare_cached(super::statements::ANCESTRY_OF_UUID)?;
		let mut rows = stmt.query(params![uuid])?;
		while let Some(row) = rows.next()? {
			let ancestor: Uuid = row.get(0)?;
			if sync_roots.contains_key(&ancestor) {
				return Ok(true);
			}
		}
		Ok(false)
	}

	/// Evict a removed sync root's cached subtree. Deletes the root's descendants but PROTECTS every
	/// still-active root in `protected_roots` — both their subtrees and their ancestor paths — so the
	/// `cascade_on_delete` trigger can never reach a still-active nested root via a deleted intermediate
	/// dir. The root's own node and the account-root item are kept (see the SQL). The protected set is
	/// in-memory, so it is staged through a TEMP table for the delete to join against.
	pub(crate) fn evict_sync_root_subtree(
		&mut self,
		root: Uuid,
		protected_roots: &[Uuid],
	) -> rusqlite::Result<()> {
		let tx = self.db.transaction()?;
		// The TEMP table lives for the whole connection (not just this transaction), so CREATE uses
		// IF NOT EXISTS to be idempotent across calls and the explicit CLEAR purges any rows left by a
		// prior eviction on this connection before we repopulate it below.
		tx.execute(super::statements::EVICT_PROTECTED_ROOTS_CREATE, [])?;
		tx.execute(super::statements::EVICT_PROTECTED_ROOTS_CLEAR, [])?;
		{
			let mut stmt = tx.prepare_cached(super::statements::EVICT_PROTECTED_ROOTS_INSERT)?;
			for protected in protected_roots {
				stmt.execute(params![protected])?;
			}
		}
		tx.execute(super::statements::EVICT_SYNC_ROOT, params![root])?;
		tx.commit()
	}

	/// Which configured sync roots `uuid` belongs to (the intersection of its ancestor chain with the
	/// `sync_roots` keys) — a uuid nested under several roots belongs to all of them. Used by dispatch to
	/// route an applied event to every owning root's callback.
	pub(crate) fn owning_sync_roots<V>(
		&self,
		uuid: Uuid,
		sync_roots: &HashMap<Uuid, V>,
	) -> rusqlite::Result<Vec<Uuid>> {
		Ok(self
			.ancestors_of(uuid)?
			.into_iter()
			.filter(|ancestor| sync_roots.contains_key(ancestor))
			.collect())
	}
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use crate::fs::dir::cache::CacheableDir;
	use chrono::Utc;
	use filen_types::api::v3::dir::color::DirColor;

	use super::*;

	/// Build a dir `uuid` under `parent` so membership tests can assemble arbitrary item trees.
	fn dir(uuid: u128, parent: Uuid) -> CacheableDir<'static> {
		CacheableDir {
			uuid: Uuid::from_u128(uuid),
			parent,
			color: DirColor::Default,
			favorited: false,
			timestamp: Utc::now(),
			name: Cow::Owned(format!("dir-{uuid}")),
			created: None,
		}
	}

	fn roots(uuids: &[u128]) -> HashMap<Uuid, ()> {
		uuids.iter().map(|u| (Uuid::from_u128(*u), ())).collect()
	}

	/// Cache the tree  account-root → A(1) → B(2) → C(3), and a sibling S(9) directly under the root.
	fn state_with_tree() -> CacheState {
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		let a = dir(1, root);
		let b = dir(2, a.uuid);
		let c = dir(3, b.uuid);
		let s = dir(9, root);
		state.upsert_dirs([&a, &b, &c, &s].into_iter()).unwrap();
		state
	}

	#[test]
	fn ancestors_walk_up_to_the_account_root() {
		let state = state_with_tree();
		let mut chain = state.ancestors_of(Uuid::from_u128(3)).unwrap();
		chain.sort();
		// C, B, A, and the account root.
		let mut want = vec![
			Uuid::from_u128(3),
			Uuid::from_u128(2),
			Uuid::from_u128(1),
			state.root_uuid,
		];
		want.sort();
		assert_eq!(chain, want);
	}

	#[test]
	fn ancestors_of_uncached_uuid_is_empty() {
		let state = state_with_tree();
		assert!(
			state
				.ancestors_of(Uuid::from_u128(0xDEAD))
				.unwrap()
				.is_empty()
		);
	}

	#[test]
	fn membership_true_for_descendant_of_a_sync_root() {
		let state = state_with_tree();
		// A(1) is a sync root; C(3) is its grandchild → in-root.
		assert!(
			state
				.in_any_sync_root(Uuid::from_u128(3), &roots(&[1]))
				.unwrap()
		);
	}

	#[test]
	fn membership_true_for_the_sync_root_node_itself() {
		let state = state_with_tree();
		assert!(
			state
				.in_any_sync_root(Uuid::from_u128(1), &roots(&[1]))
				.unwrap()
		);
	}

	#[test]
	fn membership_false_for_item_outside_every_sync_root() {
		let state = state_with_tree();
		// S(9) is under the account root but not under A(1); with only A configured, S is out-of-root.
		assert!(
			!state
				.in_any_sync_root(Uuid::from_u128(9), &roots(&[1]))
				.unwrap()
		);
	}

	#[test]
	fn membership_false_when_no_sync_roots_configured() {
		let state = state_with_tree();
		assert!(
			!state
				.in_any_sync_root(Uuid::from_u128(3), &roots(&[]))
				.unwrap()
		);
	}

	#[test]
	fn account_root_is_only_a_member_when_explicitly_configured() {
		let state = state_with_tree();
		let root = state.root_uuid;
		// With only A configured, the account root itself is NOT in any sync root...
		assert!(!state.in_any_sync_root(root, &roots(&[1])).unwrap());
		// ...but configuring the account root makes everything a member (whole-account sync).
		let account_roots: HashMap<Uuid, ()> = [(root, ())].into_iter().collect();
		assert!(
			state
				.in_any_sync_root(Uuid::from_u128(9), &account_roots)
				.unwrap()
		);
	}

	#[test]
	fn owning_roots_returns_all_nested_roots() {
		let state = state_with_tree();
		// Both A(1) and B(2) are sync roots; C(3) is nested under both.
		let mut owners = state
			.owning_sync_roots(Uuid::from_u128(3), &roots(&[1, 2]))
			.unwrap();
		owners.sort();
		let mut want = vec![Uuid::from_u128(1), Uuid::from_u128(2)];
		want.sort();
		assert_eq!(owners, want);
	}
}
