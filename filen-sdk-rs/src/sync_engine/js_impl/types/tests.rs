use std::{path::PathBuf, sync::Arc};

use super::*;
use crate::{
	auth::test_support::offline_client,
	cache::open_sqlite,
	sync_engine::{IgnoreLevel, PlannedActionKind, PlannedNodeKind, SyncEngine, UnsyncableReason},
};

fn conflict(index: usize) -> PlannedConflict {
	PlannedConflict {
		rel_path: format!("conflict-{index}"),
		local: Some(PlannedNodeKind::File),
		remote: None,
	}
}

fn trash(index: usize) -> PlannedAction {
	PlannedAction {
		kind: PlannedActionKind::TrashRemote,
		rel_path: format!("trash-{index}"),
		node: PlannedNodeKind::File,
		size: Some(1),
		transfer: None,
	}
}

fn report_with(entries: usize) -> SyncReport {
	SyncReport {
		conflicts: (0..entries).map(conflict).collect(),
		held: (0..entries).map(trash).collect(),
		unsyncable: (0..entries)
			.map(|index| UnsyncablePath {
				rel_path: format!("unsyncable-{index}"),
				reason: UnsyncableReason::RemoteUndecodable,
			})
			.collect(),
		ignored: (0..entries)
			.map(|index| IgnoredPath {
				rel_path: format!("ignored-{index}"),
				level: IgnoreLevel::User,
				pattern: "*.tmp".to_owned(),
				tracked: false,
			})
			.collect(),
		errors: (0..entries).map(|index| format!("error-{index}")).collect(),
		quarantined: (0..entries)
			.map(|index| format!("quarantined-{index}"))
			.collect(),
		..SyncReport::default()
	}
}

fn list_lengths(report: &SyncPassReport) -> [usize; 6] {
	[
		report.conflicts.len(),
		report.held.len(),
		report.unsyncable.len(),
		report.ignored.len(),
		report.errors.len(),
		report.quarantined.len(),
	]
}

fn totals(report: &SyncPassReport) -> [u64; 6] {
	[
		report.conflicts_total,
		report.held_total,
		report.unsyncable_total,
		report.ignored_total,
		report.errors_total,
		report.quarantined_total,
	]
}

#[test]
fn a_report_at_the_limit_keeps_every_entry() {
	let report = SyncPassReport::from(report_with(1000));

	assert_eq!(list_lengths(&report), [1000; 6]);
	assert_eq!(totals(&report), [1000; 6]);
	assert_eq!(report.held_deletions, 1000);
}

#[test]
fn a_report_past_the_limit_keeps_the_first_entries_and_counts_them_all() {
	let report = SyncPassReport::from(report_with(1001));

	assert_eq!(list_lengths(&report), [1000; 6]);
	assert_eq!(totals(&report), [1001; 6]);
	assert_eq!(report.held_deletions, 1001);
	assert_eq!(report.conflicts.first(), Some(&conflict(0)));
	assert_eq!(report.conflicts.last(), Some(&conflict(999)));
	assert_eq!(report.errors.last().map(String::as_str), Some("error-999"));
}

#[test]
fn a_report_carries_its_counts_and_why_its_pass_failed() {
	let report = SyncReport {
		downloaded: 2,
		uploaded: 3,
		interrupted: 4,
		halted: Some(HaltReason::LocalStorageFull),
		guard: Some(GuardReason::RemoteEmptied { deletions: 5 }),
		deletion_token: Some("token".to_owned()),
		..SyncReport::default()
	};
	let mirrored = SyncPassReport::from(report);

	assert_eq!(mirrored.downloaded, 2);
	assert_eq!(mirrored.uploaded, 3);
	assert_eq!(mirrored.interrupted, 4);
	assert_eq!(mirrored.halted, Some(HaltReason::LocalStorageFull));
	assert_eq!(
		mirrored.guard,
		Some(SyncGuardReason::RemoteEmptied { deletions: 5 })
	);
	assert_eq!(mirrored.deletion_token.as_deref(), Some("token"));
	assert_eq!(
		mirrored.pass_error.as_deref(),
		Some("the local disk is full: no further downloads were started this pass")
	);
	assert_eq!(mirrored.pass_error_kind, Some(ErrorKind::IO));
	let healthy = SyncPassReport::from(SyncReport::default());
	assert_eq!(healthy.pass_error, None);
	assert_eq!(healthy.pass_error_kind, None);
}

