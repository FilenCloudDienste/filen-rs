use super::*;

fn item_count(state: &CacheState) -> i64 {
	state
		.db
		.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
		.unwrap()
}

/// Async-bridging guard. The worker runs on a `std::thread` (not a runtime thread)
/// and drives the async resync island through a `tokio::runtime::Handle::block_on` captured at
/// construction (`CacheHandle::new` calls `Handle::current()` inside the app's multi-threaded
/// runtime; that handle is stashed in [`ResyncDeps`]). This locks the invariant that such a handle
/// can be `block_on`'d from a NON-runtime thread without the "cannot block_on from within a runtime"
/// panic — i.e. the bridging mechanism the worker relies on is sound.
#[test]
fn captured_runtime_handle_block_on_works_from_a_worker_thread() {
	let rt = tokio::runtime::Builder::new_multi_thread()
		.worker_threads(1)
		.build()
		.unwrap();
	let rt_handle = rt.handle().clone();

	// Mimic the worker: a plain std::thread (no async context) drives a future to completion.
	let result = std::thread::spawn(move || rt_handle.block_on(async { 40 + 2 }))
		.join()
		.unwrap();
	assert_eq!(result, 42);
}

/// A `FrontierAdvance` is persisted as a `NoOp` marker and applied by the drain: it moves
/// the watermark past a non-cacheable drive event but must never mutate `items`.
#[test]
fn frontier_advance_advances_watermark_without_mutating_items() {
	let mut state = CacheState::new_in_memory();
	let before = item_count(&state);

	state.drain_pending(Some(CacheThreadEvent::Socket(
		CacheEventMaybeDecrypted::FrontierAdvance { id: 42 },
	)));

	assert_eq!(
		item_count(&state),
		before,
		"FrontierAdvance must not mutate items"
	);
	assert_eq!(
		state.watermark().unwrap(),
		Some(42),
		"FrontierAdvance advances the watermark"
	);
}

/// a `FileMove` whose new parent is a non-navigable virtual container (here `Links`)
/// takes the file out of the synced tree, so it must convert to `FileEvent::Removed` rather than
/// failing conversion (which would make it a frontier-advance-only event and leave a stale row).
#[test]
fn file_move_to_virtual_parent_becomes_removed() {
	use std::borrow::Cow;

	use chrono::Utc;
	use filen_sdk_rs::{
		crypto::file::FileKey,
		fs::file::meta::{DecryptedFileMeta, FileMeta},
		io::RemoteFile,
		socket::{DecryptedDriveEvent, DecryptedSocketEvent, FileMove},
	};
	use filen_types::{auth::FileEncryptionVersion, fs::ParentUuid, fs::UuidStr};

	let uuid = UuidStr::new_v4();
	let expected: Uuid = (&uuid).into();
	let file = RemoteFile {
		uuid,
		parent: ParentUuid::Links,
		size: 10,
		favorited: false,
		region: "de-1".to_string(),
		bucket: "bucket-a".to_string(),
		timestamp: Utc::now(),
		chunks: 1,
		meta: FileMeta::Decoded(DecryptedFileMeta {
			name: Cow::Borrowed("moved.txt"),
			size: 10,
			mime: Cow::Borrowed("text/plain"),
			key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3)
				.unwrap(),
			last_modified: Utc::now(),
			created: None,
			hash: None,
		}),
	};
	let socket_event = DecryptedSocketEvent::Drive {
		inner: DecryptedDriveEvent::FileMove(FileMove(file)),
		drive_message_id: 5,
	};

	match CacheEventMaybeDecrypted::from_decrypted_event(&socket_event) {
		Some(CacheEventMaybeDecrypted::Decrypted(CacheEvent {
			id: Some(5),
			event: CacheEventType::File(FileEvent::Removed(removed)),
		})) => assert_eq!(removed, expected),
		other => panic!("expected a File(Removed) event, got {other:?}"),
	}
}

/// Symmetric to the file case: a `FolderMove` whose new parent is a non-navigable virtual container
/// (`Links`) leaves the synced tree, so it must convert to `DirEvent::Removed` — otherwise a stale
/// subtree (including any nested sync root) would be left behind.
#[test]
fn folder_move_to_virtual_parent_becomes_removed() {
	use std::borrow::Cow;

	use chrono::Utc;
	use filen_sdk_rs::{
		fs::dir::meta::{DecryptedDirectoryMeta, DirectoryMeta},
		io::RemoteDirectory,
		socket::{DecryptedDriveEvent, DecryptedSocketEvent, FolderMove},
	};
	use filen_types::{api::v3::dir::color::DirColor, fs::ParentUuid, fs::UuidStr};

	let uuid = UuidStr::new_v4();
	let expected: Uuid = (&uuid).into();
	let dir = RemoteDirectory::from_meta(
		uuid,
		ParentUuid::Links, // moved into a virtual container
		DirColor::Default,
		false,
		Utc::now(),
		DirectoryMeta::Decoded(DecryptedDirectoryMeta {
			name: Cow::Borrowed("moved-folder"),
			created: None,
		}),
	);
	let socket_event = DecryptedSocketEvent::Drive {
		inner: DecryptedDriveEvent::FolderMove(FolderMove(dir)),
		drive_message_id: 7,
	};

	match CacheEventMaybeDecrypted::from_decrypted_event(&socket_event) {
		Some(CacheEventMaybeDecrypted::Decrypted(CacheEvent {
			id: Some(7),
			event: CacheEventType::Dir(DirEvent::Removed(removed)),
		})) => assert_eq!(removed, expected),
		other => panic!("expected a Dir(Removed) event, got {other:?}"),
	}
}

/// A `DirEvent::New` that upserts one item+dir row (observable as a single `items` row).
fn dir_new_event(id: Option<u64>, uuid: Uuid, parent: Uuid) -> CacheEvent<'static> {
	use std::borrow::Cow;

	use chrono::Utc;
	use filen_sdk_rs::fs::dir::cache::CacheableDir;
	use filen_types::api::v3::dir::color::DirColor;

	let dir = CacheableDir {
		uuid,
		parent,
		color: DirColor::Default,
		favorited: false,
		timestamp: Utc::now(),
		name: Cow::Owned(format!("dir-{uuid}")),
		created: None,
	};
	CacheEvent {
		id,
		event: CacheEventType::Dir(DirEvent::New(dir)),
	}
}

fn cache_dir(uuid: u128, parent: Uuid) -> CacheableDir<'static> {
	use std::borrow::Cow;

	use chrono::Utc;
	use filen_types::api::v3::dir::color::DirColor;

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

fn cache_file(uuid: u128, parent: Uuid, size: u64) -> CacheableFile<'static> {
	use std::borrow::Cow;

	use chrono::Utc;
	use filen_sdk_rs::crypto::file::FileKey;
	use filen_types::auth::FileEncryptionVersion;

	CacheableFile {
		uuid: Uuid::from_u128(uuid),
		parent,
		chunks_size: size,
		chunks: 1,
		favorited: false,
		region: Cow::Borrowed("us-east-1"),
		bucket: Cow::Borrowed("bucket"),
		timestamp: Utc::now(),
		name: Cow::Owned(format!("file-{uuid}.txt")),
		size,
		mime: Cow::Borrowed("text/plain"),
		key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3).unwrap(),
		last_modified: Utc::now(),
		created: None,
		hash: None,
	}
}

