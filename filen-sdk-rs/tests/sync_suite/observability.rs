//! Progress, reporting & observability tests (`OBSERV-*`) for the two-way sync engine.
//!
//! These verify that the engine tells the truth about what it did: the per-pass [`SyncReport`]
//! counters match the real on-disk/on-remote outcome, and the per-action [`SyncEvent`] stream
//! delivered to `sync_once_observed` fires exactly once per applied action, in happen-before
//! order, bracketed by `PassStarted`/`PassCompleted`.
//!
//! Tests that need infrastructure the current harness/public API does not provide — deterministic
//! mid-transfer/crash interruption, a per-item transfer-failure injector, a dry-run/plan-only mode,
//! a mass-delete *confirmation* API, byte/size progress fields, report timestamps, mid-pass
//! listener (un)registration, or a pass identifier — are written as `#[ignore]` stubs documenting
//! the plan, rather than faked.
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::Arc;

use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::categories::{DirType, Normal};
use filen_sdk_rs::fs::{HasName, HasUUID};
use filen_sdk_rs::sync_engine::{PlanOutcome, SyncEngine, SyncEvent, SyncMode, SyncReport};
use uuid::Uuid;

use crate::harness::*;
use crate::helpers::*;

// ---------------------------------------------------------------------------
// Observer / report helpers
// ---------------------------------------------------------------------------

/// True for the per-action in-progress events (the ones that should tick the progress numerator).
fn is_action_event(e: &SyncEvent) -> bool {
	matches!(
		e,
		SyncEvent::Uploading { .. }
			| SyncEvent::Downloading { .. }
			| SyncEvent::CreatingRemoteDir { .. }
			| SyncEvent::CreatingLocalDir { .. }
			| SyncEvent::TrashingRemote { .. }
			| SyncEvent::DeletingLocal { .. }
			| SyncEvent::MovingRemote { .. }
			| SyncEvent::MovingLocal { .. }
	)
}

/// Count the per-ACTION (in-progress) events, excluding lifecycle/metadata events
/// (`PassStarted`/`Planned`/`Conflict`/`DeletionsHeld`/`Refused`/`PassCompleted`).
fn action_event_count(events: &[SyncEvent]) -> usize {
	events.iter().filter(|e| is_action_event(e)).count()
}

/// Sum of all the SUCCESSFULLY-APPLIED report buckets (every counter that represents one applied
/// action). Conflicts/held/errors are NOT applied actions and are excluded.
fn applied_total(r: &SyncReport) -> usize {
	r.uploaded
		+ r.downloaded
		+ r.local_dirs_created
		+ r.remote_dirs_created
		+ r.locally_deleted
		+ r.remotely_trashed
		+ r.moved_remote
		+ r.moved_local
}

/// The `Planned { actions }` value, if present.
fn planned_actions(events: &[SyncEvent]) -> Option<usize> {
	events.iter().find_map(|e| match e {
		SyncEvent::Planned { actions } => Some(*actions),
		_ => None,
	})
}

/// List the files directly under a single-client's remote root (ground truth, via the client).
async fn list_remote_files(sc: &SingleClient) -> Vec<filen_sdk_rs::fs::file::RemoteFile> {
	let client = sc.cache.client.clone();
	let dir = client.get_dir(sc.remote).await.unwrap();
	let (_d, files) = client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	files
}

// ===========================================================================
// OBSERV-01 — upload count equals number of new local files pushed
// ===========================================================================

#[shared_test_runtime]
async fn observ_01_upload_count_equals_new_files() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	for name in ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"] {
		write_file(&sc.local, name, content_for(name).as_slice());
	}
	let report = sc.sync().await;

	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(report.uploaded, 5, "{report:?}");
	assert_eq!(report.downloaded, 0, "{report:?}");
	assert_eq!(report.remote_dirs_created, 0, "{report:?}");
	assert_eq!(report.local_dirs_created, 0, "{report:?}");
	assert_eq!(report.remotely_trashed, 0, "{report:?}");
	assert_eq!(report.locally_deleted, 0, "{report:?}");
	assert_eq!(report.moved_remote, 0, "{report:?}");
	assert_eq!(report.moved_local, 0, "{report:?}");
	assert_eq!(report.conflicts.len(), 0, "{report:?}");
	assert_eq!(report.held_deletions, 0, "{report:?}");
	// The whole pass's applied work is exactly the 5 uploads.
	assert_eq!(applied_total(&report), 5, "{report:?}");

	sc.cleanup();
}

// ===========================================================================
// OBSERV-02 — download count equals number of new remote files pulled
// ===========================================================================

#[shared_test_runtime]
async fn observ_02_download_count_equals_new_remote_files() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let client = sc.cache.client.clone();
	for name in ["r1.txt", "r2.txt", "r3.txt", "r4.txt"] {
		let b = client.make_file_builder(name, sc.remote).unwrap();
		let f = client
			.upload_file(b, content_for(name).as_slice())
			.await
			.unwrap();
		assert!(
			poll_for_item(sc.cache.db_path(), f.uuid(), CACHE_CONVERGE_TIMEOUT).await,
			"cache never observed {name}"
		);
	}

	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(report.downloaded, 4, "{report:?}");
	assert_eq!(report.uploaded, 0, "{report:?}");
	assert_eq!(applied_total(&report), 4, "{report:?}");

	for name in ["r1.txt", "r2.txt", "r3.txt", "r4.txt"] {
		assert!(
			read_eq(&sc.local, name, content_for(name).as_slice()),
			"{name} not byte-identical locally"
		);
	}

	sc.cleanup();
}

// ===========================================================================
// OBSERV-03 — dir-created count is distinct from file uploads
// ===========================================================================