#[test]
fn a_report_names_the_paths_its_scan_could_not_read_and_why() {
	let report = SyncReport {
		guard: Some(GuardReason::ScanIncomplete),
		scan_gaps: vec![
			ScanGap {
				rel_path: "locked".to_owned(),
				cause: ScanGapCause::Unreadable {
					kind: std::io::ErrorKind::PermissionDenied,
				},
			},
			ScanGap {
				rel_path: "moving".to_owned(),
				cause: ScanGapCause::ChangedDuringScan,
			},
			ScanGap {
				rel_path: "bad\u{fffd}".to_owned(),
				cause: ScanGapCause::NonUtf8Name,
			},
		],
		scan_gaps_total: 250,
		..SyncReport::default()
	};
	let mirrored = SyncPassReport::from(report);

	assert_eq!(
		mirrored.scan_gaps,
		vec![
			SyncScanGap {
				rel_path: "locked".to_owned(),
				cause: SyncScanGapCause::Unreadable {
					kind: "PermissionDenied".to_owned(),
				},
			},
			SyncScanGap {
				rel_path: "moving".to_owned(),
				cause: SyncScanGapCause::ChangedDuringScan,
			},
			SyncScanGap {
				rel_path: "bad\u{fffd}".to_owned(),
				cause: SyncScanGapCause::NonUtf8Name,
			},
		]
	);
	assert_eq!(mirrored.scan_gaps_total, 250);
}

#[test]
fn guard_reasons_map_with_their_counts() {
	let cases = [
		(GuardReason::ScanIncomplete, SyncGuardReason::ScanIncomplete),
		(
			GuardReason::FirstSyncWithDeletions { deletions: 1 },
			SyncGuardReason::FirstSyncWithDeletions { deletions: 1 },
		),
		(
			GuardReason::RemoteUnconverged { deletions: 2 },
			SyncGuardReason::RemoteUnconverged { deletions: 2 },
		),
		(
			GuardReason::RemoteEmptied { deletions: 3 },
			SyncGuardReason::RemoteEmptied { deletions: 3 },
		),
		(
			GuardReason::ExceededThreshold {
				deletions: 40,
				limit: 10,
			},
			SyncGuardReason::ExceededThreshold {
				deletions: 40,
				limit: 10,
			},
		),
	];
	for (reason, expected) in cases {
		assert_eq!(SyncGuardReason::from(reason), expected);
	}
}

#[test]
fn events_map_with_their_payloads() {
	let cases = [
		(
			SyncEvent::Planned { actions: 7 },
			SyncEventMessage::Planned { actions: 7 },
		),
		(
			SyncEvent::DeletionsHeld {
				count: 12,
				reason: "too many".to_owned(),
				pass_token: "token".to_owned(),
			},
			SyncEventMessage::DeletionsHeld {
				count: 12,
				reason: "too many".to_owned(),
				pass_token: "token".to_owned(),
			},
		),
		(
			SyncEvent::Progress {
				rel_path: "a/b.txt".to_owned(),
				direction: TransferDirection::Download,
				bytes: 10,
				total: 20,
			},
			SyncEventMessage::Progress {
				rel_path: "a/b.txt".to_owned(),
				direction: TransferDirection::Download,
				bytes: 10,
				total: 20,
			},
		),
		(
			SyncEvent::Scanning {
				progress: LocalScanProgress {
					entries: 9,
					files_to_hash: 4,
					bytes_to_hash: 400,
					files_hashed: 3,
					bytes_hashed: 350,
				},
			},
			SyncEventMessage::Scanning {
				progress: LocalScanProgress {
					entries: 9,
					files_to_hash: 4,
					bytes_to_hash: 400,
					files_hashed: 3,
					bytes_hashed: 350,
				},
			},
		),
		(
			SyncEvent::Quarantined {
				rel_path: "a/b.txt".to_owned(),
				bin_path: PathBuf::from("/root/.filen-sync-trash/b.txt"),
			},
			SyncEventMessage::Quarantined {
				rel_path: "a/b.txt".to_owned(),
				bin_path: "/root/.filen-sync-trash/b.txt".to_owned(),
			},
		),
		(
			SyncEvent::MovingLocal {
				from: "a".to_owned(),
				to: "b".to_owned(),
			},
			SyncEventMessage::MovingLocal {
				from: "a".to_owned(),
				to: "b".to_owned(),
			},
		),
		(
			SyncEvent::Interrupted { actions: 3 },
			SyncEventMessage::Interrupted { actions: 3 },
		),
		(
			SyncEvent::PassFailed {
				error: "gone".to_owned(),
				kind: ErrorKind::Unauthenticated,
			},
			SyncEventMessage::PassFailed {
				error: "gone".to_owned(),
				kind: ErrorKind::Unauthenticated,
			},
		),
	];
	for (event, expected) in cases {
		assert_eq!(SyncEventMessage::from(event), expected);
	}
}

#[test]
fn a_completed_pass_carries_its_capped_report() {
	let message = SyncEventMessage::from(SyncEvent::PassCompleted {
		report: report_with(1001),
	});

	let SyncEventMessage::PassCompleted { report } = message else {
		panic!("a completed pass mapped to {message:?}");
	};
	assert_eq!(list_lengths(&report), [1000; 6]);
	assert_eq!(totals(&report), [1001; 6]);
}

