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

use super::item::ItemType;
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
		log::error!("resync: unexpected item type {item_type} for {uuid}; skipping deletion");
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
		log::error!("resync: unexpected item type {item_type} for {uuid}; skipping upsert");
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
			Ok((row.get::<_, Uuid>(0)?, row.get::<_, i8>(1)?))
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
			let rows = orphan_stmt
				.query_map([], |row| Ok((row.get::<_, Uuid>(0)?, row.get::<_, i8>(1)?)))?;
			for row in rows {
				let (uuid, item_type) = row?;
				log::warn!(
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
			None => log::error!(
				"resync: listed item {uuid} missing from the conversion map; skipping upsert"
			),
		};

		let mut creates_stmt = self.db.prepare_cached(super::statements::DIFF_CREATES)?;
		let rows =
			creates_stmt.query_map([], |row| Ok((row.get::<_, Uuid>(0)?, row.get::<_, i8>(1)?)))?;
		for row in rows {
			let (uuid, item_type) = row?;
			push(uuid, item_type, UpsertKind::New);
		}
		drop(creates_stmt);

		let mut moves_stmt = self.db.prepare_cached(super::statements::DIFF_MOVES)?;
		let rows = moves_stmt.query_map(params![anchor], |row| {
			Ok((row.get::<_, Uuid>(0)?, row.get::<_, i8>(1)?))
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
			Ok((row.get::<_, Uuid>(0)?, row.get::<_, i8>(1)?))
		})?;
		for row in rows {
			let (uuid, item_type) = row?;
			match build_upsert_event(uuid, item_type, UpsertKind::Changed, dirs, files) {
				Some(event) => events.push(event),
				None => log::error!(
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
mod tests {
	use std::borrow::Cow;

	use crate::{crypto::file::FileKey, fs::dir::cache::CacheableDir};
	use chrono::Utc;
	use filen_types::{api::v3::dir::color::DirColor, auth::FileEncryptionVersion};

	use super::*;

	fn make_dir(uuid: u128, parent: Uuid) -> CacheableDir<'static> {
		let now = Utc::now();
		CacheableDir {
			uuid: Uuid::from_u128(uuid),
			parent,
			color: DirColor::Default,
			favorited: false,
			timestamp: now,
			name: Cow::Owned(format!("dir_{uuid}")),
			created: Some(now),
		}
	}

	fn make_file(uuid: u128, parent: Uuid) -> CacheableFile<'static> {
		let now = Utc::now();
		CacheableFile {
			uuid: Uuid::from_u128(uuid),
			parent,
			chunks_size: 1024,
			chunks: 1,
			favorited: false,
			region: Cow::Borrowed("us-east-1"),
			bucket: Cow::Borrowed("bucket"),
			timestamp: now,
			name: Cow::Owned(format!("file_{uuid}.txt")),
			size: 1024,
			mime: Cow::Borrowed("text/plain"),
			key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3)
				.unwrap(),
			last_modified: now,
			created: Some(now),
			hash: None,
		}
	}

	fn diff_count(state: &CacheState) -> i64 {
		state
			.db
			.query_row("SELECT COUNT(*) FROM diff_incoming", [], |r| r.get(0))
			.unwrap()
	}

	#[test]
	fn reset_creates_empty_table() {
		let state = CacheState::new_in_memory();
		state.reset_diff_incoming().unwrap();
		assert_eq!(diff_count(&state), 0);
	}

	#[test]
	fn insert_stages_projection_with_fingerprint() {
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		state.reset_diff_incoming().unwrap();

		let dir = make_dir(1, root);
		let file = make_file(2, dir.uuid);
		state
			.insert_dirs_into_diff_incoming(std::iter::once(&dir))
			.unwrap();
		state
			.insert_files_into_diff_incoming(std::iter::once(&file))
			.unwrap();

		assert_eq!(diff_count(&state), 2);

		let (parent, ty, hash): (Vec<u8>, i8, Vec<u8>) = state
			.db
			.query_row(
				"SELECT parent, type, content_hash FROM diff_incoming WHERE uuid = ?",
				params![file.uuid],
				|r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
			)
			.unwrap();
		assert_eq!(parent.as_slice(), dir.uuid.as_bytes());
		assert_eq!(ty, ItemType::File as i8);
		assert_eq!(hash.as_slice(), file.content_fingerprint().as_slice());

		let (ty, hash): (i8, Vec<u8>) = state
			.db
			.query_row(
				"SELECT type, content_hash FROM diff_incoming WHERE uuid = ?",
				params![dir.uuid],
				|r| Ok((r.get(0)?, r.get(1)?)),
			)
			.unwrap();
		assert_eq!(ty, ItemType::Dir as i8);
		assert_eq!(hash.as_slice(), dir.content_fingerprint().as_slice());
	}

	#[test]
	fn reset_clears_previous_snapshot() {
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		state.reset_diff_incoming().unwrap();
		state
			.insert_dirs_into_diff_incoming(std::iter::once(make_dir(1, root)))
			.unwrap();
		assert_eq!(diff_count(&state), 1);

		// A second resync resets to an empty snapshot, then loads its own rows.
		state.reset_diff_incoming().unwrap();
		assert_eq!(diff_count(&state), 0, "reset empties the prior snapshot");
		state
			.insert_dirs_into_diff_incoming(std::iter::once(make_dir(9, root)))
			.unwrap();
		let survivor: bool = state
			.db
			.query_row(
				"SELECT EXISTS (SELECT 1 FROM diff_incoming WHERE uuid = ?)",
				params![Uuid::from_u128(9)],
				|r| r.get(0),
			)
			.unwrap();
		assert!(survivor, "only the second snapshot's row remains");
	}

	#[test]
	fn table_persists_across_calls_without_reset() {
		// TEMP table lives for the connection: inserting without an intervening reset accumulates.
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		state.reset_diff_incoming().unwrap();
		state
			.insert_dirs_into_diff_incoming(std::iter::once(make_dir(1, root)))
			.unwrap();
		state
			.insert_dirs_into_diff_incoming(std::iter::once(make_dir(2, root)))
			.unwrap();
		assert_eq!(diff_count(&state), 2);
	}

	/// Upsert the given dirs+files into the cached `items` (the "before" state).
	fn put_items(
		state: &mut CacheState,
		dirs: &[CacheableDir<'static>],
		files: &[CacheableFile<'static>],
	) {
		state.upsert_dirs(dirs.iter()).unwrap();
		state.upsert_files(files.iter()).unwrap();
	}

	/// Stage the given dirs+files as the fresh listing snapshot (the "after" state).
	fn stage_diff(
		state: &mut CacheState,
		dirs: &[CacheableDir<'static>],
		files: &[CacheableFile<'static>],
	) {
		state.reset_diff_incoming().unwrap();
		state.insert_dirs_into_diff_incoming(dirs.iter()).unwrap();
		state.insert_files_into_diff_incoming(files.iter()).unwrap();
	}

	fn dir_map(dirs: &[CacheableDir<'static>]) -> HashMap<Uuid, CacheableDir<'static>> {
		dirs.iter().map(|d| (d.uuid, d.clone())).collect()
	}

	fn file_map(files: &[CacheableFile<'static>]) -> HashMap<Uuid, CacheableFile<'static>> {
		files.iter().map(|f| (f.uuid, f.clone())).collect()
	}

	/// Classify a synthetic into `(kind, uuid)` and assert it carries `id: None`.
	fn summarize(event: &CacheEvent<'static>) -> (&'static str, Uuid) {
		assert!(event.id.is_none(), "resync synthetics must have id = None");
		match &event.event {
			CacheEventType::Dir(DirEvent::New(d)) => ("dir_new", d.uuid),
			CacheEventType::Dir(DirEvent::Move(d)) => ("dir_move", d.uuid),
			CacheEventType::Dir(DirEvent::Changed(d)) => ("dir_changed", d.uuid),
			CacheEventType::Dir(DirEvent::Removed(u)) => ("dir_removed", *u),
			CacheEventType::File(FileEvent::New(f)) => ("file_new", f.uuid),
			CacheEventType::File(FileEvent::Move(f)) => ("file_move", f.uuid),
			CacheEventType::File(FileEvent::Changed(f)) => ("file_changed", f.uuid),
			CacheEventType::File(FileEvent::Removed(u)) => ("file_removed", *u),
			other => panic!("unexpected synthetic event: {other:?}"),
		}
	}

	fn summaries(events: &[CacheEvent<'static>]) -> Vec<(&'static str, Uuid)> {
		events.iter().map(summarize).collect()
	}

	#[test]
	fn pass1_emits_removed_for_subtree_items_absent_from_listing() {
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		let dir = make_dir(1, root);
		let file = make_file(2, dir.uuid);
		put_items(&mut state, &[dir.clone()], &[file.clone()]);

		// Listing still has the dir, but the file vanished.
		stage_diff(&mut state, &[dir.clone()], &[]);

		let events = state.resync_deletes(state.root_uuid).unwrap();
		assert_eq!(summaries(&events), vec![("file_removed", file.uuid)]);
	}

	#[test]
	fn pass1_emits_removed_for_every_vanished_node() {
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		let dir = make_dir(1, root);
		let file = make_file(2, dir.uuid);
		put_items(&mut state, &[dir.clone()], &[file.clone()]);

		// Empty listing: the whole subtree vanished. Every node is emitted (cascade dedups at apply).
		stage_diff(&mut state, &[], &[]);

		let mut got = summaries(&state.resync_deletes(state.root_uuid).unwrap());
		got.sort();
		let mut want = vec![("dir_removed", dir.uuid), ("file_removed", file.uuid)];
		want.sort();
		assert_eq!(got, want);
	}

	#[test]
	fn pass1_keeps_present_items() {
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		let dir = make_dir(1, root);
		let file = make_file(2, dir.uuid);
		put_items(&mut state, &[dir.clone()], &[file.clone()]);
		stage_diff(&mut state, &[dir.clone()], &[file.clone()]);

		assert!(
			state.resync_deletes(state.root_uuid).unwrap().is_empty(),
			"nothing vanished, so no deletions"
		);
	}

	#[test]
	fn pass1_never_emits_the_sync_root() {
		// Even with an empty listing, the root row is never a deletion target.
		let mut state = CacheState::new_in_memory();
		stage_diff(&mut state, &[], &[]);
		assert!(state.resync_deletes(state.root_uuid).unwrap().is_empty());
	}

	#[test]
	fn pass1_orphan_sweep_removes_unreachable_absent_items() {
		let mut state = CacheState::new_in_memory();
		// An orphan: parent points at a uuid that is not in `items`, so the subtree CTE can't reach it.
		let orphan_parent = Uuid::from_u128(0xDEAD);
		let orphan = make_dir(1, orphan_parent);
		put_items(&mut state, &[orphan.clone()], &[]);
		stage_diff(&mut state, &[], &[]); // absent from the listing

		let events = state.resync_deletes(state.root_uuid).unwrap();
		assert_eq!(summaries(&events), vec![("dir_removed", orphan.uuid)]);
	}

	#[test]
	fn pass2_emits_new_for_listed_items_not_cached() {
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		let dir = make_dir(1, root);
		let file = make_file(2, dir.uuid);
		// Nothing cached; listing has both.
		stage_diff(&mut state, &[dir.clone()], &[file.clone()]);

		let dirs = dir_map(&[dir.clone()]);
		let files = file_map(&[file.clone()]);
		let mut got = summaries(
			&state
				.resync_creates_and_moves(state.root_uuid, &dirs, &files)
				.unwrap(),
		);
		got.sort();
		let mut want = vec![("dir_new", dir.uuid), ("file_new", file.uuid)];
		want.sort();
		assert_eq!(got, want);
	}

	#[test]
	fn pass2_orders_creates_parent_before_child() {
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		// A 3-deep chain plus a leaf file, all new. Must be emitted shallowest-first.
		let a = make_dir(1, root);
		let b = make_dir(2, a.uuid);
		let c = make_dir(3, b.uuid);
		let f = make_file(4, c.uuid);
		stage_diff(&mut state, &[c.clone(), a.clone(), b.clone()], &[f.clone()]);

		let dirs = dir_map(&[a.clone(), b.clone(), c.clone()]);
		let files = file_map(&[f.clone()]);
		let events = state
			.resync_creates_and_moves(state.root_uuid, &dirs, &files)
			.unwrap();
		let order: Vec<Uuid> = events.iter().map(|e| summarize(e).1).collect();

		// a before b before c before f (each parent precedes its child).
		let pos = |u: Uuid| order.iter().position(|x| *x == u).unwrap();
		assert!(pos(a.uuid) < pos(b.uuid));
		assert!(pos(b.uuid) < pos(c.uuid));
		assert!(pos(c.uuid) < pos(f.uuid));
	}

	/// A malformed listing that stages the sync root with a non-NULL parent would match `DIFF_MOVES` via
	/// `i.parent (NULL) IS NOT d.parent`. The `d.uuid != ?1` guard keeps the root out of the move set, so
	/// it is never emitted (and never churns every resync).
	#[test]
	fn pass2_query_excludes_the_sync_root() {
		let state = CacheState::new_in_memory();
		let root = state.root_uuid;
		state.reset_diff_incoming().unwrap();
		state
			.db
			.execute(
				"INSERT INTO diff_incoming (uuid, parent, type, content_hash) VALUES (?1, ?2, 0, ?3)",
				params![root, Uuid::from_u128(0xBEEF), [9u8; 32].as_slice()],
			)
			.unwrap();

		let moved: Vec<Uuid> = state
			.db
			.prepare(super::super::statements::DIFF_MOVES)
			.unwrap()
			.query_map(params![root], |r| r.get::<_, Uuid>(0))
			.unwrap()
			.collect::<Result<_, _>>()
			.unwrap();
		assert!(
			moved.is_empty(),
			"DIFF_MOVES must exclude the sync root, got {moved:?}"
		);
	}

	#[test]
	fn pass2_emits_move_when_parent_changed() {
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		let p1 = make_dir(1, root);
		let p2 = make_dir(2, root);
		let moved = make_dir(3, p1.uuid);
		put_items(&mut state, &[p1.clone(), p2.clone(), moved.clone()], &[]);

		// `moved` now lives under p2.
		let moved_new = make_dir(3, p2.uuid);
		stage_diff(
			&mut state,
			&[p1.clone(), p2.clone(), moved_new.clone()],
			&[],
		);

		let dirs = dir_map(&[p1.clone(), p2.clone(), moved_new.clone()]);
		let files = file_map(&[]);
		let events = state
			.resync_creates_and_moves(state.root_uuid, &dirs, &files)
			.unwrap();
		assert_eq!(summaries(&events), vec![("dir_move", moved.uuid)]);
		// The Move carries the NEW parent.
		match &events[0].event {
			CacheEventType::Dir(DirEvent::Move(d)) => assert_eq!(d.parent, p2.uuid),
			other => panic!("expected dir move, got {other:?}"),
		}
	}

	#[test]
	fn pass3_emits_changed_when_fingerprint_differs() {
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		let mut file = make_file(1, root);
		put_items(&mut state, &[], &[file.clone()]);

		// Same uuid + parent, larger size → fingerprint differs.
		file.size = 9999;
		file.chunks_size = 9999;
		stage_diff(&mut state, &[], &[file.clone()]);

		let files = file_map(&[file.clone()]);
		let events = state
			.resync_content_changes(state.root_uuid, &dir_map(&[]), &files)
			.unwrap();
		assert_eq!(summaries(&events), vec![("file_changed", file.uuid)]);
	}

	#[test]
	fn pass3_ignores_unchanged_items() {
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		let file = make_file(1, root);
		let dir = make_dir(2, root);
		put_items(&mut state, &[dir.clone()], &[file.clone()]);
		stage_diff(&mut state, &[dir.clone()], &[file.clone()]);

		assert!(
			state
				.resync_content_changes(state.root_uuid, &dir_map(&[dir]), &file_map(&[file]))
				.unwrap()
				.is_empty(),
			"identical fingerprints → no content-change synthetics"
		);
	}

	#[test]
	fn move_and_content_change_emits_only_one_move() {
		// A moved item whose content also changed must be a single Move (pass 2), never a double-emit.
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		let p2 = make_dir(1, root);
		let mut file = make_file(2, root); // cached under root
		put_items(&mut state, &[p2.clone()], &[file.clone()]);

		// Now under p2 AND larger.
		file.parent = p2.uuid;
		file.size = 5555;
		file.chunks_size = 5555;
		stage_diff(&mut state, &[p2.clone()], &[file.clone()]);

		let dirs = dir_map(&[p2.clone()]);
		let files = file_map(&[file.clone()]);
		let moves = summaries(
			&state
				.resync_creates_and_moves(state.root_uuid, &dirs, &files)
				.unwrap(),
		);
		let changes = summaries(
			&state
				.resync_content_changes(state.root_uuid, &dirs, &files)
				.unwrap(),
		);
		assert_eq!(moves, vec![("file_move", file.uuid)]);
		assert!(
			changes.is_empty(),
			"a moved item must not also emit Changed"
		);
	}

	#[test]
	fn compute_resync_synthetics_orders_creates_then_deletes_then_changes() {
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		let keep = make_dir(1, root); // unchanged
		let gone = make_file(2, root); // will vanish → delete
		let mut changed = make_file(3, root); // will change → changed
		put_items(
			&mut state,
			&[keep.clone()],
			&[gone.clone(), changed.clone()],
		);

		let created = make_dir(4, root); // new → create
		changed.size = 7777;
		changed.chunks_size = 7777;
		stage_diff(
			&mut state,
			&[keep.clone(), created.clone()],
			&[changed.clone()],
		);

		let dirs = dir_map(&[keep, created.clone()]);
		let files = file_map(&[changed.clone()]);
		let events = state
			.compute_resync_synthetics(state.root_uuid, &dirs, &files)
			.unwrap();
		assert_eq!(
			summaries(&events),
			vec![
				("dir_new", created.uuid),
				("file_removed", gone.uuid),
				("file_changed", changed.uuid),
			]
		);
	}

	#[test]
	fn compute_resync_synthetics_is_empty_when_converged() {
		let mut state = CacheState::new_in_memory();
		let root = state.root_uuid;
		let dir = make_dir(1, root);
		let file = make_file(2, dir.uuid);
		put_items(&mut state, &[dir.clone()], &[file.clone()]);
		stage_diff(&mut state, &[dir.clone()], &[file.clone()]);

		let events = state
			.compute_resync_synthetics(state.root_uuid, &dir_map(&[dir]), &file_map(&[file]))
			.unwrap();
		assert!(
			events.is_empty(),
			"a listing identical to the cache must emit zero synthetics"
		);
	}
}