fn item_exists(state: &CacheState, uuid: Uuid) -> bool {
	state
		.db
		.query_row(
			"SELECT EXISTS (SELECT 1 FROM items WHERE uuid = ?)",
			[uuid],
			|row| row.get(0),
		)
		.unwrap()
}

fn item_parent(state: &CacheState, uuid: Uuid) -> Option<Uuid> {
	state
		.db
		.query_row("SELECT parent FROM items WHERE uuid = ?", [uuid], |row| {
			row.get::<_, Option<Uuid>>(0)
		})
		.unwrap()
}

fn item_content_hash(state: &CacheState, uuid: Uuid) -> Option<Vec<u8>> {
	state
		.db
		.query_row(
			"SELECT content_hash FROM items WHERE uuid = ?",
			[uuid],
			|row| row.get(0),
		)
		.unwrap()
}

/// End-to-end self-heal: a divergent cache + a `needs_resync` flag → `apply_resync` converges
/// the cache to the listing, advances the watermark to the snapshot id, clears the flag, and drains
/// the synthetics. A second resync over the same listing is a no-op (idempotent convergence).
#[test]
fn resync_self_heals_cache_to_listing_and_is_idempotent() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;

	// Cached "before": A under root; F1 under A; F_gone under root.
	let a = cache_dir(1, root);
	let f1_old = cache_file(2, a.uuid, 100);
	let f_gone = cache_file(3, root, 100);
	state.upsert_dirs(once(&a)).unwrap();
	state.upsert_files([&f1_old, &f_gone].into_iter()).unwrap();
	// Simulate a detected gap that triggered the resync.
	state.mark_needs_resync().unwrap();

	// Listing "after": A (unchanged); F1 (changed size); F_new under A; B new under root;
	// F_gone vanished.
	let f1_new = cache_file(2, a.uuid, 999);
	let f_new = cache_file(4, a.uuid, 100);
	let b = cache_dir(5, root);
	let dirs = vec![a.clone(), b.clone()];
	let files = vec![f1_new.clone(), f_new.clone()];

	state
		.apply_resync(vec![(root, dirs.clone(), files.clone())], 100)
		.unwrap();

	// Converged: vanished file removed; new items created; changed file's hash refreshed.
	assert!(item_exists(&state, a.uuid));
	assert!(item_exists(&state, b.uuid), "new dir created");
	assert!(item_exists(&state, f_new.uuid), "new file created");
	assert!(item_exists(&state, f1_new.uuid));
	assert!(!item_exists(&state, f_gone.uuid), "vanished file deleted");
	assert_eq!(
		item_content_hash(&state, f1_new.uuid),
		Some(f1_new.content_fingerprint().to_vec()),
		"changed file's stored fingerprint is refreshed"
	);
	// root + A + B + F1 + F_new = 5 items.
	assert_eq!(item_count(&state), 5);

	// Watermark advanced to the snapshot id; flag cleared; synthetics drained.
	assert_eq!(state.watermark().unwrap(), Some(100));
	assert!(!state.needs_resync().unwrap());
	assert!(
		state.load_event_batch(10).unwrap().0.is_empty(),
		"synthetics were drained"
	);

	// Idempotent convergence: a second resync over the SAME listing emits zero synthetics.
	state.reset_diff_incoming().unwrap();
	state.insert_dirs_into_diff_incoming(dirs.iter()).unwrap();
	state.insert_files_into_diff_incoming(files.iter()).unwrap();
	let dir_map: HashMap<Uuid, CacheableDir<'static>> =
		dirs.iter().map(|d| (d.uuid, d.clone())).collect();
	let file_map: HashMap<Uuid, CacheableFile<'static>> =
		files.iter().map(|f| (f.uuid, f.clone())).collect();
	let synthetics = state
		.compute_resync_synthetics(root, &dir_map, &file_map)
		.unwrap();
	assert!(
		synthetics.is_empty(),
		"a converged listing must yield zero synthetics, got {synthetics:?}"
	);
}

/// When a parent is removed in the SAME resync that a descendant moves OUT
/// of it, the moved subtree must NOT be lost to the cascade-delete. Creates/moves must apply BEFORE
/// deletes, so the child is re-parented before its old parent (and the cascade) fire.
#[test]
fn resync_move_out_of_deleted_parent_preserves_subtree() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;

	// Cached: D_old → M → F (a subtree under a dir that will be deleted).
	let d_old = cache_dir(1, root);
	let m = cache_dir(2, d_old.uuid);
	let f = cache_file(3, m.uuid, 100);
	state.upsert_dirs([&d_old, &m].into_iter()).unwrap();
	state.upsert_files(once(&f)).unwrap();
	state.mark_needs_resync().unwrap();

	// Listing: M moved to root (D_old gone); F still under M, unchanged (so F itself emits nothing).
	let m_moved = cache_dir(2, root);
	state
		.apply_resync(vec![(root, vec![m_moved], vec![f.clone()])], 100)
		.unwrap();

	assert!(!item_exists(&state, d_old.uuid), "deleted parent removed");
	assert!(item_exists(&state, m.uuid), "moved dir survives");
	assert!(
		item_exists(&state, f.uuid),
		"a descendant of the moved dir must survive the deleted parent's cascade"
	);
	assert_eq!(item_parent(&state, f.uuid), Some(m.uuid), "F stays under M");
}

/// The startup gap-check: resync iff a hole is flagged OR the remote drive id advanced past the
/// watermark. Crucially, an UNCHANGED drive id (remote == watermark) must NOT resync.
#[test]
fn startup_should_resync_gates_on_drive_id_advance() {
	let state = CacheState::new_in_memory();

	// Fresh cache (watermark None): a non-empty drive resyncs to populate; an empty drive does not.
	assert!(
		state.startup_should_resync(5000).unwrap(),
		"fresh cache + non-empty drive → resync"
	);
	assert!(
		!state.startup_should_resync(0).unwrap(),
		"fresh cache + empty drive → nothing to populate"
	);

	state.set_watermark(100).unwrap();
	assert!(
		!state.startup_should_resync(100).unwrap(),
		"remote == watermark → NO resync (nothing changed while offline)"
	);
	assert!(
		state.startup_should_resync(101).unwrap(),
		"remote > watermark → resync (offline changes)"
	);
	assert!(
		!state.startup_should_resync(99).unwrap(),
		"remote < watermark (anomalous) → no resync"
	);

	// A durably-flagged hole forces a resync even when the drive id did not advance.
	state.mark_needs_resync().unwrap();
	assert!(
		state.startup_should_resync(100).unwrap(),
		"a flagged hole overrides the equal-id skip"
	);
}

/// Membership gate: a `New` whose parent is inside a configured sync root is cached; one whose
/// parent is OUTSIDE every sync root is skipped (the upsert dropped) — but BOTH return `Ok` so the
/// drain treats them as applied and the watermark advances (an out-of-root event must never look
/// like a hole).
#[test]
fn membership_gate_skips_out_of_root_upserts() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;

	// Configure ONE sync root A under the account root (NOT the account root itself), and
	// materialize A so the ancestry walk has an anchor.
	let a = cache_dir(1, account_root);
	state.upsert_dirs(once(&a)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;

	// In-root file (parent = A) → cached.
	let in_root = cache_file(2, a.uuid, 100);
	assert!(
		state
			.apply_event(CacheEventType::File(FileEvent::New(in_root.clone())))
			.is_ok()
	);
	assert!(item_exists(&state, in_root.uuid), "in-root file is cached");

	// Out-of-root file (parent = the account root, which is NOT a configured sync root) → skipped,
	// but still Ok (so the watermark advances).
	let out_of_root = cache_file(3, account_root, 100);
	assert!(
		state
			.apply_event(CacheEventType::File(FileEvent::New(out_of_root.clone())))
			.is_ok(),
		"an out-of-root event is treated as applied so the watermark advances"
	);
	assert!(
		!item_exists(&state, out_of_root.uuid),
		"out-of-root file is NOT cached — the gate skipped the upsert"
	);
}