#[shared_test_runtime]
async fn observ_03_dir_created_distinct_from_uploads() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	// dirA/, dirA/dirB/, dirA/dirB/file.txt, dirC/ (empty).
	write_file(&sc.local, "dirA/dirB/file.txt", b"deep");
	std::fs::create_dir_all(sc.local.join("dirC")).unwrap();

	let mut events = Vec::new();
	let report = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();

	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(
		report.remote_dirs_created, 3,
		"dirA, dirA/dirB, dirC: {report:?}"
	);
	assert_eq!(report.uploaded, 1, "only file.txt: {report:?}");
	// No double-counting: applied total is exactly 3 dirs + 1 file.
	assert_eq!(applied_total(&report), 4, "{report:?}");
	// Exactly 3 dir-create events + 1 upload event in the stream.
	let dir_events = events
		.iter()
		.filter(|e| matches!(e, SyncEvent::CreatingRemoteDir { .. }))
		.count();
	let up_events = events
		.iter()
		.filter(|e| matches!(e, SyncEvent::Uploading { .. }))
		.count();
	assert_eq!(dir_events, 3, "events: {events:?}");
	assert_eq!(up_events, 1, "events: {events:?}");

	sc.cleanup();
}

// ===========================================================================
// OBSERV-04 — delete count mirrors mirrored deletions; excludes backup modes
// ===========================================================================

#[shared_test_runtime]
async fn observ_04_delete_count_mirrored_local_to_remote() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	for name in ["k.txt", "d1.txt", "d2.txt"] {
		write_file(&sc.local, name, content_for(name).as_slice());
	}
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 3, "{r1:?}");

	std::fs::remove_file(sc.local.join("d1.txt")).unwrap();
	std::fs::remove_file(sc.local.join("d2.txt")).unwrap();
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	// 2 of 3 deleted does not trip the mass-delete guard (floor 10).
	assert_eq!(r2.remotely_trashed, 2, "{r2:?}");
	assert_eq!(r2.held_deletions, 0, "{r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");

	sc.cleanup();
}

#[shared_test_runtime]
async fn observ_04_backup_mode_does_not_mirror_deletes() {
	let sc = single_client(SyncMode::LocalBackup).await;
	for name in ["k.txt", "d1.txt", "d2.txt"] {
		write_file(&sc.local, name, content_for(name).as_slice());
	}
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 3, "{r1:?}");

	std::fs::remove_file(sc.local.join("d1.txt")).unwrap();
	std::fs::remove_file(sc.local.join("d2.txt")).unwrap();

	let mut events = Vec::new();
	let r2 = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	// Backup mode never mirrors a source-side deletion.
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.held_deletions, 0, "{r2:?}");
	assert!(
		!events
			.iter()
			.any(|e| matches!(e, SyncEvent::TrashingRemote { .. })),
		"backup mode must emit no trash event: {events:?}"
	);

	// All three still on the remote.
	assert_eq!(
		list_remote_files(&sc).await.len(),
		3,
		"backup must retain all files"
	);

	sc.cleanup();
}

// ===========================================================================
// OBSERV-05 — rename counted as a single move, not delete+create
// ===========================================================================

#[shared_test_runtime]
async fn observ_05_rename_counted_as_single_move() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "old/name.txt", b"stable content C payload");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");
	assert_eq!(r1.remote_dirs_created, 1, "{r1:?}");

	move_file(&sc.local, "old/name.txt", "old/renamed.txt");

	let mut events = Vec::new();
	let r2 = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.moved_remote, 1, "{r2:?}");
	assert_eq!(r2.uploaded, 0, "rename must not re-upload: {r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");
	// Exactly one move event, and NO trash/upload event for the renamed path.
	let moves = events
		.iter()
		.filter(|e| matches!(e, SyncEvent::MovingRemote { .. }))
		.count();
	assert_eq!(moves, 1, "events: {events:?}");
	assert!(
		!events.iter().any(|e| matches!(
			e,
			SyncEvent::TrashingRemote { .. } | SyncEvent::Uploading { .. }
		)),
		"a move must not decompose into trash+upload: {events:?}"
	);

	sc.cleanup();
}

// ===========================================================================
// OBSERV-06 — conflict count reflects genuine divergence; no overwrite
// ===========================================================================

#[shared_test_runtime]
async fn observ_06_conflict_count_and_no_overwrite() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	// Establish a shared baseline: x.txt and y.txt identical on both sides.
	write_file(&tc.local_a, "x.txt", b"base-x");
	write_file(&tc.local_a, "y.txt", b"base-y");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"baseline",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& tc.local_b.join("x.txt").is_file()
				&& tc.local_b.join("y.txt").is_file()
		},
	)
	.await;
	assert!(
		conflicts.is_empty(),
		"baseline must be clean: {conflicts:?}"
	);

	// Diverge x.txt on BOTH sides (genuine conflict); change y.txt on A only.
	write_file(&tc.local_a, "x.txt", b"LOCAL-A-edit");
	write_file(&tc.local_b, "x.txt", b"REMOTE-B-edit");
	write_file(&tc.local_a, "y.txt", b"y-edited-by-A-only");

	// Run rounds; x.txt must surface as a conflict and y.txt must propagate.
	let mut conflicts2 = BTreeSet::new();
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts2,
		"diverge",
		|| read_eq(&tc.local_b, "y.txt", b"y-edited-by-A-only"),
	)
	.await;

	assert!(
		conflicts2.iter().any(|c| c.contains("x.txt")),
		"x.txt must be surfaced as a conflict: {conflicts2:?}"
	);
	assert!(
		!conflicts2.iter().any(|c| c.contains("y.txt")),
		"y.txt's clean change must NOT be flagged: {conflicts2:?}"
	);
	// Non-destructive: neither divergent version is gone. A keeps its edit; B keeps its.
	assert!(
		read_eq(&tc.local_a, "x.txt", b"LOCAL-A-edit"),
		"A's divergent x.txt must be untouched"
	);
	assert!(
		read_eq(&tc.local_b, "x.txt", b"REMOTE-B-edit"),
		"B's divergent x.txt must be untouched"
	);

	tc.cleanup();
}

// ===========================================================================
// OBSERV-07 — held count reflects mass-deletion safety hold; applies nothing
// ===========================================================================

