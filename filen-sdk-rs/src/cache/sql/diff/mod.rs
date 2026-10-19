//! Resync diff. The worker lists the remote subtree under a write lock, converts every node to its
//! [`CacheableDir`]/[`CacheableFile`] form, and loads the `(uuid, parent, type, content_hash)`
//! projection into the per-connection TEMP table `diff_incoming`. The diff queries (deletes /
//! creates+moves / content-changes) then compare that snapshot against the cached `items` and emit
//! synthetic events onto the durable drain — the same rail live socket events flow through, so the
//! convergence reuses all the apply/ordering machinery instead of mutating `items` directly.
//!
//! The staged `content_hash` is derived from the same `content_fingerprint()` written to
//! `items.content_hash`, so the content-change query is a direct hash comparison rather than a
//! field-by-field one.

use std::{borrow::Borrow, collections::HashMap};

use crate::fs::{dir::cache::CacheableDir, file::cache::CacheableFile};
use rusqlite::{CachedStatement, params};
use uuid::Uuid;

use super::{
	columns::{ITEMS_TYPE, ITEMS_UUID},
	item::ItemType,
};
use crate::cache::{
	CacheState,
	state::{CacheEvent, CacheEventType, DirEvent, FileEvent},
};

/// Which upsert-shaped synthetic to emit for a listed item that differs from the cache. All three
/// resolve to the same `items` upsert at apply time (so they converge identically); the distinction is
/// kept because it is semantically correct and the per-root dispatch reports the right kind of change.
#[derive(Clone, Copy)]
enum UpsertKind {
	New,
	Move,
	Changed,
}

/// Build a synthetic deletion (`id: None`) for a vanished cached item, dispatched by its stored type.
/// Type 0 (the root) is never a deletion target and yields `None`.
fn build_removed_event(uuid: Uuid, item_type: i8) -> Option<CacheEvent<'static>> {
	let event = if item_type == ItemType::Dir as i8 {
		CacheEventType::Dir(DirEvent::Removed(uuid))
	} else if item_type == ItemType::File as i8 {
		CacheEventType::File(FileEvent::Removed(uuid))
	} else {
		// The SQL passes never return type 0 (the root is guarded out), so this is unreachable in
		// practice; surface it loudly rather than silently dropping a deletion.
		tracing::error!("resync: unexpected item type {item_type} for {uuid}; skipping deletion");
		return None;
	};
	Some(CacheEvent { id: None, event })
}

/// Build a synthetic upsert (`id: None`) carrying the full cacheable payload pulled from the listing
/// maps. Returns `None` if the type is unexpected or the uuid is missing from the map (a bug, since
/// `diff_incoming` is populated from these very maps — the caller logs it).
fn build_upsert_event(
	uuid: Uuid,
	item_type: i8,
	kind: UpsertKind,
	dirs: &HashMap<Uuid, CacheableDir<'static>>,
	files: &HashMap<Uuid, CacheableFile<'static>>,
) -> Option<CacheEvent<'static>> {
	let event = if item_type == ItemType::Dir as i8 {
		let dir = dirs.get(&uuid)?.clone();
		CacheEventType::Dir(match kind {
			UpsertKind::New => DirEvent::New(dir),
			UpsertKind::Move => DirEvent::Move(dir),
			UpsertKind::Changed => DirEvent::Changed(dir),
		})
	} else if item_type == ItemType::File as i8 {
		let file = files.get(&uuid)?.clone();
		CacheEventType::File(match kind {
			UpsertKind::New => FileEvent::New(file),
			UpsertKind::Move => FileEvent::Move(file),
			UpsertKind::Changed => FileEvent::Changed(file),
		})
	} else {
		// Unreachable in practice (the SQL passes guard out type 0); log loudly so this is not
		// confused with the missing-from-map `?` returns above, which the caller attributes differently.
		tracing::error!("resync: unexpected item type {item_type} for {uuid}; skipping upsert");
		return None;
	};
	Some(CacheEvent { id: None, event })
}

/// Depth of `uuid` in the listed tree (direct children of the sync root = 0), walking the parent
/// chain. Used to order creates/moves parent-before-child. The walk is bounded to `parent_of.len() + 1`
/// steps so a malformed cyclic listing cannot loop forever (it just yields an imprecise depth).
fn depth_of(uuid: Uuid, parent_of: &HashMap<Uuid, Uuid>, root: Uuid) -> u32 {
	let mut depth = 0;
	let mut current = uuid;
	for _ in 0..=parent_of.len() {
		match parent_of.get(&current) {
			Some(&parent) if parent != root => {
				current = parent;
				depth += 1;
			}
			// Reached the sync root, or a parent not present in the listing (defensive).
			_ => break,
		}
	}
	depth
}

fn insert_diff_row(
	stmt: &mut CachedStatement<'_>,
	uuid: Uuid,
	parent: Option<Uuid>,
	item_type: ItemType,
	content_hash: &[u8],
) -> rusqlite::Result<()> {
	stmt.execute(params![uuid, parent, item_type as i8, content_hash])?;
	Ok(())
}