/// A `Move` of a cached item OUT of every sync root must DELETE the stale
/// cached row, not skip it (a skip would leave it under its old parent, where a later cascade-delete
/// of that parent would wrongly remove a still-live item).
#[test]
fn move_out_of_sync_root_deletes_the_stale_row() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	let file = cache_file(2, a.uuid, 100);
	state.upsert_dirs(once(&a)).unwrap();
	state.upsert_files(once(&file)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;
	assert!(item_exists(&state, file.uuid), "file starts cached under A");

	// Move the file OUT of A — to the account root, which is NOT a configured sync root.
	let moved = cache_file(2, account_root, 100);
	state
		.apply_event(CacheEventType::File(FileEvent::Move(moved)))
		.unwrap();

	assert!(
		!item_exists(&state, file.uuid),
		"the moved-out file is deleted, not left stale under A"
	);
}

/// A dir that IS a sync root, moved OUT of its containing root (to a parent outside every root), must
/// be RE-PARENTED (upsert), not deleted — it stays a configured root, so its subtree must survive.
/// (Without the `contains_key` short-circuit the parent gate would wipe the whole root.)
#[test]
fn move_of_a_sync_root_out_of_its_parent_keeps_its_subtree() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	// account_root → A(root) → B(nested root) → child.
	let a = cache_dir(1, account_root);
	let b = cache_dir(2, a.uuid);
	let child = cache_file(3, b.uuid, 100);
	state.upsert_dirs([&a, &b].into_iter()).unwrap();
	state.upsert_files(once(&child)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	sync_roots.insert(b.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;

	// Move sync root B out from under A to directly under the account root (NOT a configured root).
	let moved_b = cache_dir(2, account_root);
	state
		.apply_event(CacheEventType::Dir(DirEvent::Move(moved_b)))
		.unwrap();

	assert!(
		item_exists(&state, b.uuid),
		"the moved sync root B survives"
	);
	assert!(
		item_exists(&state, child.uuid),
		"B's subtree survives the move"
	);
	assert_eq!(
		item_parent(&state, b.uuid),
		Some(account_root),
		"B is re-parented to its new location"
	);
}

/// A sync-root dir that lives OUT of every other root (its parent is not in a sync root — e.g. it was
/// moved out) must still apply its own `Changed`: the gate's "the item itself is a root" exception
/// covers New/Changed, not only Move. (Without it, a `Changed` on such a root is dropped and its
/// metadata goes stale until the next resync.)
#[test]
fn changed_on_out_of_root_sync_root_dir_applies() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	// B is a sync root whose parent is the account root, which is NOT a sync root → B is out-of-root.
	let b = cache_dir(1, account_root);
	state.upsert_dirs(once(&b)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(b.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;

	let before = item_content_hash(&state, b.uuid);
	// Same uuid + parent, renamed → the fingerprint changes.
	let mut b_changed = cache_dir(1, account_root);
	b_changed.name = std::borrow::Cow::Owned("renamed".to_string());
	state
		.apply_event(CacheEventType::Dir(DirEvent::Changed(b_changed.clone())))
		.unwrap();

	assert_ne!(
		item_content_hash(&state, b.uuid),
		before,
		"the Changed must apply (the root's own metadata refreshes)"
	);
	assert_eq!(
		item_content_hash(&state, b.uuid),
		Some(b_changed.content_fingerprint().to_vec()),
		"the stored fingerprint matches the new state"
	);
}

/// A multi-root resync converges EACH sync root's subtree independently (anchored at that
/// root), and an item OUTSIDE every sync root is never touched by any root's resync.
#[test]
fn apply_resync_converges_each_sync_root_independently() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;

	// Two sibling sync roots A and B under the account root, plus a sibling S outside both.
	let a = cache_dir(1, account_root);
	let b = cache_dir(2, account_root);
	let s = cache_dir(3, account_root);
	// Cached subtrees: A → fA1, fA2 (fA2 will vanish); B → fB1; S → fS1 (out-of-root).
	let fa1 = cache_file(10, a.uuid, 100);
	let fa2 = cache_file(11, a.uuid, 100);
	let fb1 = cache_file(20, b.uuid, 100);
	let fs1 = cache_file(30, s.uuid, 100);
	state.upsert_dirs([&a, &b, &s].into_iter()).unwrap();
	state
		.upsert_files([&fa1, &fa2, &fb1, &fs1].into_iter())
		.unwrap();

	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	sync_roots.insert(b.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;

	// Listings: A → fA1 + new fA3 (fA2 gone); B → fB1 + new fB2.
	let fa3 = cache_file(12, a.uuid, 100);
	let fb2 = cache_file(21, b.uuid, 100);
	let per_root = vec![
		(a.uuid, vec![], vec![fa1.clone(), fa3.clone()]),
		(b.uuid, vec![], vec![fb1.clone(), fb2.clone()]),
	];
	state.apply_resync(per_root, 500).unwrap();

	// A converged.
	assert!(item_exists(&state, fa1.uuid));
	assert!(
		!item_exists(&state, fa2.uuid),
		"vanished file under A is deleted"
	);
	assert!(item_exists(&state, fa3.uuid), "new file under A is created");
	// B converged.
	assert!(item_exists(&state, fb1.uuid));
	assert!(item_exists(&state, fb2.uuid), "new file under B is created");
	// S is OUTSIDE both sync roots — its subtree is untouched (no root's deletes pass scopes it).
	assert!(
		item_exists(&state, s.uuid),
		"out-of-root sibling dir untouched"
	);
	assert!(
		item_exists(&state, fs1.uuid),
		"file under the out-of-root sibling untouched"
	);
	// One watermark advance across both roots.
	assert_eq!(state.watermark().unwrap(), Some(500));
}

/// An all-skipped resync (every root failed to list → empty `per_root`) still advances
/// the watermark and clears `needs_resync` — so a permanently-unreachable root cannot stall the
/// worker into a per-event resync loop — and it deletes nothing (the cached subtrees of unlisted
/// roots are left intact, not converged to empty).
#[test]
fn apply_resync_with_no_listings_advances_and_clears_without_deleting() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;
	let a = cache_dir(1, root);
	let f = cache_file(2, a.uuid, 100);
	state.upsert_dirs(once(&a)).unwrap();
	state.upsert_files(once(&f)).unwrap();
	state.mark_needs_resync().unwrap();

	// Every configured root was skipped (e.g. all unreachable) → nothing listed.
	state.apply_resync(vec![], 777).unwrap();

	assert!(
		item_exists(&state, a.uuid),
		"cached items are NOT deleted when nothing listed"
	);
	assert!(item_exists(&state, f.uuid));
	assert_eq!(
		state.watermark().unwrap(),
		Some(777),
		"watermark still advances so the gap-check is satisfied"
	);
	assert!(
		!state.needs_resync().unwrap(),
		"needs_resync is cleared → no per-event resync loop"
	);
}

/// evicting a sync root deletes its subtree but PROTECTS a still-active nested root — both
/// the nested root's subtree AND the intermediate path to it (else the cascade trigger would wipe
/// the nested root via a deleted intermediate dir).
#[test]
fn remove_sync_root_eviction_protects_nested_root() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;

	// Tree: account_root → A → I → B(nested root). A-only + I-only files; B has a child.
	let a = cache_dir(1, account_root);
	let i = cache_dir(2, a.uuid);
	let b = cache_dir(3, i.uuid);
	let a_only = cache_file(10, a.uuid, 100);
	let i_only = cache_file(11, i.uuid, 100);
	let b_child = cache_file(20, b.uuid, 100);
	state.upsert_dirs([&a, &i, &b].into_iter()).unwrap();
	state
		.upsert_files([&a_only, &i_only, &b_child].into_iter())
		.unwrap();

	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	sync_roots.insert(b.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;

	state.handle_remove_sync_root(a.uuid, true);

	assert!(
		!state.sync_roots.contains_key(&a.uuid),
		"A removed from the map"
	);
	assert!(state.sync_roots.contains_key(&b.uuid), "B still active");

	// A's own content is evicted; the protected nested root B, its child, and the intermediate path
	// dir I all survive. A's own node is kept.
	assert!(!item_exists(&state, a_only.uuid), "A-only file evicted");
	assert!(!item_exists(&state, i_only.uuid), "I-only file evicted");
	assert!(
		item_exists(&state, i.uuid),
		"intermediate dir on the path to B is protected"
	);
	assert!(item_exists(&state, b.uuid), "nested root B survives");
	assert!(item_exists(&state, b_child.uuid), "B's child survives");
	assert!(
		item_exists(&state, a.uuid),
		"the evicted root's own node is kept"
	);
}

/// A sibling eviction wipes the evicted root's subtree and leaves an unrelated sibling root alone.
#[test]
fn remove_sync_root_eviction_leaves_siblings_alone() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	let b = cache_dir(2, account_root);
	let fa = cache_file(10, a.uuid, 100);
	let fb = cache_file(20, b.uuid, 100);
	state.upsert_dirs([&a, &b].into_iter()).unwrap();
	state.upsert_files([&fa, &fb].into_iter()).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	sync_roots.insert(b.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;

	state.handle_remove_sync_root(a.uuid, true);

	assert!(!item_exists(&state, fa.uuid), "A's file evicted");
	assert!(
		item_exists(&state, b.uuid) && item_exists(&state, fb.uuid),
		"sibling B untouched"
	);
}

/// `RemoveSyncRoot` without eviction just stops syncing — the cached items remain (stale, no longer
/// updated by the membership gate).
#[test]
fn remove_sync_root_without_evict_keeps_items() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	let fa = cache_file(10, a.uuid, 100);
	state.upsert_dirs(once(&a)).unwrap();
	state.upsert_files(once(&fa)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;

	state.handle_remove_sync_root(a.uuid, false);

	assert!(!state.sync_roots.contains_key(&a.uuid));
	assert!(item_exists(&state, fa.uuid), "no eviction → items remain");
}

/// a `DirEvent::Removed` of a sync-root node drops that root from the active set and emits a
/// `SyncRootsDeleted` notification (the app must re-add to resume).
#[test]
fn dir_removed_of_sync_root_drops_it_and_notifies() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel(8);
	state.msg_sender = msg_tx;

	// One subdir sync root B (under the account root) with a child file.
	let b = cache_dir(1, account_root);
	let child = cache_file(2, b.uuid, 100);
	state.upsert_dirs(once(&b)).unwrap();
	state.upsert_files(once(&child)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(b.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;

	// B is deleted server-side.
	state
		.apply_event(CacheEventType::Dir(DirEvent::Removed(b.uuid)))
		.unwrap();

	assert!(!item_exists(&state, b.uuid), "B's node is deleted");
	assert!(
		!item_exists(&state, child.uuid),
		"B's child is cascade-deleted"
	);
	assert!(
		!state.sync_roots.contains_key(&b.uuid),
		"B is dropped from the active set"
	);

	let msg = msg_rx
		.try_recv()
		.expect("a SyncRootsDeleted status message");
	assert!(
		matches!(&msg[..], [CacheMessage::SyncRootsDeleted(roots)] if *roots == [b.uuid]),
		"the notification names the deleted root, got {msg:?}"
	);
}

/// deleting an ancestor that is itself a sync root cascade-wipes a NESTED sync root; both are
/// dropped from the active set and reported in one notification.
#[test]
fn cascade_delete_drops_nested_sync_root() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel(8);
	state.msg_sender = msg_tx;

	// account_root → A(root) → I → B(nested root).
	let a = cache_dir(1, account_root);
	let i = cache_dir(2, a.uuid);
	let b = cache_dir(3, i.uuid);
	state.upsert_dirs([&a, &i, &b].into_iter()).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	sync_roots.insert(b.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;

	// A is deleted server-side; the cascade wipes I and the nested root B.
	state
		.apply_event(CacheEventType::Dir(DirEvent::Removed(a.uuid)))
		.unwrap();

	assert!(!item_exists(&state, a.uuid));
	assert!(
		!item_exists(&state, b.uuid),
		"nested root B cascade-deleted"
	);
	assert!(!state.sync_roots.contains_key(&a.uuid), "A dropped");
	assert!(
		!state.sync_roots.contains_key(&b.uuid),
		"nested root B dropped"
	);

	let msg = msg_rx
		.try_recv()
		.expect("a SyncRootsDeleted status message");
	let [CacheMessage::SyncRootsDeleted(roots)] = &msg[..] else {
		panic!("expected a single SyncRootsDeleted, got {msg:?}");
	};
	assert!(
		roots.contains(&a.uuid) && roots.contains(&b.uuid),
		"both roots reported, got {roots:?}"
	);
}

/// A dir (not itself a root) that MOVES out of every sync root is deleted, and the cascade wipes any
/// NESTED sync root under it — which must then be dropped from the active set + reported, exactly as
/// a `Removed` would (the move-out delete path must not leave the nested root a zombie).
#[test]
fn dir_move_out_of_root_drops_nested_sync_root() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel(8);
	state.msg_sender = msg_tx;

	// account_root → A(root) → C(plain dir) → B(nested root) → child.
	let a = cache_dir(1, account_root);
	let c = cache_dir(2, a.uuid);
	let b = cache_dir(3, c.uuid);
	let child = cache_file(4, b.uuid, 100);
	state.upsert_dirs([&a, &c, &b].into_iter()).unwrap();
	state.upsert_files(once(&child)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	sync_roots.insert(b.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;

	// Move C out from under A to directly under the account root (not a sync root) → C's subtree,
	// including the nested root B, is cascade-deleted.
	let moved_c = cache_dir(2, account_root);
	state
		.apply_event(CacheEventType::Dir(DirEvent::Move(moved_c)))
		.unwrap();

	assert!(
		!item_exists(&state, b.uuid),
		"nested root B cascade-deleted"
	);
	assert!(
		!state.sync_roots.contains_key(&b.uuid),
		"nested root B dropped from the active set"
	);
	assert!(state.sync_roots.contains_key(&a.uuid), "A still active");

	let msg = msg_rx
		.try_recv()
		.expect("a SyncRootsDeleted notification for the cascade-wiped nested root");
	assert!(
		matches!(&msg[..], [CacheMessage::SyncRootsDeleted(roots)] if roots.contains(&b.uuid)),
		"B reported deleted, got {msg:?}"
	);
}

/// `finalize_resync` drops a sync root the server reported GONE (a `get_dir` not-found during the
/// locked listing): its cached subtree is deleted, it is removed from the active set, the app is
/// notified, and — since a not-found is definite progress, not a transient failure — the watermark
/// still advances and the resync flag clears.
#[test]
fn finalize_resync_drops_a_server_deleted_root() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel(8);
	state.msg_sender = msg_tx;

	// Selective config: subdir root B (+ a child) under the account root.
	let b = cache_dir(1, account_root);
	let child = cache_file(2, b.uuid, 100);
	state.upsert_dirs(once(&b)).unwrap();
	state.upsert_files(once(&child)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(b.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;
	state.mark_needs_resync().unwrap();

	// The locked listing reported B not-found (deleted server-side); nothing else to list or skip.
	state
		.finalize_resync(Vec::new(), vec![b.uuid], false, 100)
		.unwrap();

	assert!(!item_exists(&state, b.uuid), "B's node is deleted");
	assert!(
		!item_exists(&state, child.uuid),
		"B's subtree is cascade-deleted"
	);
	assert!(!state.sync_roots.contains_key(&b.uuid), "B is dropped");
	let msg = msg_rx.try_recv().expect("a SyncRootsDeleted message");
	assert!(
		matches!(&msg[..], [CacheMessage::SyncRootsDeleted(roots)] if *roots == [b.uuid]),
		"the notification names B, got {msg:?}"
	);
	assert_eq!(
		state.watermark().unwrap(),
		Some(100),
		"a definitive deletion is progress: the watermark advances"
	);
	assert!(!state.needs_resync().unwrap(), "the resync flag clears");
}

/// `finalize_resync` must NOT advance the watermark or clear `needs_resync` when EVERY root failed to
/// list with a transient (non-not-found) error: committing an empty convergence would jump the
/// watermark past — and clear the flag for — a gap that was never reconciled (silent data loss). A
/// later cycle must retry instead.
#[test]
fn finalize_resync_all_transient_failure_preserves_watermark_and_flag() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let b = cache_dir(1, account_root);
	state.upsert_dirs(once(&b)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(b.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;

	// A real prior watermark + a pending hole that this resync is meant to heal.
	state.set_watermark(50).unwrap();
	state.mark_needs_resync().unwrap();

	// The locked block got the lock + snapshot id (remote_under_lock = 100) but every root then failed
	// transiently: nothing listed, nothing deleted, `any_transient` set.
	state
		.finalize_resync(Vec::new(), Vec::new(), true, 100)
		.unwrap();

	assert_eq!(
		state.watermark().unwrap(),
		Some(50),
		"the watermark must NOT jump to the snapshot id when nothing was reconciled"
	);
	assert!(
		state.needs_resync().unwrap(),
		"needs_resync must stay set so a later cycle retries"
	);
}

/// In selective-sync mode a `DeleteAll` wipes the subdir-roots' ancestry rows, so the membership gate
/// would then drop every subsequent live event. The handler must mark a resync so the cache re-lists
/// and re-converges, rather than silently going dark for the rest of the session.
#[test]
fn delete_all_marks_needs_resync() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	// Selective config: a subdir root A (the account root is NOT itself a sync root).
	let a = cache_dir(1, account_root);
	state.upsert_dirs(once(&a)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;

	assert!(!state.needs_resync().unwrap(), "starts clear");
	state
		.apply_event(CacheEventType::Global(GlobalEvent::DeleteAll))
		.unwrap();

	assert!(
		state.needs_resync().unwrap(),
		"DeleteAll wiped the root ancestry, so a resync must be scheduled to re-converge"
	);
}

/// A `Removed` of an ordinary (non-root) dir under a sync root leaves the active set unchanged and
/// emits no `SyncRootsDeleted`.
#[test]
fn dir_removed_of_non_root_does_not_touch_the_set() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel(8);
	state.msg_sender = msg_tx;

	let a = cache_dir(1, account_root); // sync root
	let sub = cache_dir(2, a.uuid); // ordinary dir under A
	state.upsert_dirs([&a, &sub].into_iter()).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	state.sync_roots = sync_roots;

	state
		.apply_event(CacheEventType::Dir(DirEvent::Removed(sub.uuid)))
		.unwrap();

	assert!(state.sync_roots.contains_key(&a.uuid), "A still active");
	assert!(
		msg_rx.try_recv().is_err(),
		"no SyncRootsDeleted for an ordinary dir removal"
	);
}

/// Dispatch: after the drain applies a batch, a sync root's callback receives the events
/// that touched ITS subtree — and not events outside it.
#[test]
fn dispatch_notifies_only_the_owning_sync_root() {
	use std::sync::{Arc, Mutex};

	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	state.upsert_dirs(once(&a)).unwrap();

	let received: Arc<Mutex<Vec<Uuid>>> = Arc::new(Mutex::new(Vec::new()));
	let received_cb = received.clone();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(
		a.uuid,
		Box::new(move |events: &mut dyn Iterator<Item = &CacheEvent<'_>>| {
			let mut got = received_cb.lock().unwrap();
			for event in events {
				let uuid = match &event.event {
					CacheEventType::File(FileEvent::New(f)) => f.uuid,
					CacheEventType::Dir(DirEvent::New(d)) => d.uuid,
					_ => continue,
				};
				got.push(uuid);
			}
		}),
	);
	state.sync_roots = sync_roots;

	// A New under A (in-root) and a New under the account root (out-of-root, not a sync root).
	let in_root = cache_file(2, a.uuid, 100);
	let out_of_root = cache_dir(3, account_root);
	state
		.insert_event(&CacheEvent {
			id: Some(1),
			event: CacheEventType::File(FileEvent::New(in_root.clone())),
		})
		.unwrap();
	state
		.insert_event(&CacheEvent {
			id: Some(2),
			event: CacheEventType::Dir(DirEvent::New(out_of_root.clone())),
		})
		.unwrap();

	state.drain_persisted().unwrap();

	let got = received.lock().unwrap();
	assert!(
		got.contains(&in_root.uuid),
		"A's callback received the in-root file"
	);
	assert!(
		!got.contains(&out_of_root.uuid),
		"A's callback did NOT receive the out-of-root dir"
	);
}

/// a move from sync root A to sync root B notifies BOTH — A (the item left, resolved from the
/// pre-move parent snapshot) and B (the item arrived, resolved post-apply).
#[test]
fn dispatch_move_between_sync_roots_notifies_both() {
	use std::sync::{Arc, Mutex};

	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	let b = cache_dir(2, account_root);
	let file = cache_file(3, a.uuid, 100); // initially under A
	state.upsert_dirs([&a, &b].into_iter()).unwrap();
	state.upsert_files(once(&file)).unwrap();

	let a_got = Arc::new(Mutex::new(false));
	let b_got = Arc::new(Mutex::new(false));
	let (a_cb, b_cb) = (a_got.clone(), b_got.clone());
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(
		a.uuid,
		Box::new(move |events: &mut dyn Iterator<Item = &CacheEvent<'_>>| {
			if events.next().is_some() {
				*a_cb.lock().unwrap() = true;
			}
		}),
	);
	sync_roots.insert(
		b.uuid,
		Box::new(move |events: &mut dyn Iterator<Item = &CacheEvent<'_>>| {
			if events.next().is_some() {
				*b_cb.lock().unwrap() = true;
			}
		}),
	);
	state.sync_roots = sync_roots;

	let moved = cache_file(3, b.uuid, 100); // same uuid, now under B
	state
		.insert_event(&CacheEvent {
			id: Some(1),
			event: CacheEventType::File(FileEvent::Move(moved)),
		})
		.unwrap();
	state.drain_persisted().unwrap();

	assert!(
		*a_got.lock().unwrap(),
		"A (old parent) notified of the move-out"
	);
	assert!(
		*b_got.lock().unwrap(),
		"B (new parent) notified of the move-in"
	);
}

/// Dispatch isolates a panicking callback: a panic in one root's callback is caught (surfaced as a
/// status error) and does NOT suppress another root's callback or stall the drain.
#[test]
fn dispatch_isolates_a_panicking_callback() {
	use std::sync::{Arc, Mutex};

	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel(8);
	state.msg_sender = msg_tx;

	let a = cache_dir(1, account_root);
	let b = cache_dir(2, account_root);
	state.upsert_dirs([&a, &b].into_iter()).unwrap();

	let b_called = Arc::new(Mutex::new(false));
	let b_cb = b_called.clone();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(
		a.uuid,
		Box::new(|_: &mut dyn Iterator<Item = &CacheEvent<'_>>| {
			panic!("boom from root A's callback");
		}),
	);
	sync_roots.insert(
		b.uuid,
		Box::new(move |events: &mut dyn Iterator<Item = &CacheEvent<'_>>| {
			if events.next().is_some() {
				*b_cb.lock().unwrap() = true;
			}
		}),
	);
	state.sync_roots = sync_roots;

	// One New under A (fires A's panicking callback) and one under B.
	state
		.insert_event(&CacheEvent {
			id: Some(1),
			event: CacheEventType::File(FileEvent::New(cache_file(10, a.uuid, 100))),
		})
		.unwrap();
	state
		.insert_event(&CacheEvent {
			id: Some(2),
			event: CacheEventType::File(FileEvent::New(cache_file(11, b.uuid, 100))),
		})
		.unwrap();

	// The drain must complete despite A's callback panicking.
	state.drain_persisted().unwrap();

	assert!(
		*b_called.lock().unwrap(),
		"B's callback still fired despite A's panic"
	);
	let msg = msg_rx
		.try_recv()
		.expect("a status message for the caught panic");
	assert!(
		msg.iter().any(|m| matches!(
			m,
			CacheMessage::Error(errs)
				if errs.iter().any(|e| matches!(e, CacheError::SyncRootCallbackPanic(_)))
		)),
		"the panic surfaced as SyncRootCallbackPanic, got {msg:?}"
	);
}