#[shared_test_runtime]
async fn observ_07_held_count_mass_delete_hold() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	const TOTAL: usize = 20;
	for i in 0..TOTAL {
		write_file(
			&sc.local,
			&format!("f{i:02}.txt"),
			format!("c{i}").as_bytes(),
		);
	}
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, TOTAL, "{r1:?}");

	// Delete all 20 (>= max(floor 10, 50%)) → trips the guard.
	for i in 0..TOTAL {
		std::fs::remove_file(sc.local.join(format!("f{i:02}.txt"))).unwrap();
	}

	let mut events = Vec::new();
	let r2 = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert!(r2.errors.is_empty(), "a hold is not an error: {r2:?}");
	assert!(r2.held_deletions > 0, "guard should hold deletions: {r2:?}");
	assert_eq!(
		r2.remotely_trashed, 0,
		"nothing destroyed this pass: {r2:?}"
	);
	assert!(r2.guard_message.is_some(), "{r2:?}");
	// A DeletionsHeld event surfaced the hold with the same count.
	let held_event = events.iter().find_map(|e| match e {
		SyncEvent::DeletionsHeld { count, .. } => Some(*count),
		_ => None,
	});
	assert_eq!(held_event, Some(r2.held_deletions), "events: {events:?}");
	// No trash action event fired.
	assert!(
		!events
			.iter()
			.any(|e| matches!(e, SyncEvent::TrashingRemote { .. })),
		"held pass must trash nothing: {events:?}"
	);

	// Remote still has all 20.
	assert_eq!(
		list_remote_files(&sc).await.len(),
		TOTAL,
		"guard must keep all remote files"
	);

	sc.cleanup();
}

// ===========================================================================
// OBSERV-09 — counter sum invariant across a heterogeneous changeset
// ===========================================================================

// NOTE: the plan's OBSERV-09 includes "1 induced transfer failure" which requires a
// fault-injection harness that does not exist. This implements the rest of the invariant on a
// mixed but all-succeeding changeset: every applied action lands in exactly one bucket, the report
// sum equals the per-action event count and the Planned denominator, and nothing is double-counted.
#[shared_test_runtime]
async fn observ_09_counter_sum_invariant_mixed_changeset() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	// Baseline: a dir + two files (one to delete, one to rename), and a file to modify.
	write_file(&sc.local, "keep/del.txt", b"to-delete");
	write_file(&sc.local, "keep/ren.txt", b"to-rename-unique-bytes");
	write_file(&sc.local, "mod.txt", b"v1");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 3, "{r1:?}");
	assert_eq!(r1.remote_dirs_created, 1, "{r1:?}");

	// Stage a mixed changeset: 2 new files, 1 delete, 1 rename, 1 new empty dir, 1 modify.
	write_file(&sc.local, "new1.txt", b"new-one");
	write_file(&sc.local, "new2.txt", b"new-two");
	std::fs::remove_file(sc.local.join("keep/del.txt")).unwrap();
	move_file(&sc.local, "keep/ren.txt", "keep/renamed.txt");
	std::fs::create_dir_all(sc.local.join("emptydir")).unwrap();
	write_file(&sc.local, "mod.txt", b"v2-longer-content");

	let mut events = Vec::new();
	let r2 = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	// 2 new + 1 modify = 3 uploads; 1 rename = 1 move; 1 delete = 1 trash; 1 empty dir = 1 dir.
	assert_eq!(r2.uploaded, 3, "{r2:?}");
	assert_eq!(r2.moved_remote, 1, "{r2:?}");
	assert_eq!(r2.remotely_trashed, 1, "{r2:?}");
	assert_eq!(r2.remote_dirs_created, 1, "{r2:?}");
	// Sum invariant: applied buckets total == per-action event count == Planned denominator.
	let applied = applied_total(&r2);
	assert_eq!(applied, 6, "{r2:?}");
	assert_eq!(
		action_event_count(&events),
		applied,
		"one event per applied action: {events:?}"
	);
	assert_eq!(
		planned_actions(&events),
		Some(applied),
		"Planned denominator must equal applied count: {events:?}"
	);

	sc.cleanup();
}

// ===========================================================================
// OBSERV-10 — per-action event fires exactly once for every applied action
// ===========================================================================

#[shared_test_runtime]
async fn observ_10_event_per_applied_action_exactly_once() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	// A spread of action types: dir-creates + uploads.
	write_file(&sc.local, "d1/a.txt", b"a");
	write_file(&sc.local, "d1/b.txt", b"b");
	write_file(&sc.local, "d2/c.txt", b"c");
	write_file(&sc.local, "root.txt", b"r");

	let mut events = Vec::new();
	let report = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert!(report.errors.is_empty(), "{report:?}");

	// Event count == applied action count.
	assert_eq!(
		action_event_count(&events),
		applied_total(&report),
		"events: {events:?}"
	);

	// Each (kind, path) appears exactly once — no duplicates, no missing.
	let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
	for e in &events {
		let key = match e {
			SyncEvent::Uploading { rel_path } => Some(("up".to_string(), rel_path.clone())),
			SyncEvent::CreatingRemoteDir { rel_path } => {
				Some(("mkdir".to_string(), rel_path.clone()))
			}
			_ => None,
		};
		if let Some(k) = key {
			assert!(
				seen.insert(k.clone()),
				"duplicate event for {k:?}: {events:?}"
			);
		}
	}
	// We expect exactly the 4 uploads + 2 dir-creates we staged.
	assert_eq!(report.uploaded, 4, "{report:?}");
	assert_eq!(report.remote_dirs_created, 2, "{report:?}");
	assert_eq!(seen.len(), 6, "distinct applied (kind,path): {seen:?}");

	sc.cleanup();
}

// ===========================================================================
// OBSERV-11 — events respect happen-before causal ordering
// ===========================================================================