impl CacheState {
	/// Ensure the `diff_incoming` TEMP table exists on this connection and is empty, so each resync
	/// starts from a clean snapshot.
	pub(crate) fn reset_diff_incoming(&self) -> rusqlite::Result<()> {
		self.db
			.execute_batch(super::statements::DIFF_INCOMING_RESET)
	}

	/// Stage the listed remote directories into `diff_incoming`, chunked into transactions like
	/// `upsert_dirs`. Call [`reset_diff_incoming`](Self::reset_diff_incoming) first.
	pub(crate) fn insert_dirs_into_diff_incoming<'a>(
		&mut self,
		dirs: impl Iterator<Item = impl Borrow<CacheableDir<'a>>>,
	) -> rusqlite::Result<()> {
		self.execute_chunked(dirs, |transaction, chunk| {
			let mut stmt = transaction.prepare_cached(super::statements::DIFF_INCOMING_INSERT)?;
			for dir in chunk {
				let dir = dir.borrow();
				insert_diff_row(
					&mut stmt,
					dir.uuid,
					Some(dir.parent),
					ItemType::Dir,
					&dir.content_fingerprint(),
				)?;
			}
			Ok(())
		})
	}

	/// Stage the listed remote files into `diff_incoming`, chunked into transactions like
	/// `upsert_files`. Call [`reset_diff_incoming`](Self::reset_diff_incoming) first.
	pub(crate) fn insert_files_into_diff_incoming<'a>(
		&mut self,
		files: impl Iterator<Item = impl Borrow<CacheableFile<'a>>>,
	) -> rusqlite::Result<()> {
		self.execute_chunked(files, |transaction, chunk| {
			let mut stmt = transaction.prepare_cached(super::statements::DIFF_INCOMING_INSERT)?;
			for file in chunk {
				let file = file.borrow();
				insert_diff_row(
					&mut stmt,
					file.uuid,
					Some(file.parent),
					ItemType::File,
					&file.content_fingerprint(),
				)?;
			}
			Ok(())
		})
	}

	/// Deletions: cached items under the sync root that are absent from the listing → synthetic
	/// `Removed`, plus an orphan sweep for items the subtree walk cannot reach. Emits one `Removed` per
	/// vanished row; the cascade trigger makes any redundant child-deletes no-ops.
	fn resync_deletes(&self, anchor: Uuid) -> rusqlite::Result<Vec<CacheEvent<'static>>> {
		let mut events = Vec::new();

		let mut subtree_stmt = self
			.db
			.prepare_cached(super::statements::DIFF_SUBTREE_ABSENT)?;
		let rows = subtree_stmt.query_map(params![anchor], |row| {
			Ok((
				row.get::<_, Uuid>(ITEMS_UUID)?,
				row.get::<_, i8>(ITEMS_TYPE)?,
			))
		})?;
		for row in rows {
			let (uuid, item_type) = row?;
			if let Some(event) = build_removed_event(uuid, item_type) {
				events.push(event);
			}
		}
		drop(subtree_stmt);

		// Orphan sweep ONLY for the account-root resync. A broken-ancestry orphan cannot be attributed
		// to a sync root (its parent chain is gone), so a per-subdir-root resync cannot tell
		// whether an orphan belongs to THIS root or another — deleting it could wipe another root's item.
		// When `anchor` is the account root the whole account is in scope, so an orphan absent from the
		// listing is unambiguously gone. (A live orphan under a subdir root is instead re-created by
		// pass 2, since the listing carries it with its correct parent.)
		if anchor == self.root_uuid {
			let mut orphan_stmt = self
				.db
				.prepare_cached(super::statements::DIFF_ORPHANS_ABSENT)?;
			let rows = orphan_stmt.query_map([], |row| {
				Ok((
					row.get::<_, Uuid>(ITEMS_UUID)?,
					row.get::<_, i8>(ITEMS_TYPE)?,
				))
			})?;
			for row in rows {
				let (uuid, item_type) = row?;
				tracing::warn!(
					"resync: sweeping orphaned cached item {uuid} (broken ancestry, absent from listing)"
				);
				if let Some(event) = build_removed_event(uuid, item_type) {
					events.push(event);
				}
			}
		}

		Ok(events)
	}

	/// Creates and moves: listed items not cached → `New`; cached under a different parent → `Move`.
	/// Emitted parent-before-child by sorting on listed-tree depth, so a child's create never lands
	/// before its parent's create/move in the drain (`seq` = insertion order is the tiebreaker).
	fn resync_creates_and_moves(
		&self,
		anchor: Uuid,
		dirs: &HashMap<Uuid, CacheableDir<'static>>,
		files: &HashMap<Uuid, CacheableFile<'static>>,
	) -> rusqlite::Result<Vec<CacheEvent<'static>>> {
		// Parent map over the whole listing, for depth ordering.
		let mut parent_of: HashMap<Uuid, Uuid> = HashMap::with_capacity(dirs.len() + files.len());
		for dir in dirs.values() {
			parent_of.insert(dir.uuid, dir.parent);
		}
		for file in files.values() {
			parent_of.insert(file.uuid, file.parent);
		}

		let mut with_depth: Vec<(u32, CacheEvent<'static>)> = Vec::new();

		let mut push = |uuid: Uuid, item_type: i8, kind: UpsertKind| match build_upsert_event(
			uuid, item_type, kind, dirs, files,
		) {
			// Depth is measured from the sync-root `anchor` (the listing's top level has parent ==
			// anchor), so creates/moves still emit parent-before-child within this root's subtree.
			Some(event) => with_depth.push((depth_of(uuid, &parent_of, anchor), event)),
			// Impossible by construction (diff_incoming is staged from these very maps); an error here
			// means a created/moved item would be silently dropped → divergence.
			None => tracing::error!(
				"resync: listed item {uuid} missing from the conversion map; skipping upsert"
			),
		};

		let mut creates_stmt = self.db.prepare_cached(super::statements::DIFF_CREATES)?;
		let rows = creates_stmt.query_map([], |row| {
			Ok((
				row.get::<_, Uuid>(ITEMS_UUID)?,
				row.get::<_, i8>(ITEMS_TYPE)?,
			))
		})?;
		for row in rows {
			let (uuid, item_type) = row?;
			push(uuid, item_type, UpsertKind::New);
		}
		drop(creates_stmt);

		let mut moves_stmt = self.db.prepare_cached(super::statements::DIFF_MOVES)?;
		let rows = moves_stmt.query_map(params![anchor], |row| {
			Ok((
				row.get::<_, Uuid>(ITEMS_UUID)?,
				row.get::<_, i8>(ITEMS_TYPE)?,
			))
		})?;
		for row in rows {
			let (uuid, item_type) = row?;
			push(uuid, item_type, UpsertKind::Move);
		}
		drop(moves_stmt);

		with_depth.sort_by_key(|(depth, _)| *depth);
		Ok(with_depth.into_iter().map(|(_, event)| event).collect())
	}

	/// Content changes: listed items cached at the SAME parent whose fingerprint differs → `Changed`.
	/// Mutually exclusive with the move query, so no item is double-emitted.
	fn resync_content_changes(
		&self,
		anchor: Uuid,
		dirs: &HashMap<Uuid, CacheableDir<'static>>,
		files: &HashMap<Uuid, CacheableFile<'static>>,
	) -> rusqlite::Result<Vec<CacheEvent<'static>>> {
		let mut events = Vec::new();
		let mut stmt = self
			.db
			.prepare_cached(super::statements::DIFF_CONTENT_CHANGES)?;
		let rows = stmt.query_map(params![anchor], |row| {
			Ok((
				row.get::<_, Uuid>(ITEMS_UUID)?,
				row.get::<_, i8>(ITEMS_TYPE)?,
			))
		})?;
		for row in rows {
			let (uuid, item_type) = row?;
			match build_upsert_event(uuid, item_type, UpsertKind::Changed, dirs, files) {
				Some(event) => events.push(event),
				None => tracing::error!(
					"resync: changed item {uuid} missing from the conversion map; skipping upsert"
				),
			}
		}
		Ok(events)
	}

	/// Run all three diff queries and return the synthetic events in apply order: creates/moves
	/// (parent-first), THEN deletions, THEN content changes. The caller persists them with `id: None` in
	/// this order — `seq` (insertion order) is the drain's tiebreaker for synthetics.
	///
	/// Creates/moves MUST precede deletions: a descendant that moves OUT of a directory deleted in the
	/// same resync would otherwise be cascade-deleted (the cascade trigger removes a deleted dir's
	/// children) before its `Move` re-parents it — losing the subtree and breaking item-id stability.
	/// Re-parenting first leaves the doomed directory childless, so its delete cascades into nothing.
	/// Content changes are same-parent upserts on surviving items, so their position relative to deletes
	/// is irrelevant (the two never touch the same uuid).
	pub(crate) fn compute_resync_synthetics(
		&self,
		anchor: Uuid,
		dirs: &HashMap<Uuid, CacheableDir<'static>>,
		files: &HashMap<Uuid, CacheableFile<'static>>,
	) -> rusqlite::Result<Vec<CacheEvent<'static>>> {
		let mut events = self.resync_creates_and_moves(anchor, dirs, files)?;
		events.extend(self.resync_deletes(anchor)?);
		events.extend(self.resync_content_changes(anchor, dirs, files)?);
		Ok(events)
	}
}

#[cfg(test)]
// Tests pass fixtures as `&[x.clone()]` list literals; the uniform array form reads better than
// mixing in `std::slice::from_ref` for the single-element cases.
#[allow(clippy::cloned_ref_to_slice_refs)]
mod tests;