/// A `DeleteAll` driven through the drain wipes every non-root item, schedules a resync, AND
/// notifies EVERY configured sync root's callback (it is account-global).
#[test]
fn delete_all_through_drain_wipes_resyncs_and_notifies_all_roots() {
	use std::sync::{Arc, Mutex};

	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	let b = cache_dir(2, account_root);
	let fa = cache_file(10, a.uuid, 100);
	state.upsert_dirs([&a, &b].into_iter()).unwrap();
	state.upsert_files(once(&fa)).unwrap();

	let a_got = Arc::new(Mutex::new(false));
	let b_got = Arc::new(Mutex::new(false));
	let (a_cb, b_cb) = (a_got.clone(), b_got.clone());
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(
		a.uuid,
		Box::new(move |events: &mut dyn Iterator<Item = &CacheEvent<'_>>| {
			if events.next().is_some() {
				*a_cb.lock().unwrap() = true;
			}
		}),
	);
	sync_roots.insert(
		b.uuid,
		Box::new(move |events: &mut dyn Iterator<Item = &CacheEvent<'_>>| {
			if events.next().is_some() {
				*b_cb.lock().unwrap() = true;
			}
		}),
	);
	state.sync_roots = sync_roots;

	state
		.insert_event(&CacheEvent {
			id: Some(1),
			event: CacheEventType::Global(GlobalEvent::DeleteAll),
		})
		.unwrap();
	state.drain_persisted().unwrap();

	assert_eq!(item_count(&state), 1, "only the account-root item remains");
	assert!(
		state.needs_resync().unwrap(),
		"DeleteAll schedules a resync"
	);
	assert!(
		*a_got.lock().unwrap(),
		"root A notified of the account-global DeleteAll"
	);
	assert!(
		*b_got.lock().unwrap(),
		"root B notified of the account-global DeleteAll"
	);
}