#[shared_test_runtime]
async fn observ_11_events_causal_ordering() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "dirA/file.txt", b"child");

	let mut events = Vec::new();
	let report = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert!(report.errors.is_empty(), "{report:?}");

	// The dir-create for dirA must precede the upload of dirA/file.txt.
	let dir_idx = events
		.iter()
		.position(|e| matches!(e, SyncEvent::CreatingRemoteDir { rel_path } if rel_path == "dirA"));
	let file_idx = events.iter().position(
		|e| matches!(e, SyncEvent::Uploading { rel_path } if rel_path == "dirA/file.txt"),
	);
	let dir_idx = dir_idx.unwrap_or_else(|| panic!("no dirA create event: {events:?}"));
	let file_idx = file_idx.unwrap_or_else(|| panic!("no dirA/file.txt upload event: {events:?}"));
	assert!(
		dir_idx < file_idx,
		"parent dir create must precede child upload: {events:?}"
	);

	// PassStarted is first; PassCompleted is last (after every per-action event).
	assert!(
		matches!(events.first(), Some(SyncEvent::PassStarted { .. })),
		"first event must be PassStarted: {events:?}"
	);
	assert!(
		matches!(events.last(), Some(SyncEvent::PassCompleted { .. })),
		"last event must be PassCompleted: {events:?}"
	);
	// No per-action event after PassCompleted.
	let completed_idx = events
		.iter()
		.position(|e| matches!(e, SyncEvent::PassCompleted { .. }))
		.unwrap();
	assert!(
		!events[completed_idx + 1..].iter().any(is_action_event),
		"no action event may follow PassCompleted: {events:?}"
	);

	sc.cleanup();
}

// ===========================================================================
// OBSERV-13 — planned denominator matches actual applied work; numerator monotone
// ===========================================================================

#[shared_test_runtime]
async fn observ_13_planned_denominator_matches_work() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	// 10 uploads under d/ + one extra empty dir = 12 actions (d/, emptyA/ + 10 files).
	for i in 0..10 {
		write_file(
			&sc.local,
			&format!("d/f{i:02}.txt"),
			format!("c{i}").as_bytes(),
		);
	}
	std::fs::create_dir_all(sc.local.join("emptyA")).unwrap();

	let mut events = Vec::new();
	let report = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert!(report.errors.is_empty(), "{report:?}");

	let denom = planned_actions(&events).expect("a Planned event must be emitted");
	let applied = applied_total(&report);
	assert_eq!(
		denom, applied,
		"denominator must equal applied work: {report:?}"
	);
	assert_eq!(denom, 12, "10 uploads + 2 dirs: {report:?}");

	// Numerator (running per-action event count after the Planned marker) is monotone and never
	// exceeds the denominator.
	let planned_pos = events
		.iter()
		.position(|e| matches!(e, SyncEvent::Planned { .. }))
		.unwrap();
	let mut numerator = 0usize;
	for e in &events[planned_pos + 1..] {
		if is_action_event(e) {
			numerator += 1;
			assert!(
				numerator <= denom,
				"numerator overshot denominator: {events:?}"
			);
		}
	}
	assert_eq!(
		numerator, denom,
		"numerator must reach the denominator: {events:?}"
	);

	sc.cleanup();
}

// ===========================================================================
// OBSERV-15 — a no-op pass reports all-zero counts and emits no per-action events
// ===========================================================================

#[shared_test_runtime]
async fn observ_15_noop_pass_zero_counts_and_events() {
	// LocalToRemote so the engine's remote view (the cache, which converges asynchronously) can
	// never be read as a remote-side deletion on the second pass.
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "settled.txt", b"settled bytes");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Wait for the cache (the engine's remote view) to observe the just-uploaded file, so the
	// second pass reconciles against converged truth (a genuine no-op, not a re-upload/lag artifact).
	let uploaded_uuid = list_remote_files(&sc).await[0].uuid();
	assert!(
		poll_for_item(sc.cache.db_path(), uploaded_uuid, CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed the uploaded settled.txt"
	);

	// Second pass: nothing changed anywhere.
	let mut events = Vec::new();
	let r2 = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	assert_eq!(r2.local_dirs_created, 0, "{r2:?}");
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");
	assert_eq!(r2.locally_deleted, 0, "{r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.moved_remote, 0, "{r2:?}");
	assert_eq!(r2.moved_local, 0, "{r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");
	assert_eq!(r2.held_deletions, 0, "{r2:?}");
	assert!(r2.errors.is_empty(), "{r2:?}");

	// Zero per-action events, but a PassStarted/PassCompleted bracket still fires.
	assert_eq!(
		action_event_count(&events),
		0,
		"no-op must emit no action events: {events:?}"
	);
	assert!(
		events
			.iter()
			.any(|e| matches!(e, SyncEvent::PassStarted { .. })),
		"PassStarted must still fire: {events:?}"
	);
	assert_eq!(
		events
			.iter()
			.filter(|e| matches!(e, SyncEvent::PassCompleted { .. }))
			.count(),
		1,
		"exactly one PassCompleted: {events:?}"
	);
	assert_eq!(
		planned_actions(&events),
		Some(0),
		"Planned(0) on a no-op: {events:?}"
	);

	sc.cleanup();
}

// ===========================================================================
// OBSERV-17 — per-pair reports are isolated when multiple pairs run together
// ===========================================================================

#[shared_test_runtime]
async fn observ_17_per_pair_reports_isolated() {
	// Two fully-disjoint pairs (each its own local + remote root + engine).
	let p1 = single_client(SyncMode::LocalToRemote).await;
	let p2 = single_client(SyncMode::LocalToRemote).await;

	// P1: 3 new uploads.
	for name in ["u1.txt", "u2.txt", "u3.txt"] {
		write_file(&p1.local, name, content_for(name).as_slice());
	}
	// P2: stage 2 files, sync, then delete them so the next pass is 2 deletes.
	for name in ["g1.txt", "g2.txt"] {
		write_file(&p2.local, name, content_for(name).as_slice());
	}
	let p2_seed = p2.sync().await;
	assert_eq!(p2_seed.uploaded, 2, "{p2_seed:?}");
	std::fs::remove_file(p2.local.join("g1.txt")).unwrap();
	std::fs::remove_file(p2.local.join("g2.txt")).unwrap();

	// Run both pairs concurrently.
	let (r1, r2) = tokio::join!(p1.sync(), p2.sync());

	assert!(r1.errors.is_empty(), "{r1:?}");
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r1.uploaded, 3, "P1 uploads: {r1:?}");
	assert_eq!(r1.remotely_trashed, 0, "P1 has no deletes: {r1:?}");
	assert_eq!(r2.remotely_trashed, 2, "P2 deletes: {r2:?}");
	assert_eq!(r2.uploaded, 0, "P2 has no uploads: {r2:?}");

	p1.cleanup();
	p2.cleanup();
}

// ===========================================================================
// OBSERV-18 — first sync against a populated destination is a no-wipe
// ===========================================================================

#[shared_test_runtime]
async fn observ_18_first_sync_populated_destination_no_wipe() {
	let sc = single_client(SyncMode::TwoWay).await;
	let client = sc.cache.client.clone();
	// Destination (remote) already has D1, D2.
	for name in ["D1.txt", "D2.txt"] {
		let b = client.make_file_builder(name, sc.remote).unwrap();
		let f = client
			.upload_file(b, content_for(name).as_slice())
			.await
			.unwrap();
		assert!(
			poll_for_item(sc.cache.db_path(), f.uuid(), CACHE_CONVERGE_TIMEOUT).await,
			"cache never saw {name}"
		);
	}
	// Source (local) has S1, S2.
	write_file(&sc.local, "S1.txt", content_for("S1.txt").as_slice());
	write_file(&sc.local, "S2.txt", content_for("S2.txt").as_slice());

	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "{report:?}");
	// First sync of a populated destination must NOT wipe the destination-only files.
	assert_eq!(
		report.remotely_trashed, 0,
		"no remote wipe on first sync: {report:?}"
	);
	assert_eq!(
		report.locally_deleted, 0,
		"no local wipe on first sync: {report:?}"
	);

	// D1, D2 survive on the remote; nothing destroyed.
	let names: BTreeSet<String> = list_remote_files(&sc)
		.await
		.iter()
		.filter_map(|f| f.name().map(str::to_owned))
		.collect();
	assert!(names.contains("D1.txt"), "D1 wiped! {names:?}");
	assert!(names.contains("D2.txt"), "D2 wiped! {names:?}");

	sc.cleanup();
}

