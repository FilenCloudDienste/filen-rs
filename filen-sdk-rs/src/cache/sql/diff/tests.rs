use std::borrow::Cow;

use crate::{crypto::file::FileKey, fs::dir::cache::CacheableDir};
use chrono::Utc;
use filen_types::{api::v3::dir::color::DirColor, auth::FileEncryptionVersion, fs::StableUuid};

use super::*;
use crate::cache::sql::columns::{COUNT, ITEM_EXISTS, ITEMS_CONTENT_HASH, ITEMS_PARENT};

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
		stable_uuid: StableUuid::new_for_test(Uuid::from_u128(uuid)),
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
		key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3).unwrap(),
		last_modified: now,
		created: Some(now),
		hash: None,
	}
}

fn diff_count(state: &CacheState) -> i64 {
	state
		.db
		.query_row("SELECT COUNT(*) AS count FROM diff_incoming", [], |r| {
			r.get(COUNT)
		})
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
			|r| {
				Ok((
					r.get(ITEMS_PARENT)?,
					r.get(ITEMS_TYPE)?,
					r.get(ITEMS_CONTENT_HASH)?,
				))
			},
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
			|r| Ok((r.get(ITEMS_TYPE)?, r.get(ITEMS_CONTENT_HASH)?)),
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
			"SELECT EXISTS (SELECT 1 FROM diff_incoming WHERE uuid = ?) AS item_exists",
			params![Uuid::from_u128(9)],
			|r| r.get(ITEM_EXISTS),
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
		.query_map(params![root], |r| r.get::<_, Uuid>(ITEMS_UUID))
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