/// App close/resume at the storage layer: opening the SAME DB file again preserves items, the
/// watermark, and the `needs_resync` flag (`init_db` only wipes on a version mismatch).
#[test]
fn state_persists_across_reopen_on_same_db_file() {
	let path = std::env::temp_dir().join(format!("filen-cache-reopen-{}.db", Uuid::new_v4()));
	let root = Uuid::new_v4();
	{
		let mut state = CacheState::new_on_path(&path, root);
		state.upsert_dirs(once(&cache_dir(1, root))).unwrap();
		state.set_watermark(4242).unwrap();
		state.mark_needs_resync().unwrap();
	} // drop → SQLite connection closed → WAL checkpointed/flushed

	{
		let state = CacheState::new_on_path(&path, root);
		assert_eq!(
			state.watermark().unwrap(),
			Some(4242),
			"watermark survives reopen"
		);
		assert!(
			item_exists(&state, Uuid::from_u128(1)),
			"items survive reopen"
		);
		assert!(
			state.needs_resync().unwrap(),
			"needs_resync flag survives reopen"
		);
	}

	let _ = std::fs::remove_file(&path);
	let _ = std::fs::remove_file(path.with_extension("db-wal"));
	let _ = std::fs::remove_file(path.with_extension("db-shm"));
}