// ===========================================================================
// OBSERV-19 — quarantine of a locally-removed file is counted, not lost
// ===========================================================================

#[shared_test_runtime]
async fn observ_19_quarantine_counted_not_data_loss() {
	// RemoteToLocal: a remote delete propagates as a LOCAL deletion (which quarantines locally).
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let client = sc.cache.client.clone();
	let payload = b"recoverable content C";
	let b = client.make_file_builder("q.txt", sc.remote).unwrap();
	let mut rf = client.upload_file(b, payload).await.unwrap();
	assert!(
		poll_for_item(sc.cache.db_path(), rf.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never saw q.txt"
	);

	let r1 = sc.sync().await;
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert!(read_eq(&sc.local, "q.txt", payload), "q.txt not downloaded");

	// Trash on remote; wait for the cache to drop it.
	client.trash_file(&mut rf).await.unwrap();
	assert!(
		poll_for_item_absent(sc.cache.db_path(), rf.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never dropped trashed q.txt"
	);

	let mut events = Vec::new();
	let r2 = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.locally_deleted, 1, "removal must be counted: {r2:?}");
	// The deletion surfaced as a "DeletingLocal" (quarantine) event, not a hard-destroy claim.
	assert!(
		events
			.iter()
			.any(|e| matches!(e, SyncEvent::DeletingLocal { .. })),
		"removal must surface as a local-delete/quarantine event: {events:?}"
	);
	// The live path is gone ...
	assert!(!sc.local.join("q.txt").exists(), "q.txt still live");
	// ... but the prior content is recoverable from the quarantine bin, byte-identical.
	let trash = sc.local.join(".filen-sync-trash");
	let mut recovered = false;
	if trash.is_dir() {
		let mut stack = vec![trash];
		while let Some(d) = stack.pop() {
			for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
				let p = entry.path();
				if p.is_dir() {
					stack.push(p);
				} else if std::fs::read(&p).ok().as_deref() == Some(payload.as_slice()) {
					recovered = true;
				}
			}
		}
	}
	assert!(
		recovered,
		"deleted content not recoverable from quarantine — data loss"
	);

	sc.cleanup();
}

// ===========================================================================
// OBSERV-20 — watch-mode self-writes do not generate phantom counts / loops
// ===========================================================================

// Built inline (not via `single_client`) because `watch_observed` takes `Arc<SyncEngine>` and
// `SyncEngine` is not `Clone`, so the engine must be owned by an `Arc` from construction.
#[shared_test_runtime]
async fn observ_20_watch_self_writes_no_phantom_counts() {
	use std::sync::Mutex;
	use std::time::Duration;

	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir("obs20");

	let engine = Arc::new(
		SyncEngine::open(cache.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair = engine
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();

	// Record every PassCompleted report + count every per-action event from the continuous watch.
	let reports: Arc<Mutex<Vec<SyncReport>>> = Arc::new(Mutex::new(Vec::new()));
	let action_events: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
	let r2 = reports.clone();
	let a2 = action_events.clone();
	let observer: filen_sdk_rs::sync_engine::SyncObserver = Box::new(move |e: SyncEvent| {
		if is_action_event(&e) {
			*a2.lock().unwrap() += 1;
		}
		if let SyncEvent::PassCompleted { report } = e {
			r2.lock().unwrap().push(report);
		}
	});
	let handle = engine.clone().watch_observed(pair, observer).await.unwrap();

	// One external local change.
	write_file(&local, "w.txt", b"watched payload");

	// Wait until a pass reported the upload.
	let uploaded = poll_until(CACHE_CONVERGE_TIMEOUT, || {
		reports.lock().unwrap().iter().any(|r| r.uploaded >= 1)
	})
	.await;
	assert!(uploaded, "watch never reported the upload");

	// Snapshot the action-event count after the upload settled, then wait through additional
	// debounce + safety-net windows with NO further external change.
	tokio::time::sleep(Duration::from_secs(8)).await;
	let actions_after_settle = *action_events.lock().unwrap();
	tokio::time::sleep(Duration::from_secs(8)).await;
	let actions_final = *action_events.lock().unwrap();

	// No further per-action work from the engine's own writes (no re-sync loop).
	assert_eq!(
		actions_final, actions_after_settle,
		"self-induced passes must not apply phantom actions (no re-sync loop)"
	);
	// Total uploads across all reports is exactly 1.
	let total_uploaded: usize = reports.lock().unwrap().iter().map(|r| r.uploaded).sum();
	assert_eq!(total_uploaded, 1, "w.txt must be uploaded exactly once");

	// w.txt present on the remote.
	let client = cache.client.clone();
	let dir = client.get_dir(remote).await.unwrap();
	let (_d, files) = client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	assert!(
		files.iter().any(|f| f.name() == Some("w.txt")),
		"w.txt missing on remote"
	);

	drop(handle);
	std::fs::remove_dir_all(&local).ok();
}

// ===========================================================================
// OBSERV-25 — metadata-only (mtime) change does not produce a transfer count
// ===========================================================================

#[shared_test_runtime]
async fn observ_25_metadata_only_change_no_transfer() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "stable.txt", b"identical bytes never change");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Re-write the SAME bytes (content unchanged; mtime/metadata churns).
	tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
	write_file(&sc.local, "stable.txt", b"identical bytes never change");

	let mut events = Vec::new();
	let r2 = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	// Identical content must NOT re-transfer.
	assert_eq!(
		r2.uploaded, 0,
		"unchanged content must not re-upload: {r2:?}"
	);
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	assert_eq!(
		r2.conflicts.len(),
		0,
		"no false conflict for unchanged file: {r2:?}"
	);
	assert!(
		!events
			.iter()
			.any(|e| matches!(e, SyncEvent::Uploading { .. })),
		"no upload event for an unchanged file: {events:?}"
	);

	// A third pass must be a clean no-op (baseline advanced).
	let r3 = sc.sync().await;
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(applied_total(&r3), 0, "{r3:?}");

	sc.cleanup();
}

