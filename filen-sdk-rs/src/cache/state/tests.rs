use super::*;

/// Unit constructors have no resync deps, so these futures never touch the network — a minimal
/// current-thread runtime suffices.
fn drive<F: Future>(fut: F) -> F::Output {
	tokio::runtime::Builder::new_current_thread()
		.build()
		.unwrap()
		.block_on(fut)
}

fn item_count(state: &CacheState) -> i64 {
	state
		.db
		.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
		.unwrap()
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

/// A malformed drive event (unknown kind or undecryptable payload upstream) carries only its id;
/// the cache must map it to a `FrontierAdvance` marker so the watermark passes it without a gap.
#[test]
fn drive_malformed_event_maps_to_frontier_advance() {
	use crate::socket::DecryptedSocketEvent;

	let socket_event = DecryptedSocketEvent::DriveMalformed {
		drive_message_id: 99,
	};

	match CacheEventMaybeDecrypted::from_decrypted_event(&socket_event) {
		Some(CacheEventMaybeDecrypted::FrontierAdvance { id: 99 }) => {}
		other => panic!("expected a FrontierAdvance {{ id: 99 }}, got {other:?}"),
	}
}

/// A socket reconnect must NOT be dropped: it maps to a resync signal so the disconnect-window
/// gap is closed. The initial-connect meta-events (AuthSuccess/AuthFailed) are covered by the
/// startup path and stay unmapped, so they do not force a spurious resync on every connect.
#[test]
fn reconnecting_maps_to_resync_signal_others_dropped() {
	use crate::socket::DecryptedSocketEvent;

	assert!(matches!(
		CacheEventMaybeDecrypted::from_decrypted_event(&DecryptedSocketEvent::Reconnecting),
		Some(CacheEventMaybeDecrypted::ResyncSignal)
	));
	assert!(
		CacheEventMaybeDecrypted::from_decrypted_event(&DecryptedSocketEvent::AuthSuccess)
			.is_none()
	);
	assert!(
		CacheEventMaybeDecrypted::from_decrypted_event(&DecryptedSocketEvent::AuthFailed).is_none()
	);
}

/// Draining a reconnect signal REQUESTS the cheap gap-check (drain returns `true`) but does NOT
/// touch the durable `needs_resync` flag — a reconnect is a suspicion, not an observed hole, so it
/// must not force a resync. (Pins the redesign: the old code marked `needs_resync` here.)
#[test]
fn reconnect_signal_requests_gap_check_without_durable_flag() {
	let mut state = CacheState::new_in_memory();
	assert!(!state.needs_resync().unwrap(), "starts clear");

	let reconnected = state.drain_pending(Some(CacheThreadEvent::Socket(
		CacheEventMaybeDecrypted::ResyncSignal,
	)));

	assert!(
		reconnected,
		"draining a reconnect must request a gap-check (drain returns true)"
	);
	assert!(
		!state.needs_resync().unwrap(),
		"a reconnect is a suspicion, not an observed hole — it must NOT set the durable flag"
	);
}

/// A reconnect storm (several `ResyncSignal`s in one drain) collapses into a SINGLE gap-check
/// request — one check per drain, not one per event — and still never flags the durable resync.
#[test]
fn reconnect_burst_collapses_to_one_gap_check() {
	let (mut state, producer) = CacheState::new_in_memory_with_producer();
	for _ in 0..3 {
		producer
			.events
			.try_send(CacheThreadEvent::Socket(
				CacheEventMaybeDecrypted::ResyncSignal,
			))
			.unwrap();
	}

	// One drain consumes the whole burst and reports a single gap-check request.
	let reconnected = state.drain_pending(None);

	assert!(
		reconnected,
		"a reconnect burst requests exactly one gap-check"
	);
	assert!(
		!state.needs_resync().unwrap(),
		"a reconnect burst must not set the durable resync flag"
	);
}

/// A drain with NO reconnect signal does not request a gap-check.
#[test]
fn drain_without_reconnect_requests_no_gap_check() {
	let mut state = CacheState::new_in_memory();
	let reconnected = state.drain_pending(Some(CacheThreadEvent::Socket(
		CacheEventMaybeDecrypted::FrontierAdvance { id: 7 },
	)));
	assert!(
		!reconnected,
		"a non-reconnect drain must not request a gap-check"
	);
}

/// a `FileMove` whose new parent is a non-navigable virtual container (here `Links`)
/// takes the file out of the synced tree, so it must convert to `FileEvent::Removed` rather than
/// failing conversion (which would make it a frontier-advance-only event and leave a stale row).
#[test]
fn file_move_to_virtual_parent_becomes_removed() {
	use std::borrow::Cow;

	use crate::{
		crypto::file::FileKey,
		fs::file::meta::{DecryptedFileMeta, FileMeta},
		io::RemoteFile,
		socket::{DecryptedDriveEvent, DecryptedSocketEvent, FileMove},
	};
	use chrono::Utc;
	use filen_types::{auth::FileEncryptionVersion, fs::ParentUuid};

	let expected = Uuid::new_v4();
	let file = RemoteFile {
		uuid: expected,
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

	use crate::{
		fs::dir::meta::{DecryptedDirectoryMeta, DirectoryMeta},
		io::RemoteDirectory,
		socket::{DecryptedDriveEvent, DecryptedSocketEvent, FolderMove},
	};
	use chrono::Utc;
	use filen_types::{api::v3::dir::color::DirColor, fs::ParentUuid};

	let expected = Uuid::new_v4();
	let dir = RemoteDirectory::from_meta(
		expected,
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

fn dir_new_event(id: Option<u64>, uuid: Uuid, parent: Uuid) -> CacheEvent<'static> {
	use std::borrow::Cow;

	use crate::fs::dir::cache::CacheableDir;
	use chrono::Utc;
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

	use crate::crypto::file::FileKey;
	use chrono::Utc;
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
		.apply_resync(vec![(root, dirs.clone(), files.clone())], 100, false)
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
		.apply_resync(vec![(root, vec![m_moved], vec![f.clone()])], 100, false)
		.unwrap();

	assert!(!item_exists(&state, d_old.uuid), "deleted parent removed");
	assert!(item_exists(&state, m.uuid), "moved dir survives");
	assert!(
		item_exists(&state, f.uuid),
		"a descendant of the moved dir must survive the deleted parent's cascade"
	);
	assert_eq!(item_parent(&state, f.uuid), Some(m.uuid), "F stays under M");
}

/// The gap-check gate shared by the startup check AND the reconnect check: resync iff a hole is
/// flagged OR the remote drive id advanced past the watermark. Crucially, an UNCHANGED drive id
/// (remote == watermark) must NOT resync — this is what makes a reconnect on a quiet account (or a
/// reconnect storm) cheap: the gap-check reads the remote id and skips the re-list.
#[test]
fn should_resync_for_remote_gates_on_drive_id_advance() {
	let state = CacheState::new_in_memory();

	// Fresh cache (watermark None): a non-empty drive resyncs to populate; an empty drive does not.
	assert!(
		state.should_resync_for_remote(5000).unwrap(),
		"fresh cache + non-empty drive → resync"
	);
	assert!(
		!state.should_resync_for_remote(0).unwrap(),
		"fresh cache + empty drive → nothing to populate"
	);

	state.set_watermark(100).unwrap();
	// The reconnect case the owner cares about: reconnect on an unchanged drive → NO resync.
	assert!(
		!state.should_resync_for_remote(100).unwrap(),
		"remote == watermark → NO resync (nothing changed during the disconnect window)"
	);
	// The reconnect case that DOES need a resync: the drive advanced while we were disconnected.
	assert!(
		state.should_resync_for_remote(101).unwrap(),
		"remote > watermark → resync (changes landed while offline)"
	);
	assert!(
		!state.should_resync_for_remote(99).unwrap(),
		"remote < watermark (anomalous) → no resync"
	);

	// A durably-flagged hole (observed elsewhere) forces a resync even when the drive id did not
	// advance — the gate folds `needs_resync` in for both the startup and reconnect paths.
	state.mark_needs_resync().unwrap();
	assert!(
		state.should_resync_for_remote(100).unwrap(),
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
	state.set_test_sync_roots(sync_roots);

	// In-root file (parent = A) → cached.
	let in_root = cache_file(2, a.uuid, 100);
	assert!(
		state
			.apply_event(
				CacheEventType::File(FileEvent::New(in_root.clone())),
				EventTrust::Checked
			)
			.is_ok()
	);
	assert!(item_exists(&state, in_root.uuid), "in-root file is cached");

	// Out-of-root file (parent = the account root, which is NOT a configured sync root) → skipped,
	// but still Ok (so the watermark advances).
	let out_of_root = cache_file(3, account_root, 100);
	assert!(
		state
			.apply_event(
				CacheEventType::File(FileEvent::New(out_of_root.clone())),
				EventTrust::Checked
			)
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
	state.set_test_sync_roots(sync_roots);
	assert!(item_exists(&state, file.uuid), "file starts cached under A");

	// Move the file OUT of A — to the account root, which is NOT a configured sync root.
	let moved = cache_file(2, account_root, 100);
	state
		.apply_event(
			CacheEventType::File(FileEvent::Move(moved)),
			EventTrust::Checked,
		)
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
	state.set_test_sync_roots(sync_roots);

	// Move sync root B out from under A to directly under the account root (NOT a configured root).
	let moved_b = cache_dir(2, account_root);
	state
		.apply_event(
			CacheEventType::Dir(DirEvent::Move(moved_b)),
			EventTrust::Checked,
		)
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
	state.set_test_sync_roots(sync_roots);

	let before = item_content_hash(&state, b.uuid);
	// Same uuid + parent, renamed → the fingerprint changes.
	let mut b_changed = cache_dir(1, account_root);
	b_changed.name = std::borrow::Cow::Owned("renamed".to_string());
	state
		.apply_event(
			CacheEventType::Dir(DirEvent::Changed(b_changed.clone())),
			EventTrust::Checked,
		)
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
	state.set_test_sync_roots(sync_roots);

	// Listings: A → fA1 + new fA3 (fA2 gone); B → fB1 + new fB2.
	let fa3 = cache_file(12, a.uuid, 100);
	let fb2 = cache_file(21, b.uuid, 100);
	let per_root = vec![
		(a.uuid, vec![], vec![fa1.clone(), fa3.clone()]),
		(b.uuid, vec![], vec![fb1.clone(), fb2.clone()]),
	];
	state.apply_resync(per_root, 500, false).unwrap();

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
	state.apply_resync(vec![], 777, false).unwrap();

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
	state.set_test_sync_roots(sync_roots);

	drive(state.handle_remove_registration(a.uuid, 0, true, None));

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
	state.set_test_sync_roots(sync_roots);

	drive(state.handle_remove_registration(a.uuid, 0, true, None));

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
	state.set_test_sync_roots(sync_roots);

	drive(state.handle_remove_registration(a.uuid, 0, false, None));

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
	state.set_test_sync_roots(sync_roots);

	// B is deleted server-side.
	state
		.apply_event(
			CacheEventType::Dir(DirEvent::Removed(b.uuid)),
			EventTrust::Checked,
		)
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
	state.set_test_sync_roots(sync_roots);

	// A is deleted server-side; the cascade wipes I and the nested root B.
	state
		.apply_event(
			CacheEventType::Dir(DirEvent::Removed(a.uuid)),
			EventTrust::Checked,
		)
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
	state.set_test_sync_roots(sync_roots);

	// Move C out from under A to directly under the account root (not a sync root) → C's subtree,
	// including the nested root B, is cascade-deleted.
	let moved_c = cache_dir(2, account_root);
	state
		.apply_event(
			CacheEventType::Dir(DirEvent::Move(moved_c)),
			EventTrust::Checked,
		)
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
	state.set_test_sync_roots(sync_roots);
	state.mark_needs_resync().unwrap();

	// The locked listing reported B not-found (deleted server-side); nothing else to list or skip.
	state
		.finalize_resync(ResyncListing {
			per_root_raw: Vec::new(),
			deleted_roots: vec![b.uuid],
			any_transient: false,
			remote_under_lock: 100,
		})
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
	state.set_test_sync_roots(sync_roots);

	// A real prior watermark + a pending hole that this resync is meant to heal.
	state.set_watermark(50).unwrap();
	state.mark_needs_resync().unwrap();

	// The locked block got the lock + snapshot id (remote_under_lock = 100) but every root then failed
	// transiently: nothing listed, nothing deleted, `any_transient` set.
	state
		.finalize_resync(ResyncListing {
			per_root_raw: Vec::new(),
			deleted_roots: Vec::new(),
			any_transient: true,
			remote_under_lock: 100,
		})
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
	state.set_test_sync_roots(sync_roots);

	assert!(!state.needs_resync().unwrap(), "starts clear");
	state
		.apply_event(
			CacheEventType::Global(GlobalEvent::DeleteAll),
			EventTrust::Checked,
		)
		.unwrap();

	assert!(
		state.needs_resync().unwrap(),
		"DeleteAll wiped the root ancestry, so a resync must be scheduled to re-converge"
	);
}

/// `apply_resync` dispatches its synthetics to the owning registrations: creates ride the
/// per-anchor fast path (owners resolved once per root), while delete-shaped synthetics keep
/// the exact per-event resolution — both must reach the anchor's callback.
#[test]
fn apply_resync_dispatches_synthetics_through_both_owner_paths() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;
	let received: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
	let received_cb = received.clone();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(
		root,
		Box::new(move |events: &mut dyn Iterator<Item = &CacheEvent<'_>>| {
			received_cb
				.lock()
				.unwrap()
				.extend(events.map(|event| format!("{:?}", event.event)));
		}),
	);
	state.set_test_sync_roots(sync_roots);

	// A pre-cached file the listing will NOT contain (→ a Removed synthetic, exact path) and
	// a listed new dir (→ a New synthetic, fast per-anchor path).
	let stale = cache_file(7, root, 10);
	state.upsert_files(once(&stale)).unwrap();
	let new_dir = cache_dir(2, root);
	state
		.apply_resync(vec![(root, vec![new_dir.clone()], vec![])], 50, false)
		.unwrap();

	let got = received.lock().unwrap();
	assert!(
		got.iter()
			.any(|event| event.contains("New") && event.contains(&new_dir.uuid.to_string())),
		"the created dir dispatches via the per-anchor owners: {got:?}"
	);
	assert!(
		got.iter()
			.any(|event| event.contains("Removed") && event.contains(&stale.uuid.to_string())),
		"the removed file dispatches via the exact per-event owners: {got:?}"
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
	state.set_test_sync_roots(sync_roots);

	state
		.apply_event(
			CacheEventType::Dir(DirEvent::Removed(sub.uuid)),
			EventTrust::Checked,
		)
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
	state.set_test_sync_roots(sync_roots);

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
	state.set_test_sync_roots(sync_roots);

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
	state.set_test_sync_roots(sync_roots);

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
	state.set_test_sync_roots(sync_roots);

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

/// A hole (id 2 missing) is still applied to `items`, but the watermark holds at the
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

/// A corrupt row is quarantined (deleted) during the drain, the good event still applies, and
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

/// A lost low id (here a corrupt row at id=1) must NOT let a later good id (id=2) free-seed the
/// frontier — the watermark must stay below the lost id, with a resync requested. A `None`
/// frontier must not accept a later id as "contiguous" and silently claim coverage of the lost id.
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
	let (events_tx, events_rx) = tokio::sync::mpsc::channel(EVENT_SHED_CAP);
	let shed = AtomicBool::new(false);

	let total = 100;
	for id in 0..total as u64 {
		route_thread_event(
			CacheThreadEvent::Socket(CacheEventMaybeDecrypted::FrontierAdvance { id }),
			&events_tx,
			&shed,
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

/// once the channel is full (its capacity IS the shed cap), the router SHEDS further events
/// (bounded memory) and latches `shed` instead of growing without bound.
#[test]
fn route_thread_event_sheds_at_cap() {
	let cap = 4;
	let (events_tx, events_rx) = tokio::sync::mpsc::channel(cap);
	let shed = AtomicBool::new(false);

	// Fill exactly to the cap, then route a handful more.
	for id in 0..(cap as u64 + 5) {
		route_thread_event(
			CacheThreadEvent::Socket(CacheEventMaybeDecrypted::FrontierAdvance { id }),
			&events_tx,
			&shed,
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

/// the per-connection PRAGMAs apply on every open — including a REOPEN, where the version check
/// skips INIT — since none of them persist in the DB file.
#[test]
fn init_db_applies_per_connection_pragmas_on_reopen() {
	let dir = std::env::temp_dir().join(format!("filen-cache-pragma-{}", Uuid::new_v4()));
	std::fs::create_dir_all(&dir).unwrap();
	let path = dir.join("cache.db");
	let root = Uuid::new_v4();
	drop(CacheState::new_on_path(&path, root)); // first open: full INIT
	let state = CacheState::new_on_path(&path, root); // reopen: INIT skipped

	let synchronous: i64 = state
		.db
		.query_row("PRAGMA synchronous", [], |row| row.get(0))
		.unwrap();
	assert_eq!(synchronous, 1, "synchronous = NORMAL on every open");
	let cache_size: i64 = state
		.db
		.query_row("PRAGMA cache_size", [], |row| row.get(0))
		.unwrap();
	assert_eq!(cache_size, -32768, "page-cache budget on every open");
	let mmap_size: i64 = state
		.db
		.query_row("PRAGMA mmap_size", [], |row| row.get(0))
		.unwrap();
	assert_eq!(mmap_size, 268_435_456, "mmap budget on every open");

	drop(state);
	let _ = std::fs::remove_dir_all(&dir);
}

/// a failing apply aborts the batched fast path WITHOUT surfacing errors, and the per-event
/// fallback re-runs the same rows with the pre-batching semantics: the poison event is
/// quarantined (consumed, no torn item/file row pair), the good event still applies, a resync
/// is durably recorded, and no transaction leaks open.
#[test]
fn drain_falls_back_to_per_event_when_a_bulk_apply_fails() {
	let (mut state, producer) = CacheState::new_in_memory_with_producer();
	let root = state.root_uuid;

	let good_dir = cache_dir(2, root);
	let poison_file = cache_file(3, root, 100);
	producer
		.events
		.try_send(CacheThreadEvent::Socket(
			CacheEventMaybeDecrypted::Decrypted(CacheEvent {
				id: Some(1),
				event: CacheEventType::Dir(DirEvent::New(good_dir.clone())),
			}),
		))
		.unwrap();
	producer
		.events
		.try_send(CacheThreadEvent::Socket(
			CacheEventMaybeDecrypted::Decrypted(CacheEvent {
				id: Some(2),
				event: CacheEventType::File(FileEvent::New(poison_file.clone())),
			}),
		))
		.unwrap();
	// Make every file apply fail mid-event (after its items row is written): the bulk pass must roll the
	// whole batch back and the fallback must quarantine ONLY the file event. (A trigger, not a
	// DROP TABLE: items' own triggers reference `files`, so dropping it would poison dir
	// applies too.)
	state
		.db
		.execute_batch(
			"CREATE TRIGGER poison_file_insert BEFORE INSERT ON files BEGIN \
			 SELECT RAISE(ABORT, 'poisoned'); END;",
		)
		.unwrap();

	state.drain_pending(None);

	assert!(
		state.db.is_autocommit(),
		"no transaction may leak open after the fallback"
	);
	assert!(
		state.needs_resync().unwrap(),
		"a quarantined event durably records a resync"
	);
	assert!(
		state.load_event_batch(10).unwrap().0.is_empty(),
		"both events are consumed (the poison one by quarantine)"
	);
	let dir_applied: i64 = state
		.db
		.query_row(
			"SELECT count(*) FROM items WHERE uuid = ?1",
			[good_dir.uuid],
			|row| row.get(0),
		)
		.unwrap();
	assert_eq!(
		dir_applied, 1,
		"the good event still applies via the fallback"
	);
	let torn_file_row: i64 = state
		.db
		.query_row(
			"SELECT count(*) FROM items WHERE uuid = ?1",
			[poison_file.uuid],
			|row| row.get(0),
		)
		.unwrap();
	assert_eq!(
		torn_file_row, 0,
		"the poison event's partial apply is rolled back (no orphan items row)"
	);
	assert_eq!(
		state.watermark().unwrap(),
		Some(1),
		"the watermark holds at the last good id, never crossing the quarantined one"
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
		route_thread_event(event, &producer.events, &producer.shed);
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
			.try_send(CacheThreadEvent::Socket(
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
		.try_send(CacheThreadEvent::Socket(
			CacheEventMaybeDecrypted::Decrypted(dir_new_event(Some(1), Uuid::from_u128(1), root)),
		))
		.unwrap();
	producer
		.events
		.try_send(CacheThreadEvent::Manual(ManualEvent::ListDirRecursive(
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

/// registering an additional callback on an ALREADY-ACTIVE root appends it (both
/// registrations live), acks `Ok`, and requests NO convergence resync — the subtree is already
/// converged and the uuid's existence is established.
#[test]
fn add_registration_to_active_root_appends_without_resync() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	state.upsert_dirs(once(&a)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	state.set_test_sync_roots(sync_roots);

	let (ack, mut ack_rx) = tokio::sync::oneshot::channel();
	let needs_resync = drive(state.handle_add_sync_root(a.uuid, 1, Box::new(|_| {}), ack));

	assert!(!needs_resync, "an already-active root needs no resync");
	assert!(matches!(ack_rx.try_recv(), Ok(Ok(()))), "acked Ok");
	assert_eq!(
		state.sync_roots.get(&a.uuid).unwrap().len(),
		2,
		"both registrations live"
	);
}

/// registering a NEW root acks `Ok` once it is in the active set and reports that a
/// convergence resync is needed. (Unit construction has no resync deps, so `get_dir` validation
/// is skipped.)
#[test]
fn add_registration_for_new_root_acks_and_requests_resync() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let b = cache_dir(2, account_root);
	state.upsert_dirs(once(&b)).unwrap();
	state.set_test_sync_roots(HashMap::new());

	let (ack, mut ack_rx) = tokio::sync::oneshot::channel();
	let needs_resync = drive(state.handle_add_sync_root(b.uuid, 7, Box::new(|_| {}), ack));

	assert!(needs_resync, "a newly-active root must be converged");
	assert!(matches!(ack_rx.try_recv(), Ok(Ok(()))), "acked Ok");
	assert!(state.sync_roots.contains_key(&b.uuid));
}

/// removing ONE of two registrations keeps the root active and SKIPS the requested
/// eviction (deleting the subtree out from under the surviving registration would fight the
/// membership gate); the ack reports `Ok(false)`.
#[test]
fn remove_registration_with_survivors_skips_eviction() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	let fa = cache_file(10, a.uuid, 100);
	state.upsert_dirs(once(&a)).unwrap();
	state.upsert_files(once(&fa)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	state.set_test_sync_roots(sync_roots);
	let (add_ack, _add_ack_rx) = tokio::sync::oneshot::channel();
	drive(state.handle_add_sync_root(a.uuid, 1, Box::new(|_| {}), add_ack));

	let (ack, mut ack_rx) = tokio::sync::oneshot::channel();
	drive(state.handle_remove_registration(a.uuid, 0, true, Some(ack)));

	assert!(
		matches!(ack_rx.try_recv(), Ok(Ok(false))),
		"eviction skipped while a registration survives"
	);
	assert!(state.sync_roots.contains_key(&a.uuid), "root still active");
	assert!(item_exists(&state, fa.uuid), "subtree intact");
}

/// removing the LAST registration with `evict` deletes the root's subtree and acks
/// `Ok(true)`.
#[test]
fn remove_last_registration_evicts_and_acks_true() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	let fa = cache_file(10, a.uuid, 100);
	state.upsert_dirs(once(&a)).unwrap();
	state.upsert_files(once(&fa)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	state.set_test_sync_roots(sync_roots);

	let (ack, mut ack_rx) = tokio::sync::oneshot::channel();
	drive(state.handle_remove_registration(a.uuid, 0, true, Some(ack)));

	assert!(matches!(ack_rx.try_recv(), Ok(Ok(true))), "subtree evicted");
	assert!(!state.sync_roots.contains_key(&a.uuid), "root inactive");
	assert!(!item_exists(&state, fa.uuid), "A's file evicted");
	assert!(item_exists(&state, a.uuid), "the root's own node is kept");
}

/// a registration removal for a root that was already dropped server-side (a stale
/// handle's drop) is a harmless no-op acked `Ok(false)`.
#[test]
fn remove_registration_after_server_side_deletion_is_noop() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let b = cache_dir(1, account_root);
	state.upsert_dirs(once(&b)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(b.uuid, Box::new(|_| {}));
	state.set_test_sync_roots(sync_roots);

	// B is deleted server-side → dropped from the active set (all registrations at once).
	state
		.apply_event(
			CacheEventType::Dir(DirEvent::Removed(b.uuid)),
			EventTrust::Checked,
		)
		.unwrap();
	assert!(!state.sync_roots.contains_key(&b.uuid));

	let (ack, mut ack_rx) = tokio::sync::oneshot::channel();
	drive(state.handle_remove_registration(b.uuid, 0, true, Some(ack)));

	assert!(
		matches!(ack_rx.try_recv(), Ok(Ok(false))),
		"stale removal no-ops"
	);
}

/// dispatch fans a batch out to EVERY registration on the owning root, not just one.
#[test]
fn dispatch_fires_every_registration_on_a_root() {
	use std::sync::{Arc, Mutex};

	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	state.upsert_dirs(once(&a)).unwrap();

	let first_got = Arc::new(Mutex::new(false));
	let second_got = Arc::new(Mutex::new(false));
	let (first_cb, second_cb) = (first_got.clone(), second_got.clone());
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(
		a.uuid,
		Box::new(move |events: &mut dyn Iterator<Item = &CacheEvent<'_>>| {
			if events.next().is_some() {
				*first_cb.lock().unwrap() = true;
			}
		}),
	);
	state.set_test_sync_roots(sync_roots);
	let (add_ack, _add_ack_rx) = tokio::sync::oneshot::channel();
	drive(state.handle_add_sync_root(
		a.uuid,
		1,
		Box::new(move |events: &mut dyn Iterator<Item = &CacheEvent<'_>>| {
			if events.next().is_some() {
				*second_cb.lock().unwrap() = true;
			}
		}),
		add_ack,
	));

	let in_root = cache_file(2, a.uuid, 100);
	state
		.insert_event(&CacheEvent {
			id: Some(1),
			event: CacheEventType::File(FileEvent::New(in_root)),
		})
		.unwrap();
	state.drain_persisted().unwrap();

	assert!(*first_got.lock().unwrap(), "first registration notified");
	assert!(*second_got.lock().unwrap(), "second registration notified");
}

/// a control burst registers everything queued behind the first message before the (single)
/// convergence resync, and a queued `Shutdown` wins immediately.
#[test]
fn control_burst_processes_queued_messages_and_shutdown_wins() {
	let (mut state, producer) = CacheState::new_in_memory_with_producer();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	let b = cache_dir(2, account_root);
	state.upsert_dirs([&a, &b].into_iter()).unwrap();
	state.set_test_sync_roots(HashMap::new());

	// Queue a second add behind the first; the burst must pick it up via try_recv.
	let (b_ack, mut b_ack_rx) = tokio::sync::oneshot::channel();
	producer
		.control
		.send(CacheControlMessage::AddSyncRoot {
			uuid: b.uuid,
			registration_id: 1,
			callback: Box::new(|_| {}),
			ack: b_ack,
		})
		.unwrap();
	let (a_ack, mut a_ack_rx) = tokio::sync::oneshot::channel();
	let shutdown = drive(
		state.process_control_burst(CacheControlMessage::AddSyncRoot {
			uuid: a.uuid,
			registration_id: 0,
			callback: Box::new(|_| {}),
			ack: a_ack,
		}),
	);

	assert!(!shutdown);
	assert!(matches!(a_ack_rx.try_recv(), Ok(Ok(()))));
	assert!(matches!(b_ack_rx.try_recv(), Ok(Ok(()))));
	assert!(state.sync_roots.contains_key(&a.uuid) && state.sync_roots.contains_key(&b.uuid));
	assert!(
		state.needs_resync().unwrap(),
		"the new roots' convergence is durably scheduled (the unit-ctx resync is a no-op, so the \
		 flag survives for a later drain to retry)"
	);

	// A Shutdown anywhere in the burst stops processing immediately.
	assert!(drive(
		state.process_control_burst(CacheControlMessage::Shutdown)
	));
}

/// evicting the account root while a subdir root survives wipes everything flat, then
/// durably schedules the survivors' re-convergence — so a transiently failing inline resync (a
/// no-op in unit construction) is retried by a later drain instead of stranding them empty.
#[test]
fn account_root_evict_with_survivors_marks_needs_resync() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	let fa = cache_file(10, a.uuid, 100);
	state.upsert_dirs(once(&a)).unwrap();
	state.upsert_files(once(&fa)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(account_root, Box::new(|_| {}));
	sync_roots.insert(a.uuid, Box::new(|_| {}));
	state.set_test_sync_roots(sync_roots);

	let (ack, mut ack_rx) = tokio::sync::oneshot::channel();
	drive(state.handle_remove_registration(account_root, 0, true, Some(ack)));

	assert!(
		matches!(ack_rx.try_recv(), Ok(Ok(true))),
		"account root evicted"
	);
	assert!(
		!item_exists(&state, fa.uuid),
		"the flat wipe removed the survivor's file"
	);
	assert!(
		state.sync_roots.contains_key(&a.uuid),
		"survivor still registered"
	);
	assert!(
		state.needs_resync().unwrap(),
		"survivor re-convergence durably scheduled"
	);
}

/// adding a sync root that is COVERED by an active root (cached, ancestry reaches the
/// active key) takes the fast path: registered immediately, no convergence resync requested.
#[test]
fn covered_add_registers_without_resync() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	let nested = cache_dir(2, a.uuid);
	state.upsert_dirs([&a, &nested].into_iter()).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(account_root, Box::new(|_| {}));
	state.set_test_sync_roots(sync_roots);

	let (ack, mut ack_rx) = tokio::sync::oneshot::channel();
	let needs_resync = drive(state.handle_add_sync_root(nested.uuid, 1, Box::new(|_| {}), ack));

	assert!(!needs_resync, "covered add must not request a resync");
	assert!(matches!(ack_rx.try_recv(), Ok(Ok(()))), "acked Ok");
	assert!(state.sync_roots.contains_key(&nested.uuid));
	assert!(
		!state.needs_resync().unwrap(),
		"no durable resync scheduled for a covered add"
	);
}

/// an UNCOVERED uuid (cached rows but no active root above it) still takes the slow path
/// and requests a convergence resync.
#[test]
fn uncovered_add_still_requests_resync() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	state.upsert_dirs(once(&a)).unwrap();
	// No active roots at all: A's rows exist but nothing covers them.
	state.set_test_sync_roots(HashMap::new());

	let (ack, mut ack_rx) = tokio::sync::oneshot::channel();
	let needs_resync = drive(state.handle_add_sync_root(a.uuid, 1, Box::new(|_| {}), ack));

	assert!(needs_resync, "uncovered add must be converged");
	assert!(matches!(ack_rx.try_recv(), Ok(Ok(()))));
	assert!(state.sync_roots.contains_key(&a.uuid));
}

/// a covered add while a gap is already durably flagged keeps the flag set (the scheduled
/// resync lists all roots, including the newly covered one) — the fast path masks nothing.
#[test]
fn covered_add_keeps_pending_resync_flag() {
	let mut state = CacheState::new_in_memory();
	let account_root = state.root_uuid;
	let a = cache_dir(1, account_root);
	state.upsert_dirs(once(&a)).unwrap();
	let mut sync_roots: HashMap<Uuid, SyncRootCallback> = HashMap::new();
	sync_roots.insert(account_root, Box::new(|_| {}));
	state.set_test_sync_roots(sync_roots);
	state.mark_needs_resync().unwrap();

	let (ack, _ack_rx) = tokio::sync::oneshot::channel();
	let needs_resync = drive(state.handle_add_sync_root(a.uuid, 1, Box::new(|_| {}), ack));

	assert!(!needs_resync, "fast path requests no immediate resync");
	assert!(
		state.needs_resync().unwrap(),
		"the pre-existing durable flag stays set for the scheduled resync"
	);
}

/// `apply_synthetics_direct` multi-row-batches simple `New`/`Changed` upserts (the resync-populate
/// hot path) and must land every item with the correct type/parent, FK-link the type-specific row
/// to its `items` row, round-trip metadata through the multi-row bind, and keep the `items` rowid
/// STABLE across a re-upsert (else the `files`/`dirs` foreign keys would dangle).
#[test]
fn apply_synthetics_batches_creates_with_stable_fk() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;

	// dirs A,B under root; files under A and B — interleaved as the diff emits them, so both the
	// dir batch and the file batch are exercised in one chunk.
	let a = cache_dir(1, root);
	let b = cache_dir(2, root);
	let f1 = cache_file(11, a.uuid, 100);
	let f2 = cache_file(12, a.uuid, 200);
	let f3 = cache_file(13, b.uuid, 300);

	let synthetics = vec![
		CacheEvent {
			id: None,
			event: CacheEventType::Dir(DirEvent::New(a.clone())),
		},
		CacheEvent {
			id: None,
			event: CacheEventType::Dir(DirEvent::New(b.clone())),
		},
		CacheEvent {
			id: None,
			event: CacheEventType::File(FileEvent::New(f1.clone())),
		},
		CacheEvent {
			id: None,
			event: CacheEventType::File(FileEvent::New(f2.clone())),
		},
		CacheEvent {
			id: None,
			event: CacheEventType::File(FileEvent::New(f3.clone())),
		},
	];

	state
		.apply_synthetics_direct(root, synthetics, true)
		.unwrap();

	for dir in [&a, &b] {
		let linked: i64 = state
			.db
			.query_row(
				"SELECT COUNT(*) FROM dirs d JOIN items i ON i.id = d.id WHERE i.uuid = ?",
				[dir.uuid],
				|r| r.get(0),
			)
			.unwrap();
		assert_eq!(linked, 1, "dir {} FK-linked to its items row", dir.uuid);
	}
	for file in [&f1, &f2, &f3] {
		let linked: i64 = state
			.db
			.query_row(
				"SELECT COUNT(*) FROM files fi JOIN items i ON i.id = fi.id WHERE i.uuid = ?",
				[file.uuid],
				|r| r.get(0),
			)
			.unwrap();
		assert_eq!(linked, 1, "file {} FK-linked to its items row", file.uuid);
	}
	let f1_size: i64 = state
		.db
		.query_row(
			"SELECT size FROM files fi JOIN items i ON i.id = fi.id WHERE i.uuid = ?",
			[f1.uuid],
			|r| r.get(0),
		)
		.unwrap();
	assert_eq!(
		f1_size, 100,
		"size round-tripped through the multi-row bind"
	);

	// FK stability: a Changed must reuse the existing rowid (upsert, not insert-or-replace).
	let id_before: i64 = state
		.db
		.query_row("SELECT id FROM items WHERE uuid = ?", [f1.uuid], |r| {
			r.get(0)
		})
		.unwrap();
	let mut changed = f1.clone();
	changed.size = 999;
	state
		.apply_synthetics_direct(
			root,
			vec![CacheEvent {
				id: None,
				event: CacheEventType::File(FileEvent::Changed(changed)),
			}],
			true,
		)
		.unwrap();
	let id_after: i64 = state
		.db
		.query_row("SELECT id FROM items WHERE uuid = ?", [f1.uuid], |r| {
			r.get(0)
		})
		.unwrap();
	assert_eq!(
		id_before, id_after,
		"re-upsert keeps the items rowid stable"
	);
	let f1_size_after: i64 = state
		.db
		.query_row(
			"SELECT size FROM files fi JOIN items i ON i.id = fi.id WHERE i.uuid = ?",
			[f1.uuid],
			|r| r.get(0),
		)
		.unwrap();
	assert_eq!(
		f1_size_after, 999,
		"the Changed upsert updated the row in place"
	);
}

/// A delete in the SAME chunk as preceding batched creates must see those creates applied: the chunk
/// flushes the pending upsert batch BEFORE applying the delete (preserving the diff's
/// creates/moves → deletes order). A delete-then-flush bug would no-op the delete on an empty cache
/// and then resurrect the very rows the delete was meant to remove.
#[test]
fn apply_synthetics_flushes_pending_creates_before_a_delete() {
	let mut state = CacheState::new_in_memory();
	let root = state.root_uuid;
	let a = cache_dir(1, root);
	let f = cache_file(11, a.uuid, 100);

	let synthetics = vec![
		CacheEvent {
			id: None,
			event: CacheEventType::Dir(DirEvent::New(a.clone())),
		},
		CacheEvent {
			id: None,
			event: CacheEventType::File(FileEvent::New(f.clone())),
		},
		CacheEvent {
			id: None,
			event: CacheEventType::Dir(DirEvent::Removed(a.uuid)),
		},
	];
	state
		.apply_synthetics_direct(root, synthetics, true)
		.unwrap();

	assert!(
		!item_exists(&state, a.uuid),
		"the deleted dir is gone — its create was flushed before the delete"
	);
	assert!(
		!item_exists(&state, f.uuid),
		"its child cascaded away, proving it existed when the delete ran"
	);
}