/// On a REOPENED DB connection, deleting a dir must still recursively cascade-delete its whole
/// subtree (grandchildren included). This needs `recursive_triggers` + `foreign_keys` set on EVERY
/// connection — they are per-connection pragmas that revert to OFF on a fresh open, so applying them
/// only inside the schema-creating `INIT` (skipped on a version-matching reopen) leaves the cascade
/// non-recursive after a restart.
#[test]
fn cascade_delete_recurses_on_a_reopened_db() {
	let path =
		std::env::temp_dir().join(format!("filen-cache-reopen-cascade-{}.db", Uuid::new_v4()));
	let root = Uuid::new_v4();
	{
		// First open creates the schema (version mismatch → INIT runs).
		let mut state = CacheState::new_on_path(&path, root);
		let a = cache_dir(1, root);
		let b = cache_dir(2, a.uuid);
		let file = cache_file(3, b.uuid, 100);
		state.upsert_dirs([&a, &b].into_iter()).unwrap();
		state.upsert_files(once(&file)).unwrap();
	} // connection closed

	// Reopen the SAME db: version matches now, so init_db early-returns BEFORE re-running INIT.
	let mut state = CacheState::new_on_path(&path, root);
	let (a, b, file) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
	assert!(
		item_exists(&state, b) && item_exists(&state, file),
		"subtree present after reopen"
	);

	// Delete A: the cascade trigger must recurse through B down to the grandchild file.
	state.delete_items(once(a)).unwrap();

	assert!(!item_exists(&state, a), "A deleted");
	assert!(!item_exists(&state, b), "B (child) cascade-deleted");
	assert!(
		!item_exists(&state, file),
		"grandchild file recursively cascade-deleted (recursive_triggers must be set on reopen)"
	);

	let _ = std::fs::remove_file(&path);
	let _ = std::fs::remove_file(path.with_extension("db-wal"));
	let _ = std::fs::remove_file(path.with_extension("db-shm"));
}