// ===========================================================================
// (add) — backup mode reports an ignored source delete distinctly from applied work
// ===========================================================================

#[shared_test_runtime]
async fn observ_add_backup_ignored_delete_not_miscounted() {
	let sc = single_client(SyncMode::LocalBackup).await;
	for name in ["f1.txt", "f2.txt", "f3.txt"] {
		write_file(&sc.local, name, content_for(name).as_slice());
	}
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 3, "{r1:?}");

	// Delete f2 (must be ignored by backup mode) AND modify f3 (must re-push).
	std::fs::remove_file(sc.local.join("f2.txt")).unwrap();
	write_file(&sc.local, "f3.txt", b"f3-modified-longer");

	let mut events = Vec::new();
	let r2 = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 1, "only f3 re-push: {r2:?}");
	assert_eq!(
		r2.remotely_trashed, 0,
		"backup must not mirror f2's delete: {r2:?}"
	);
	assert_eq!(
		r2.held_deletions, 0,
		"an ignored backup delete is not a held delete: {r2:?}"
	);
	// The ignored delete is NOT counted in any applied bucket, and emits no trash event.
	assert_eq!(
		applied_total(&r2),
		1,
		"only the f3 re-push is applied work: {r2:?}"
	);
	assert!(
		!events
			.iter()
			.any(|e| matches!(e, SyncEvent::TrashingRemote { .. })),
		"no trash event for an ignored backup delete: {events:?}"
	);

	// f2 survives on the remote, byte-intact.
	assert!(
		list_remote_files(&sc)
			.await
			.iter()
			.any(|f| f.name() == Some("f2.txt")),
		"backup destination must retain f2.txt"
	);

	sc.cleanup();
}

// ===========================================================================
// (add) — net-new dir-create distinguished from a pre-existing destination dir
// ===========================================================================

#[shared_test_runtime]
async fn observ_add_no_phantom_dir_create_for_existing_dir() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	// Establish dirA in the baseline (created on the first pass).
	write_file(&sc.local, "dirA/seed.txt", b"seed");
	let r1 = sc.sync().await;
	assert_eq!(
		r1.remote_dirs_created, 1,
		"dirA created on first pass: {r1:?}"
	);
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Now add dirA/new.txt (into the EXISTING dirA) and a brand-new dirB/.
	write_file(&sc.local, "dirA/new.txt", b"new");
	std::fs::create_dir_all(sc.local.join("dirB")).unwrap();

	let mut events = Vec::new();
	let r2 = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	// Only dirB is net-new; dirA must NOT be re-counted.
	assert_eq!(r2.remote_dirs_created, 1, "only dirB is new: {r2:?}");
	assert_eq!(r2.uploaded, 1, "only new.txt: {r2:?}");
	assert_eq!(applied_total(&r2), 2, "{r2:?}");
	// No dir-create event for the pre-existing dirA.
	assert!(
		!events.iter().any(|e| matches!(
			e,
			SyncEvent::CreatingRemoteDir { rel_path } if rel_path == "dirA"
		)),
		"no phantom dir-create for the pre-existing dirA: {events:?}"
	);
	assert!(
		events.iter().any(|e| matches!(
			e,
			SyncEvent::CreatingRemoteDir { rel_path } if rel_path == "dirB"
		)),
		"dirB create event missing: {events:?}"
	);

	sc.cleanup();
}

// ===========================================================================
// (add) — a SLOW progress listener does not lose events or abort the pass
// ===========================================================================

// The observer is a `FnMut` called synchronously; a Rust observer cannot "throw" without unwinding
// through the engine (the plan's throwing-listener leg), so the genuinely-supported robustness
// variant is a SLOW listener: it sleeps on every event and must still receive every event while the
// pass completes and applies all work.
#[shared_test_runtime]
async fn observ_add_slow_listener_does_not_lose_events() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	for i in 0..6 {
		write_file(&sc.local, &format!("s{i}.txt"), format!("c{i}").as_bytes());
	}

	let mut events = Vec::new();
	let report = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| {
			// Brief synchronous delay on every event (a misbehaving-but-not-throwing UI callback).
			std::thread::sleep(std::time::Duration::from_millis(5));
			events.push(e);
		})
		.await
		.unwrap();

	assert!(
		report.errors.is_empty(),
		"slow listener must not abort the pass: {report:?}"
	);
	assert_eq!(report.uploaded, 6, "{report:?}");
	// The slow listener still received exactly one event per applied action.
	assert_eq!(
		action_event_count(&events),
		applied_total(&report),
		"events: {events:?}"
	);
	assert_eq!(
		events
			.iter()
			.filter(|e| matches!(e, SyncEvent::PassCompleted { .. }))
			.count(),
		1,
		"exactly one PassCompleted despite a slow listener: {events:?}"
	);

	sc.cleanup();
}