#[test]
fn delete_guards_map_to_their_state() {
	assert_eq!(
		DeleteGuardState::from(DeleteGuard::default()),
		DeleteGuardState::Limited {
			floor: 10,
			ratio: 0.5
		}
	);
	assert_eq!(
		DeleteGuardState::from(DeleteGuard::unlimited()),
		DeleteGuardState::Unlimited
	);
	assert_eq!(
		DeleteGuardState::from(DeleteGuard::new(3, 0.25).unwrap()),
		DeleteGuardState::Limited {
			floor: 3,
			ratio: 0.25
		}
	);
}

#[test]
fn pause_options_map_both_ways() {
	let options = PauseOptions::from(SyncPauseOptions {
		mode: PauseMode::Suspend,
		cancel_after_ms: Some(1500),
	});
	assert_eq!(options.mode, PauseMode::Suspend);
	assert_eq!(options.cancel_after, Some(Duration::from_millis(1500)));

	let never = PauseOptions::from(SyncPauseOptions {
		mode: PauseMode::Cancel,
		cancel_after_ms: None,
	});
	assert_eq!(never.cancel_after, None);

	assert_eq!(
		SyncPauseOptions::from(PauseOptions::default()),
		SyncPauseOptions {
			mode: PauseMode::Suspend,
			cancel_after_ms: Some(300_000),
		}
	);
}

#[test]
fn a_negative_pair_id_is_an_internal_error() {
	assert_eq!(pair_id_out(7).unwrap(), 7);
	assert_eq!(pair_id_out(-1).unwrap_err().kind(), ErrorKind::Internal);
}

/// The refusal a second engine on an open baseline fails with, read off the engine's own error.
#[tokio::test]
async fn a_baseline_in_use_reads_as_its_refusal() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("baseline.db");
	let _first = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();

	let error = match SyncEngine::open(Arc::new(offline_client()), path.clone()).await {
		Ok(_) => panic!("a second engine opened a baseline the first one has open"),
		Err(error) => error,
	};
	assert_eq!(
		sync_refusal(&error),
		Some(SyncRefusal::BaselineInUse {
			path: path.to_string_lossy().into_owned(),
		})
	);
}

/// The refusal an open of a baseline stamped at a schema version this build does not read fails
/// with, read off the engine's own error and naming both versions, and the DB left as it was.
#[tokio::test]
async fn a_baseline_at_another_schema_version_is_refused_as_a_typed_refusal() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("baseline.db");
	drop(
		SyncEngine::open(Arc::new(offline_client()), path.clone())
			.await
			.unwrap(),
	);
	let user_version = |conn: &rusqlite::Connection| -> i64 {
		conn.query_row("PRAGMA user_version", [], |row| row.get(0))
			.unwrap()
	};
	let conn = open_sqlite(&path).unwrap();
	let current = user_version(&conn);
	conn.execute_batch("PRAGMA user_version = 1000;").unwrap();
	drop(conn);

	let error = match SyncEngine::open(Arc::new(offline_client()), path.clone()).await {
		Ok(_) => panic!("an engine opened a baseline at another schema version"),
		Err(error) => error,
	};
	assert_eq!(error.kind(), ErrorKind::InvalidState);
	assert_eq!(
		sync_refusal(&error),
		Some(SyncRefusal::BaselineSchemaMismatch {
			path: path.to_string_lossy().into_owned(),
			found: 1000,
			expected: current,
		})
	);
	assert_eq!(
		user_version(&open_sqlite(&path).unwrap()),
		1000,
		"the refused DB was rewritten"
	);
}

/// The refusals `add_pair` fails with for overlapping roots, read off the engine's own errors,
/// naming the conflicting pair by its JS id.
#[tokio::test]
async fn an_overlapping_pair_reads_as_its_refusal() {
	let dir = tempfile::tempdir().unwrap();
	let outer = dir.path().join("outer");
	let inner = outer.join("inner");
	let other = dir.path().join("other");
	std::fs::create_dir_all(&inner).unwrap();
	std::fs::create_dir_all(&other).unwrap();
	let engine = SyncEngine::open(Arc::new(offline_client()), dir.path().join("baseline.db"))
		.await
		.unwrap();
	let remote = Uuid::from_u128(1);
	let pair = engine
		.add_pair(outer.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let pair = u64::try_from(pair).unwrap();

	let nested = engine
		.add_pair(inner, Uuid::from_u128(2), SyncMode::TwoWay)
		.await
		.unwrap_err();
	assert_eq!(
		sync_refusal(&nested),
		Some(SyncRefusal::LocalRootNested {
			pair,
			existing: std::fs::canonicalize(&outer)
				.unwrap()
				.to_string_lossy()
				.into_owned(),
		})
	);

	let same_remote = engine
		.add_pair(other, remote, SyncMode::TwoWay)
		.await
		.unwrap_err();
	assert_eq!(
		sync_refusal(&same_remote),
		Some(SyncRefusal::RemoteRootInUse {
			pair,
			existing: remote,
		})
	);
}

#[test]
fn an_error_without_a_refusal_has_none() {
	let error = Error::custom(ErrorKind::InvalidState, "unknown sync pair");
	assert_eq!(sync_refusal(&error), None);
}