/// With `W=None`, the contiguous frontier must seed off the first applied id — NOT 1 — so a
/// real account (which starts at a high counter) advances its watermark instead of freezing.
#[test]
fn drain_advances_watermark_from_high_first_id() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;
	state
		.insert_event(&dir_new_event(Some(5000), Uuid::from_u128(1), root))
		.unwrap();
	state
		.insert_event(&dir_new_event(Some(5001), Uuid::from_u128(2), root))
		.unwrap();

	let resync = state.drain_persisted().unwrap();

	assert!(!resync, "a contiguous run needs no resync");
	assert_eq!(state.watermark().unwrap(), Some(5001));
	assert_eq!(item_count(&state), 3, "root + 2 applied dirs");
	assert!(state.load_event_batch(10).unwrap().0.is_empty());
}

/// D2/C3: a hole (id 2 missing) is still applied to `items`, but the watermark holds at the
/// contiguous frontier (1) and a resync is requested. The durable `needs_resync` flag is set
/// by the drain ITSELF (atomically, in `commit_drain_batch`) — no separate best-effort write.
#[test]
fn drain_holds_watermark_at_hole_and_requests_resync() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;
	state
		.insert_event(&dir_new_event(Some(1), Uuid::from_u128(1), root))
		.unwrap();
	state
		.insert_event(&dir_new_event(Some(3), Uuid::from_u128(3), root))
		.unwrap();

	assert!(!state.needs_resync().unwrap(), "flag starts clear");
	let resync = state.drain_persisted().unwrap();

	assert!(resync, "a hole requests a resync");
	assert!(
		state.needs_resync().unwrap(),
		"the drain durably recorded needs_resync atomically with the batch commit"
	);
	assert_eq!(
		state.watermark().unwrap(),
		Some(1),
		"watermark holds at the gap-free frontier, NOT past the hole"
	);
	assert_eq!(item_count(&state), 3, "both events still applied to items");
	assert!(state.load_event_batch(10).unwrap().0.is_empty());
}

/// Events at or below the watermark are deduped (simulates a crash after `set_watermark` but
/// before the rows were deleted): they are NOT re-applied, but ARE consumed.
#[test]
fn drain_dedups_events_at_or_below_watermark() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;
	state.set_watermark(2).unwrap();
	state
		.insert_event(&dir_new_event(Some(1), Uuid::from_u128(1), root))
		.unwrap();
	state
		.insert_event(&dir_new_event(Some(2), Uuid::from_u128(2), root))
		.unwrap();

	let resync = state.drain_persisted().unwrap();

	assert!(!resync);
	assert_eq!(
		item_count(&state),
		1,
		"deduped events do not re-apply (root only)"
	);
	assert_eq!(state.watermark().unwrap(), Some(2), "watermark unchanged");
	assert!(state.load_event_batch(10).unwrap().0.is_empty());
}

/// Re-draining the same events (a crash where rows were applied but not deleted) is idempotent.
#[test]
fn drain_is_idempotent_across_a_replay() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;
	let one = dir_new_event(Some(1), Uuid::from_u128(1), root);
	let three = dir_new_event(Some(3), Uuid::from_u128(3), root);
	state.insert_event(&one).unwrap();
	state.insert_event(&three).unwrap();
	state.drain_persisted().unwrap();

	// Re-insert the very same events (crash replay) and drain again.
	state.insert_event(&one).unwrap();
	state.insert_event(&three).unwrap();
	state.drain_persisted().unwrap();

	assert_eq!(item_count(&state), 3, "replay produced no duplicate rows");
	assert_eq!(state.watermark().unwrap(), Some(1));
	assert!(state.load_event_batch(10).unwrap().0.is_empty());
}

/// C1: a corrupt row is quarantined (deleted) during the drain, the good event still applies, and
/// a resync is requested to recover the lost one.
#[test]
fn drain_quarantines_corrupt_row_and_requests_resync() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;
	state
		.insert_event(&dir_new_event(Some(1), Uuid::from_u128(1), root))
		.unwrap();
	state
		.db
		.execute(
			"INSERT INTO events (drive_message_id, synthetic, payload) VALUES (2, FALSE, ?1)",
			[b"garbage".as_slice()],
		)
		.unwrap();

	let resync = state.drain_persisted().unwrap();

	assert!(resync, "a corrupt row forces a resync");
	assert_eq!(
		item_count(&state),
		2,
		"the good event applied (root + 1 dir)"
	);
	assert!(
		state.load_event_batch(10).unwrap().0.is_empty(),
		"the corrupt row was quarantined (deleted)"
	);
}

/// a lost LOW id (here a corrupt row at id=1) must NOT let a later good id (id=2)
/// free-seed the frontier — the watermark must stay below the lost id, with a resync requested.
/// Before the `frontier_broken` fix, the `None` frontier accepted id=2 as "contiguous" and the
/// watermark jumped to 2, silently claiming coverage of the lost id=1.
#[test]
fn drain_does_not_advance_watermark_past_a_lost_low_id() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;
	// Corrupt (lost) row at id=1, good event at id=2 — watermark starts None.
	state
		.db
		.execute(
			"INSERT INTO events (drive_message_id, synthetic, payload) VALUES (1, FALSE, ?1)",
			[b"garbage".as_slice()],
		)
		.unwrap();
	state
		.insert_event(&dir_new_event(Some(2), Uuid::from_u128(2), root))
		.unwrap();

	let resync = state.drain_persisted().unwrap();

	assert!(resync, "a lost low id forces a resync");
	assert_eq!(
		state.watermark().unwrap(),
		None,
		"watermark must NOT jump past the lost id=1"
	);
	assert_eq!(item_count(&state), 2, "the good event (id=2) still applied");
	assert!(state.load_event_batch(10).unwrap().0.is_empty());
}

/// The contiguous-prefix frontier carries ACROSS `BATCH_SIZE` load boundaries: a contiguous run
/// longer than one batch advances the watermark all the way to the last id (the frontier is not
/// reset or re-seeded between batches).
#[test]
fn drain_advances_watermark_across_batch_boundaries() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;
	let total = (BATCH_SIZE + 50) as u64; // spans two load batches
	for id in 1..=total {
		state
			.insert_event(&dir_new_event(Some(id), Uuid::from_u128(id as u128), root))
			.unwrap();
	}

	let resync = state.drain_persisted().unwrap();

	assert!(!resync, "a fully contiguous run needs no resync");
	assert_eq!(
		state.watermark().unwrap(),
		Some(total),
		"watermark advances through BOTH batches to the last id"
	);
	assert_eq!(
		item_count(&state),
		1 + total as i64,
		"root + every applied dir"
	);
	assert!(
		state.load_event_batch(10).unwrap().0.is_empty(),
		"store fully drained"
	);
}