// ===========================================================================
// BLOCKED — require infrastructure the current harness / public API does not provide.
// ===========================================================================

#[ignore = "blocked: needs per-item transfer-failure injection harness — see OBSERV-08 TODO"]
#[shared_test_runtime]
async fn observ_08_error_per_failed_action_pass_continues() {
	// plan: 5 queued uploads, induce exactly 1 transfer failure; assert errors == 1, uploaded == 4,
	// the pass does not abort, the 4 healthy files land byte-identical, the failing path is named in
	// report.errors and its baseline is NOT advanced (retried next pass). Needs a fault injector to
	// make exactly one transfer fail deterministically.
}

#[ignore = "blocked: needs a configurable parallel-apply knob + concurrency race harness — see OBSERV-12 TODO"]
#[shared_test_runtime]
async fn observ_12_events_under_concurrency_complete_unique_wellformed() {
	// plan: apply ~200 mixed actions with parallelism, record every event with a sequence number,
	// assert event set == applied set (no dup/miss/torn), per-path start-before-complete ordering,
	// and report counters == per-type event tallies. The public observer is delivered synchronously
	// in happen-before order (no parallel-apply or torn-event scenario is reachable black-box).
}

/// OBSERV-14 — `plan_pair` is a true dry run: it reports every intended action with its path,
/// mutates neither tree, the quarantine area nor the persisted baseline, and the real pass that
/// follows applies exactly what was predicted.
#[shared_test_runtime]
async fn observ_14_dry_run_reports_intent_mutates_nothing() {
	let sc = single_client(SyncMode::TwoWay).await;

	// A mixed changeset: two new files in a new directory, plus one already-synced file.
	write_file(&sc.local, "kept.txt", b"already synced");
	assert_eq!(sc.sync().await.uploaded, 1);
	write_file(&sc.local, "sub/one.txt", b"first");
	write_file(&sc.local, "sub/two.txt", b"second");

	let before_local = walk_tree(&sc.local);
	let before_remote: Vec<String> = list_remote_files(&sc)
		.await
		.iter()
		.filter_map(|f| f.name().map(str::to_string))
		.collect();

	let PlanOutcome::Planned {
		actions,
		held_deletions,
		conflicts,
		guard_message,
		pass_token,
	} = sc.engine.plan_pair(sc.pair).await.unwrap()
	else {
		panic!("the dry run must not refuse this pair");
	};
	assert!(held_deletions.is_empty(), "{held_deletions:?}");
	assert!(conflicts.is_empty(), "{conflicts:?}");
	assert!(guard_message.is_none(), "{guard_message:?}");
	assert!(pass_token.is_none(), "no held batch, so no token");

	// Every intended action names its path, and the intended count is the breakdown.
	assert_eq!(
		actions.len(),
		3,
		"one dir create + two uploads: {actions:?}"
	);
	assert!(
		actions.iter().any(|a| a.contains("\"sub\"")),
		"the plan must name the new directory: {actions:?}"
	);
	for name in ["sub/one.txt", "sub/two.txt"] {
		assert!(
			actions.iter().any(|a| a.contains(name)),
			"the plan must name {name}: {actions:?}"
		);
	}
	assert!(
		!actions.iter().any(|a| a.contains("kept.txt")),
		"an already-synced file must not be planned: {actions:?}"
	);

	// NOTHING moved: local tree, remote listing and the quarantine area are all unchanged.
	assert_eq!(
		walk_tree(&sc.local),
		before_local,
		"the dry run wrote locally"
	);
	let after_remote: Vec<String> = list_remote_files(&sc)
		.await
		.iter()
		.filter_map(|f| f.name().map(str::to_string))
		.collect();
	assert_eq!(
		after_remote, before_remote,
		"the dry run wrote to the remote"
	);
	assert!(
		!sc.local.join(".filen-sync-trash").exists(),
		"the dry run created a quarantine bin"
	);

	// The baseline is untouched too: re-planning yields exactly the same plan.
	let PlanOutcome::Planned { actions: again, .. } = sc.engine.plan_pair(sc.pair).await.unwrap()
	else {
		panic!("second dry run refused");
	};
	assert_eq!(again, actions, "the dry run advanced the baseline");

	// The real pass applies exactly what was predicted.
	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "{report:?}");
	assert_eq!(applied_total(&report), actions.len(), "{report:?}");
	assert_eq!(report.uploaded, 2, "{report:?}");
	assert_eq!(report.remote_dirs_created, 1, "{report:?}");

	sc.cleanup();
}

#[ignore = "blocked: needs deterministic mid-pass interruption/abort — see OBSERV-16 TODO"]
#[shared_test_runtime]
async fn observ_16_rerun_after_partial_reports_only_remaining() {
	// plan: stage 10 uploads, interrupt after ~4 applied, re-run to completion; assert the second
	// pass reports uploaded == 6 (not 10), no file is re-uploaded, each of the 10 is uploaded exactly
	// once total, final remote has all 10 byte-identical, errors == 0. Needs a way to abort a pass
	// after a known number of applied actions (fault-injection / crash harness).
}

#[ignore = "blocked: needs a transient subsystem-error injector — see OBSERV-21 TODO"]
#[shared_test_runtime]
async fn observ_21_report_emitted_exactly_once_even_with_errors() {
	// plan: inject a transient mid-pass subsystem error (e.g. partial remote-listing failure / a few
	// item failures); assert exactly one final PassCompleted report is emitted (not zero/two), it
	// carries errors > 0 with per-error detail and accurate applied-counts, no premature report fires,
	// and the pass terminates cleanly. Needs fault injection to produce the error path deterministically.
}

#[ignore = "blocked: SyncEvent carries no byte/size fields — see OBSERV-22 TODO"]
#[shared_test_runtime]
async fn observ_22_byte_size_accounting_matches_transferred() {
	// plan: transfer files of known sizes (0 B, 1 B, 1 MiB, multi-chunk); assert each action's
	// reported total size == true byte size, cumulative transferred == size at completion (incl. the
	// 0-byte file), destination byte-identical, multi-chunk progress monotone and <= total. The
	// public SyncEvent variants carry only rel_path — no size/bytes/progress fields exist.
}

/// OBSERV-23 — a held mass delete reports as HELD with zero deletion events, and once approved by
/// its token the next pass reports exactly those deletions, one event each, with nothing still held.
#[shared_test_runtime]
async fn observ_23_held_then_confirmed_reported_as_deletes() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	const TOTAL: usize = 40;
	const DELETE: usize = 30;
	for i in 0..TOTAL {
		write_file(
			&sc.local,
			&format!("f{i:03}.txt"),
			format!("v{i}").as_bytes(),
		);
	}
	assert_eq!(sc.sync().await.uploaded, TOTAL);
	for i in 0..DELETE {
		std::fs::remove_file(sc.local.join(format!("f{i:03}.txt"))).unwrap();
	}

	// Pass 1: held, and NOT ONE deletion event fires.
	let mut events = Vec::new();
	let held_report = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert!(held_report.errors.is_empty(), "{held_report:?}");
	assert_eq!(held_report.held_deletions, DELETE, "{held_report:?}");
	assert_eq!(held_report.remotely_trashed, 0, "{held_report:?}");
	assert_eq!(
		events
			.iter()
			.filter(|e| matches!(e, SyncEvent::TrashingRemote { .. }))
			.count(),
		0,
		"a held batch must fire no deletion events: {events:?}"
	);
	let token = events
		.iter()
		.find_map(|e| match e {
			SyncEvent::DeletionsHeld {
				count, pass_token, ..
			} => {
				assert_eq!(*count, DELETE, "held event count disagrees with the report");
				Some(pass_token.clone())
			}
			_ => None,
		})
		.expect("a DeletionsHeld event must carry the batch token");
	assert_eq!(
		Some(token.clone()),
		held_report.deletion_token,
		"the event and the report must name the same batch"
	);

	// Approve exactly that batch; pass 2 applies it, one event per deletion, nothing left held.
	sc.engine.approve_deletions(sc.pair, &token).await;
	let mut events = Vec::new();
	let applied = sc
		.engine
		.sync_once_observed(sc.pair, &mut |e| events.push(e))
		.await
		.unwrap();
	assert!(applied.errors.is_empty(), "{applied:?}");
	assert_eq!(applied.remotely_trashed, DELETE, "{applied:?}");
	assert_eq!(applied.held_deletions, 0, "{applied:?}");
	assert!(applied.deletion_token.is_none(), "{applied:?}");
	assert_eq!(
		events
			.iter()
			.filter(|e| matches!(e, SyncEvent::TrashingRemote { .. }))
			.count(),
		DELETE,
		"one deletion event per applied deletion"
	);
	assert!(
		!events
			.iter()
			.any(|e| matches!(e, SyncEvent::DeletionsHeld { .. })),
		"nothing may still be held: {events:?}"
	);

	// The destination reflects exactly DELETE removals.
	assert_eq!(list_remote_files(&sc).await.len(), TOTAL - DELETE);

	sc.cleanup();
}

#[ignore = "blocked: needs a per-item transfer-failure injector — see OBSERV-24 TODO"]
#[shared_test_runtime]
async fn observ_24_conflicts_and_errors_do_not_inflate_success() {
	// plan: stage 1 genuine two-way conflict + 1 induced transfer error + 3 clean uploads; assert
	// uploaded == 3, conflicts == 1, errors == 1 (each with its path), the conflict/error paths are
	// NOT in uploaded/downloaded/moved/deleted, and the event log has exactly 3 upload events plus a
	// conflict + error event. The induced-error leg needs a fault injector; the conflict-no-inflation
	// aspect alone overlaps OBSERV-06.
}

#[ignore = "blocked: SyncReport carries no timestamp/duration fields — see review-add TODO"]
#[shared_test_runtime]
async fn observ_add_report_timestamps_present_and_coherent() {
	// plan: capture pass-start/pass-complete times and every per-action event timestamp; assert the
	// report has started-at/finished-at (or duration) with finished-at >= started-at, every event
	// timestamp falls within that bracket, duration matches wall-clock within tolerance, and a no-op
	// pass still has a non-negative duration. SyncReport/SyncEvent expose no timing fields.
}

#[ignore = "blocked: no mid-pass listener (un)registration API — see review-add TODO"]
#[shared_test_runtime]
async fn observ_add_listener_lifecycle_mid_pass() {
	// plan: register L1 at pass start, register L2 after the pass began applying, unregister L1
	// partway; assert each listener gets a consistent event subset (no dup, no delivery after
	// unregister) and the final report still equals the true applied total. sync_once_observed takes
	// a single observer for the whole pass; there is no add/remove-listener-mid-pass API.
}

#[ignore = "blocked: no pass-identifier field on SyncReport/SyncEvent — see review-add TODO"]
#[shared_test_runtime]
async fn observ_add_unique_pass_identifier_correlation() {
	// plan: run two sequential passes (2 uploads, then 1 delete); assert the two reports carry
	// distinct pass identifiers, every per-action event maps to exactly one pass id matching its
	// report, no cross-pass attribution, and each report's counters match the count of events bearing
	// its id. Neither SyncReport nor SyncEvent exposes a pass identifier.
}

#[ignore = "blocked: a throwing observer would unwind through the engine (undefined for these tests) — slow-listener variant implemented as observ_add_slow_listener_does_not_lose_events"]
#[shared_test_runtime]
async fn observ_add_throwing_listener_does_not_corrupt() {
	// plan: register one throwing listener + one well-behaved recorder; assert the pass completes and
	// applies all N actions, the recorder still gets exactly N events, the report counters == N, and
	// exactly one final report fires. A Rust observer cannot "throw" without panicking/unwinding
	// through the synchronous observer call site; the supported slow-listener robustness leg is
	// covered by observ_add_slow_listener_does_not_lose_events.
}