/// A hole detected in an EARLIER batch keeps the watermark held even as LATER batches apply: the
/// `frontier_broken` barrier carries across the batch boundary, so a high id in a later batch can
/// never free the watermark past the lost low id.
#[test]
fn drain_holds_watermark_at_a_hole_across_batch_boundaries() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;
	// id 1 applies; id 2 is the hole; ids 3..=total are contiguous from 3 and span a second batch.
	let total = (BATCH_SIZE + 50) as u64;
	state
		.insert_event(&dir_new_event(Some(1), Uuid::from_u128(1), root))
		.unwrap();
	for id in 3..=total {
		state
			.insert_event(&dir_new_event(Some(id), Uuid::from_u128(id as u128), root))
			.unwrap();
	}

	let resync = state.drain_persisted().unwrap();

	assert!(resync, "the hole forces a resync");
	assert!(
		state.needs_resync().unwrap(),
		"needs_resync recorded durably"
	);
	assert_eq!(
		state.watermark().unwrap(),
		Some(1),
		"watermark held at the contiguous frontier (1), not freed by a later batch's high id"
	);
	assert!(
		item_count(&state) as usize > BATCH_SIZE + 1,
		"every event still applied across both batches (the hole holds the watermark, not the apply)"
	);
}

/// A `NoOp` frontier marker advances the watermark (it carries a real id) but mutates nothing.
#[test]
fn drain_noop_event_advances_watermark_without_mutating() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;
	state
		.insert_event(&CacheEvent {
			id: Some(1),
			event: CacheEventType::NoOp,
		})
		.unwrap();
	state
		.insert_event(&dir_new_event(Some(2), Uuid::from_u128(2), root))
		.unwrap();

	let resync = state.drain_persisted().unwrap();

	assert!(!resync, "noop(1) + dir(2) is a contiguous run");
	assert_eq!(
		item_count(&state),
		2,
		"only the dir mutated items (root + dir)"
	);
	assert_eq!(
		state.watermark().unwrap(),
		Some(2),
		"the NoOp marker advances the frontier through id=1"
	);
}

/// The callback's router must NEVER drop an event while under the cap: every routed event lands in
/// the single unbounded channel and nothing is shed.
#[test]
fn route_thread_event_buffers_without_dropping_under_cap() {
	let (events_tx, events_rx) = crossbeam::channel::unbounded();
	let shed = AtomicBool::new(false);

	let total = 100;
	for id in 0..total as u64 {
		route_thread_event(
			CacheThreadEvent::Socket(CacheEventMaybeDecrypted::FrontierAdvance { id }),
			&events_tx,
			&shed,
			EVENT_SHED_CAP,
		);
	}

	assert_eq!(
		events_rx.len(),
		total,
		"every routed event is buffered (zero loss)"
	);
	assert!(
		!shed.load(Ordering::Acquire),
		"well under the cap, so nothing is shed"
	);
}

/// once the channel reaches the cap, the router SHEDS further events (bounded memory) and
/// latches `shed` instead of growing without bound.
#[test]
fn route_thread_event_sheds_at_cap() {
	let (events_tx, events_rx) = crossbeam::channel::unbounded();
	let shed = AtomicBool::new(false);
	let cap = 4;

	// Fill exactly to the cap, then route a handful more.
	for id in 0..(cap as u64 + 5) {
		route_thread_event(
			CacheThreadEvent::Socket(CacheEventMaybeDecrypted::FrontierAdvance { id }),
			&events_tx,
			&shed,
			cap,
		);
	}

	assert_eq!(
		events_rx.len(),
		cap,
		"the channel is capped at the shed cap; excess events are shed, not buffered"
	);
	assert!(
		shed.load(Ordering::Acquire),
		"the shed latch is set so the worker can record a recovery resync"
	);
}

/// when the producer shed events, the worker's drain observes the latch, durably records a
/// resync (so the dropped events are recovered), and clears the latch.
#[test]
fn drain_pending_records_resync_when_events_were_shed() {
	let (mut state, producer) = CacheState::new_in_memory_with_producer();
	producer.shed.store(true, Ordering::Release); // simulate a shed under load

	assert!(!state.needs_resync().unwrap(), "flag starts clear");
	state.drain_pending(None);

	assert!(
		state.needs_resync().unwrap(),
		"a shed forces a durable resync to recover the dropped events"
	);
	assert!(
		!producer.shed.load(Ordering::Acquire),
		"the shed latch is consumed (cleared) by the drain"
	);
}

/// The headline end-to-end guarantee: flooding a large burst through the single unbounded channel
/// (zero loss), the worker persists everything to `events`, the ordered drain applies it all, and
/// the watermark reaches the last id.
#[test]
fn flood_persists_applies_with_zero_loss() {
	let (mut state, producer) = CacheState::new_in_memory_with_producer();
	let root = state.root_uuid;

	let total = 200u64;
	for id in 1..=total {
		let event = CacheThreadEvent::Socket(CacheEventMaybeDecrypted::Decrypted(dir_new_event(
			Some(id),
			Uuid::from_u128(id as u128),
			root,
		)));
		route_thread_event(event, &producer.events, &producer.shed, EVENT_SHED_CAP);
	}
	assert!(
		!producer.shed.load(Ordering::Acquire),
		"the flood stays under the shed cap, so nothing is shed"
	);

	state.drain_pending(None);

	assert_eq!(
		item_count(&state),
		1 + total as i64,
		"every flooded event was applied (root + all dirs, zero loss)"
	);
	assert_eq!(
		state.watermark().unwrap(),
		Some(total),
		"watermark advanced to the last contiguous id"
	);
	assert!(
		state.load_event_batch(10).unwrap().0.is_empty(),
		"the events store is fully drained"
	);
}

/// a hole holds the watermark AND durably records that a resync is needed, so the recovery
/// signal is not silently lost (a resync consumes it).
#[test]
fn drain_pending_records_needs_resync_on_a_hole() {
	let (mut state, producer) = CacheState::new_in_memory_with_producer();
	let root = state.root_uuid;
	assert!(!state.needs_resync().unwrap(), "starts clean");

	for id in [1u64, 3] {
		// id 2 is missing → a hole
		producer
			.events
			.send(CacheThreadEvent::Socket(
				CacheEventMaybeDecrypted::Decrypted(dir_new_event(
					Some(id),
					Uuid::from_u128(id as u128),
					root,
				)),
			))
			.unwrap();
	}

	state.drain_pending(None);

	assert!(
		state.needs_resync().unwrap(),
		"the hole durably records needs_resync"
	);
	assert_eq!(
		state.watermark().unwrap(),
		Some(1),
		"watermark held at the gap-free frontier"
	);
}

/// a Manual (`list_dir`) event flows through the drain — deferred to apply AFTER the
/// ordered socket events — without breaking the socket drain.
#[test]
fn drain_pending_applies_socket_then_deferred_manual() {
	let (mut state, producer) = CacheState::new_in_memory_with_producer();
	let root = state.root_uuid;

	producer
		.events
		.send(CacheThreadEvent::Socket(
			CacheEventMaybeDecrypted::Decrypted(dir_new_event(Some(1), Uuid::from_u128(1), root)),
		))
		.unwrap();
	producer
		.events
		.send(CacheThreadEvent::Manual(ManualEvent::ListDirRecursive(
			vec![],
			vec![],
		)))
		.unwrap();

	state.drain_pending(None);

	assert_eq!(
		item_count(&state),
		2,
		"the socket dir applied; the empty manual list added nothing"
	);
	assert_eq!(state.watermark().unwrap(), Some(1));
}
