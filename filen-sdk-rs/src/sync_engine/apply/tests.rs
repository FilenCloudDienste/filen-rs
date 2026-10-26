use std::{sync::Mutex, time::Duration};

use filen_types::crypto::Blake3Hash;
use tokio::runtime::Runtime;
use uuid::Uuid;

use super::*;
use crate::{
	ErrorKind, alloc_meter,
	auth::test_support::offline_client,
	cache::{CacheState, hydrate_by_uuids},
	connect::ConnectedTargets,
	fs::bulk::create_dirs::CreateDirsError,
	sync::lock::test_support::{held_lock, undriven_runtime},
	sync_engine::{
		SyncMode,
		baseline::BaselineStore,
		changes::tests::{cacheable_dir, cacheable_file},
		stage::{PAGE_ROWS, PlanWriter, Stage},
	},
};

fn upload(rel_path: &str) -> SyncAction {
	SyncAction::UploadFile {
		rel_path: rel_path.to_string(),
	}
}

fn download(rel_path: &str) -> SyncAction {
	SyncAction::DownloadFile {
		rel_path: rel_path.to_string(),
		remote_uuid: Uuid::new_v4(),
	}
}

fn trash(rel_path: &str, kind: NodeKind) -> SyncAction {
	SyncAction::TrashRemote {
		rel_path: rel_path.to_string(),
		kind,
		remote_uuid: Uuid::new_v4(),
	}
}

/// Each entry carries what its action reads, read as the pass started, and nothing else: a
/// download its remote node and its row; an upload its local node, the remote node it versions
/// and the directory it lands in; a remote file move its local node, the row it moves from and
/// the directory it lands in; a local deletion nothing at all.
#[test]
fn each_entry_carries_what_its_action_reads() {
	let docs = Uuid::from_u128(1);
	let pulled = RemoteNode {
		remote_uuid: Uuid::from_u128(2),
		..remote_node("docs/a.txt", None)
	};
	let versioned = RemoteNode {
		remote_uuid: Uuid::from_u128(3),
		..remote_node("docs/new.txt", None)
	};
	let remote = Side::Whole(HashMap::from([
		(
			"docs".to_string(),
			RemoteNode {
				kind: NodeKind::Dir,
				remote_uuid: docs,
				..remote_node("docs", None)
			},
		),
		("docs/a.txt".to_string(), pulled.clone()),
		("docs/new.txt".to_string(), versioned.clone()),
	]));
	let local_at = |path: &str| Box::new(local_node(path, 10, None));
	let local = Side::Whole(HashMap::from(
		["docs/new.txt", "docs/renamed.txt", "gone.txt"]
			.map(|path| (path.to_string(), *local_at(path))),
	));
	let (row_a, row_old) = (
		baseline_file("docs/a.txt", None),
		baseline_file("old.txt", None),
	);
	let baseline = Baseline::from_rows([
		row_a.clone(),
		row_old.clone(),
		baseline_file("gone.txt", None),
	]);
	let actions = vec![
		SyncAction::DownloadFile {
			rel_path: "docs/a.txt".to_string(),
			remote_uuid: pulled.remote_uuid,
		},
		upload("docs/new.txt"),
		SyncAction::MoveRemote {
			from_path: "old.txt".to_string(),
			to_path: "docs/renamed.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: Uuid::from_u128(4),
		},
		SyncAction::DeleteLocal {
			rel_path: "gone.txt".to_string(),
			kind: NodeKind::File,
		},
	];
	let (_dir, stage) = Stage::in_temp_dir();
	let plan = PlannedEntry::stage_actions(&actions, baseline, local, remote, stage).unwrap();
	let mut entries = Vec::new();
	plan.pager()
		.sweep(None, |entry| entries.push(entry))
		.unwrap();
	let read: Vec<(&str, &PassStart)> = entries
		.iter()
		.map(|PlannedEntry { action, start }| (action.rel_path(), start))
		.collect();
	// In the order the apply runs them: the move, the two transfers (the download first), and
	// the deletion last.
	assert_eq!(
		read,
		[
			(
				"docs/renamed.txt",
				&PassStart::MoveRemoteFile {
					local: Some(local_at("docs/renamed.txt")),
					base_from: Some(Box::new(row_old)),
					parent_uuid: Some(docs),
				}
			),
			(
				"docs/a.txt",
				&PassStart::Download {
					remote: Some(Box::new(pulled)),
					base: Some(Box::new(row_a)),
				}
			),
			(
				"docs/new.txt",
				&PassStart::Upload {
					local: Some(local_at("docs/new.txt")),
					remote: Some(Box::new(versioned)),
					base: None,
					parent_uuid: Some(docs),
				}
			),
			("gone.txt", &PassStart::Nothing),
		]
	);
}

/// A hold runs its course on time or on count, whichever comes first.
#[test]
fn a_hold_is_spent_after_its_time_or_its_batch() {
	let budget = LockBudget::default();
	let since = Instant::now();
	assert!(!hold_spent(&budget, since, 0, since));
	assert!(!hold_spent(
		&budget,
		since,
		budget.batch - 1,
		since + budget.hold - Duration::from_millis(1)
	));
	assert!(hold_spent(&budget, since, 0, since + budget.hold));
	assert!(hold_spent(&budget, since, budget.batch, since));
}

/// Without a held lock only a remote write has to wait for one; nothing that is merely local or a
/// download does, released or not.
#[test]
fn only_a_remote_write_waits_for_the_lock() {
	let now = Instant::now();
	let budget = LockBudget::default();
	for lease in [Lease::Unheld, Lease::Released { at: now }] {
		assert!(lease.admits(&budget, &download("a"), true, now));
		assert!(!lease.admits(&budget, &trash("b", NodeKind::File), false, now));
		assert!(!lease.admits(&budget, &upload("c"), true, now));
		assert!(lease.admits(
			&budget,
			&SyncAction::CreateLocalDir {
				rel_path: "d".to_string()
			},
			true,
			now
		));
	}
}

fn over_quota() -> crate::Error {
	filen_types::error::ResponseError::ApiError {
		message: None,
		code: Some("max_storage_reached".to_string()),
	}
	.into()
}

fn io_kind(kind: std::io::ErrorKind) -> std::io::Error {
	std::io::Error::from(kind)
}

/// The SDK reports byte deltas; the engine turns them into a running total for the path, with
/// the transfer's direction and the planned size alongside.
#[test]
fn progress_callback_reports_a_running_total() {
	let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
	let callback = progress_callback(&sender, "dir/f.bin", TransferDirection::Upload, 12);
	callback(5);
	callback(7);
	drop(callback);
	let progress = |bytes| SyncEvent::Progress {
		rel_path: "dir/f.bin".to_string(),
		direction: TransferDirection::Upload,
		bytes,
		total: 12,
	};
	assert_eq!(receiver.try_recv().unwrap(), progress(5));
	assert_eq!(receiver.try_recv().unwrap(), progress(12));
	assert!(receiver.try_recv().is_err());
}

/// A full disk or a full account is found wherever the SDK put it: straight from a local write
/// (`?` on an io result, or the engine's own `io_err`), from a server response, and inside the
/// `io::Error` the upload writer wraps a server refusal in. Anything else stays a per-path
/// failure.
#[test]
fn storage_full_is_found_anywhere_in_the_error_chain() {
	use std::io::ErrorKind as Io;

	let local = Some(HaltReason::LocalStorageFull);
	assert_eq!(
		pass_halt(&crate::Error::from(io_kind(Io::StorageFull))),
		local
	);
	assert_eq!(pass_halt(&io_err(io_kind(Io::StorageFull))), local);
	assert_eq!(pass_halt(&io_err(io_kind(Io::QuotaExceeded))), local);
	assert_eq!(
		pass_halt(&crate::Error::from(std::io::Error::other(io_err(io_kind(
			Io::StorageFull
		))))),
		local
	);

	let remote = Some(HaltReason::RemoteStorageFull);
	assert_eq!(pass_halt(&over_quota()), remote);
	assert_eq!(pass_halt(&over_quota().with_context("uploading")), remote);
	assert_eq!(
		pass_halt(&crate::Error::from(std::io::Error::other(over_quota()))),
		remote,
		"the upload writer hands a failed chunk on inside an io::Error"
	);
	assert_eq!(
		pass_halt(&crate::Error::custom_with_source(
			crate::ErrorKind::Server,
			filen_types::error::ResponseError::ApiError {
				message: None,
				code: Some("max_storage_reached".to_string()),
			},
			None::<String>,
		)),
		remote,
		"the server's code decides, whatever kind it was wrapped under"
	);

	assert_eq!(
		pass_halt(&crate::Error::from(io_kind(Io::PermissionDenied))),
		None
	);
	assert_eq!(
		pass_halt(&crate::Error::from(std::io::Error::other("disk on fire"))),
		None
	);
	assert_eq!(
		pass_halt(
			&filen_types::error::ResponseError::ApiError {
				message: None,
				code: Some("file_not_found".to_string()),
			}
			.into()
		),
		None
	);
	assert_eq!(pass_halt(&internal("boom")), None);
}

/// A storage-full failure halts the pass once, with one line saying why, and counts against no
/// path; an ordinary failure next to it still counts against its own.
#[test]
fn a_storage_full_failure_halts_the_pass_instead_of_counting_against_its_path() {
	let mut report = SyncReport::default();
	note_failure(
		&mut report,
		"locked.txt",
		&crate::Error::from(io_kind(std::io::ErrorKind::PermissionDenied)),
	);
	note_failure(
		&mut report,
		"big.bin",
		&io_err(io_kind(std::io::ErrorKind::StorageFull)),
	);
	note_failure(
		&mut report,
		"again.bin",
		&io_err(io_kind(std::io::ErrorKind::StorageFull)),
	);
	note_failure(&mut report, "other.bin", &over_quota());

	assert_eq!(report.halted, Some(HaltReason::LocalStorageFull));
	assert_eq!(
		report
			.failed_paths
			.iter()
			.map(|(path, _)| path.as_str())
			.collect::<Vec<_>>(),
		["locked.txt"],
		"only the failure that is the path's own counts towards a streak"
	);
	for reason in [HaltReason::LocalStorageFull, HaltReason::RemoteStorageFull] {
		assert_eq!(
			report
				.errors
				.iter()
				.filter(|line| **line == reason.held_line())
				.count(),
			1,
			"one line per full side: {:?}",
			report.errors
		);
	}
	assert_eq!(report.errors.len(), 6, "{:?}", report.errors);
	assert_eq!(report.interrupted, 0);
}

/// A move the server refuses because another device has since uploaded a newer version of the
/// file is not reported as a failure, but its path is still owed to the next pass; any other
/// refusal of the same move is reported.
#[test]
fn a_move_of_a_file_replaced_since_the_pass_read_it_is_left_to_the_next_pass() {
	let action = SyncAction::MoveRemote {
		from_path: "e.txt".to_string(),
		to_path: "sub/e.txt".to_string(),
		kind: NodeKind::File,
		remote_uuid: Uuid::from_u128(1),
	};
	let refused = |code: &str| -> crate::Error {
		filen_types::error::ResponseError::ApiError {
			message: None,
			code: Some(code.to_string()),
		}
		.into()
	};
	let full = FullSides::default();
	let mut events = Vec::new();

	let mut report = SyncReport::default();
	note_action_failed(
		&action,
		&refused("file_is_versioned"),
		&full,
		&mut report,
		&mut |event| events.push(event),
	);
	assert!(report.errors.is_empty(), "{:?}", report.errors);
	assert!(events.is_empty());
	assert_eq!(
		report
			.failed_paths
			.iter()
			.map(|(path, _)| path.as_str())
			.collect::<Vec<_>>(),
		["sub/e.txt"]
	);
	assert_eq!(report.halted, None);

	let mut report = SyncReport::default();
	note_action_failed(
		&action,
		&refused("file_not_found"),
		&full,
		&mut report,
		&mut |event| events.push(event),
	);
	assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
	assert_eq!(report.failed_paths.len(), 1);
	assert_eq!(events.len(), 1);
}

/// A full side holds back only the transfers that write to it: a full account stops uploads and
/// lets downloads (and every non-transfer action) through, a full disk the reverse.
#[test]
fn a_full_side_holds_only_the_transfers_that_write_to_it() {
	let upload = SyncAction::UploadFile {
		rel_path: "up.bin".to_string(),
	};
	let download = SyncAction::DownloadFile {
		rel_path: "down.bin".to_string(),
		remote_uuid: Uuid::new_v4(),
	};
	let trash = SyncAction::TrashRemote {
		rel_path: "gone.bin".to_string(),
		kind: NodeKind::File,
		remote_uuid: Uuid::new_v4(),
	};
	let actions = [&upload, &download, &trash];
	let held = |full: &FullSides| actions.map(|action| full.holds(action));

	let full = FullSides::default();
	assert_eq!(held(&full), [false, false, false]);
	full.note(HaltReason::RemoteStorageFull);
	assert_eq!(held(&full), [true, false, false]);
	let full = FullSides::default();
	full.note(HaltReason::LocalStorageFull);
	assert_eq!(held(&full), [false, true, false]);
	full.note(HaltReason::RemoteStorageFull);
	assert_eq!(held(&full), [true, true, false]);
}

/// The store's own answer to a write another connection will not let go of: the error a record
/// write really fails with, mapped exactly as an action's path maps it.
fn contended_write_error() -> crate::Error {
	let path = std::env::temp_dir().join(format!("filen_apply_contended_{}.db", Uuid::new_v4()));
	let holder = BaselineStore::open(&path).unwrap();
	let (pair, _) = holder
		.create_pair("/root", Uuid::new_v4(), super::super::SyncMode::TwoWay)
		.unwrap();
	let writer = BaselineStore::open(&path).unwrap();
	writer.busy_timeout_for_test(Duration::from_millis(50));
	let entry = dir_entry("held", Some(Uuid::new_v4()), None);
	let error = {
		let _held = holder.hold_write_lock(pair, &entry).unwrap();
		writer
			.upsert_entry(pair, &entry)
			.map_err(db_err)
			.expect_err("a write contended past the busy timeout must fail rather than wait")
	};
	drop(writer);
	drop(holder);
	for suffix in ["", "-wal", "-shm"] {
		std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
	}
	error
}

fn conflict(rel_path: &str) -> PlannedConflict {
	PlannedConflict {
		rel_path: rel_path.to_string(),
		local: Some(PlannedNodeKind::File),
		remote: Some(PlannedNodeKind::File),
	}
}

/// The row that holds a version an upload of ours buried is the only reference to that version
/// anywhere — no view can re-derive it. So when it cannot be written the conflict must still
/// reach the report, and the lost row must still make the pass a failed one.
#[test]
fn a_buried_version_whose_row_could_not_be_written_is_still_reported() {
	let mut report = SyncReport::default();
	note_overwrote(
		&mut report,
		"a.txt",
		conflict("a.txt"),
		Some(contended_write_error()),
	);
	assert_eq!(report.uploaded, 1, "the upload landed and still counts");
	assert_eq!(
		report
			.conflicts
			.iter()
			.map(|conflict| conflict.rel_path.as_str())
			.collect::<Vec<_>>(),
		["a.txt"],
		"the buried version must be surfaced even though its row was lost: {:?}",
		report.errors
	);
	assert_eq!(
		report.store_failed,
		Some(StoreFailure::RecordNotWritten(ErrorKind::Internal)),
		"and the lost row must still fail the pass: {:?}",
		report.errors
	);
	assert!(
		report.errors.iter().any(|line| line.starts_with("a.txt: ")),
		"naming the path whose row was lost: {:?}",
		report.errors
	);

	let mut recorded = SyncReport::default();
	note_overwrote(&mut recorded, "b.txt", conflict("b.txt"), None);
	assert_eq!(recorded.conflicts.len(), 1);
	assert!(
		recorded.store_failed.is_none() && recorded.errors.is_empty(),
		"a row that landed is no failure: {:?}",
		recorded.errors
	);
}

/// Once a record of work already done could not be written, the pass admits nothing more: every
/// action it still holds would carry out its act and then wait out another busy timeout to lose
/// its own record. What it does not get to is owed to the next pass instead.
#[test]
fn a_lost_record_stops_the_pass_admitting_more_work() {
	let mut report = SyncReport::default();
	assert!(admits_more(&report), "a healthy pass admits its actions");
	note_failure(
		&mut report,
		"docs/a.txt",
		&internal("the server refused this one"),
	);
	assert!(
		admits_more(&report),
		"an action that failed on its own leaves the pass running: {:?}",
		report.errors
	);
	note_failure(&mut report, "docs/b.txt", &contended_write_error());
	assert!(
		!admits_more(&report),
		"a record that did not land stops the pass: {:?}",
		report.errors
	);
	note_interrupted(&mut report, 7, &mut |_| {});
	assert_eq!(
		report.interrupted, 7,
		"what it did not get to is owed to the next pass"
	);
}

/// A plan the pass could not read stops it as a lost record does, but for a reason of its own;
/// a lost record in the same pass, before or after, is what it reports.
#[test]
fn an_unread_plan_stops_the_pass_for_a_reason_a_lost_record_outranks() {
	let unread = internal("reading a page of the pass's staged plan");
	let mut report = SyncReport::default();
	note_unread_plan(&mut report, &unread);
	assert_eq!(
		report.store_failed,
		Some(StoreFailure::UnreadPlan(ErrorKind::Internal))
	);
	assert!(!admits_more(&report));
	note_failure(&mut report, "docs/a.txt", &contended_write_error());
	assert_eq!(
		report.store_failed,
		Some(StoreFailure::RecordNotWritten(ErrorKind::Internal))
	);
	note_unread_plan(&mut report, &unread);
	assert_eq!(
		report.store_failed,
		Some(StoreFailure::RecordNotWritten(ErrorKind::Internal))
	);
}

/// The baseline write of an applied action records something that has already happened, so when
/// it cannot land the pass is a failed one: the store's answer reaches the report under the path,
/// and `store_failed` is what tells a watch loop to back off instead of reading the pass as
/// healthy. Both halves here are the production path — the store's own contended answer, mapped
/// by `db_err` and recorded by `note_failure`.
#[test]
fn a_baseline_row_that_could_not_be_written_makes_the_pass_a_failed_one() {
	let path = std::env::temp_dir().join(format!("filen_apply_busy_{}.db", Uuid::new_v4()));
	let holder = BaselineStore::open(&path).unwrap();
	let (pair, _) = holder
		.create_pair("/root", Uuid::new_v4(), super::super::SyncMode::TwoWay)
		.unwrap();
	let writer = BaselineStore::open(&path).unwrap();
	writer.busy_timeout_for_test(Duration::from_millis(50));
	let entry = dir_entry("docs", Some(Uuid::new_v4()), None);

	let mut report = SyncReport::default();
	{
		let _held = holder.hold_write_lock(pair, &entry).unwrap();
		let error = writer
			.upsert_entry(pair, &entry)
			.map_err(db_err)
			.expect_err("the contended row write must fail rather than wait");
		assert!(record_not_written(&error), "{error}");
		note_failure(&mut report, "docs", &error);
	}
	assert_eq!(
		report.store_failed,
		Some(StoreFailure::RecordNotWritten(ErrorKind::Internal)),
		"a lost record must mark the pass failed: {:?}",
		report.errors
	);
	assert!(
		report.errors.iter().any(|line| line.starts_with("docs: ")),
		"the report must name the path whose record was lost: {:?}",
		report.errors
	);
	assert_eq!(
		report.failed_paths.len(),
		1,
		"and the path's own streak still counts it"
	);

	drop(writer);
	drop(holder);
	for suffix in ["", "-wal", "-shm"] {
		std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
	}
}

fn temp_dir() -> PathBuf {
	let dir = std::env::temp_dir().join(format!("filen_confine_test_{}", Uuid::new_v4()));
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

/// `actions` staged as they stand, with nothing read for any of them, for the apply to page
/// through; the sync root is `u128::MAX`.
fn paged(actions: impl IntoIterator<Item = SyncAction>) -> (tempfile::TempDir, PagedPlan) {
	let (dir, stage) = Stage::in_temp_dir();
	let mut writer = PlanWriter::new(stage).unwrap();
	for action in actions {
		writer.push(&PlannedEntry::bare(action)).unwrap();
	}
	let plan = PagedPlan::new(writer.finish().unwrap(), Uuid::from_u128(u128::MAX));
	(dir, plan)
}

fn delete_local(rel_path: &str, kind: NodeKind) -> SyncAction {
	SyncAction::DeleteLocal {
		rel_path: rel_path.to_string(),
		kind,
	}
}

/// The shape a case-only directory rename planned per path takes on the remote: the create at
/// the new spelling is answered by the dedup with the EXISTING directory's uuid, and the trash
/// at the old spelling then names that same uuid. The trash must be refused.
#[test]
fn a_trash_of_a_directory_this_pass_created_is_refused() {
	let existing = Uuid::from_u128(1);
	let create = SyncAction::CreateRemoteDir {
		rel_path: "docs".to_string(),
	};
	let (_dir, mut plan) = paged([create.clone(), trash("Docs", NodeKind::Dir)]);
	plan.record_created("docs", existing).unwrap();
	plan.pager.note_written(&create).unwrap();
	assert!(
		plan.pager.shields_remote("Other", existing),
		"the directory the dedup answered with, under any name"
	);
	assert!(
		plan.pager.shields_remote("Docs", Uuid::from_u128(2)),
		"same name under another spelling"
	);
	assert!(
		!plan.pager.shields_remote("Other", Uuid::from_u128(2)),
		"an unrelated trash is not held"
	);
}

/// A trash or quarantine of a directory that now holds something this pass wrote — a child
/// moved or downloaded into it under another spelling — would take that write down with it, and
/// a trash of the item a move moved would take down the item itself.
#[test]
fn a_delete_above_a_write_of_this_pass_is_refused() {
	let moved = Uuid::from_u128(1);
	let writes = [
		SyncAction::MoveRemote {
			from_path: "Docs/a.txt".to_string(),
			to_path: "docs/a.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: moved,
		},
		SyncAction::DownloadFile {
			rel_path: "docs/b.txt".to_string(),
			remote_uuid: Uuid::from_u128(2),
		},
	];
	let (_dir, plan) = paged(
		writes
			.iter()
			.cloned()
			.chain([delete_local("Docs", NodeKind::Dir)]),
	);
	for write in &writes {
		plan.pager.note_written(write).unwrap();
	}
	assert!(plan.pager.shields_remote("Docs", Uuid::from_u128(3)));
	assert!(
		plan.pager.shields_remote("elsewhere.txt", moved),
		"the moved item"
	);
	assert!(plan.pager.shields_local("Docs"));
	assert!(
		!plan.pager.shields_local("Docs/c.txt"),
		"a sibling of the write is not held"
	);
	assert!(
		!plan.pager.shields_local("doc"),
		"a name sharing only a prefix is not held"
	);
	assert!(
		!plan.pager.shields_remote("docs/b.txt", Uuid::from_u128(3)),
		"a local write does not hold a remote trash"
	);
}

#[test]
fn a_key_sorting_between_a_dir_and_its_children_does_not_hide_them() {
	// `docs-x` sorts after `docs` but before `docs/`, so a range read has to start at the
	// child prefix rather than at the key itself.
	let shields_docs = |written: &[&str]| {
		let downloads: Vec<SyncAction> = written.iter().map(|path| download(path)).collect();
		let (_dir, plan) = paged(
			downloads
				.iter()
				.cloned()
				.chain([delete_local("Docs", NodeKind::Dir)]),
		);
		for download in &downloads {
			plan.pager.note_written(download).unwrap();
		}
		plan.pager.shields_local("Docs")
	};
	assert!(shields_docs(&["docs-x", "docs/a.txt"]));
	assert!(!shields_docs(&["docs-x"]));
}

/// A write the stage cannot record is left unshielded, so the pass admits nothing more after
/// it. The write itself landed: its path keeps no failure streak for it.
#[test]
fn a_write_the_stage_cannot_record_fails_the_pass_but_not_its_path() {
	let (_dir, plan) = paged([upload("up.txt"), trash("Up.txt", NodeKind::File)]);
	plan.pager.break_stage();
	let mut report = SyncReport::default();
	note_written(&plan, &upload("up.txt"), &mut report);
	assert_eq!(
		report.store_failed,
		Some(StoreFailure::RecordNotWritten(ErrorKind::Internal))
	);
	assert!(!admits_more(&report));
	assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
	assert!(
		report.errors[0].starts_with("up.txt: ")
			&& report.errors[0].contains("recording what this pass wrote in its stage"),
		"{}",
		report.errors[0]
	);
	assert!(report.failed_paths.is_empty());
}

/// A pass that deletes nothing has nothing a write of its own needs shielding from: a first push
/// of 10,000 files keeps none of them, in memory or in its stage.
#[test]
fn a_plan_that_deletes_nothing_keeps_nothing_of_its_writes() {
	let uploads: Vec<SyncAction> = (0..10_000)
		.map(|n| upload(&format!("d{:02}/f{n:05}.txt", n % 100)))
		.collect();
	let (_dir, plan) = paged(uploads.iter().cloned());
	let ((), peak) = alloc_meter::peak_bytes(|| {
		for action in &uploads {
			plan.pager.note_written(action).unwrap();
		}
	});
	assert_eq!(peak, 0, "10_000 uploads landed and nothing is deleted");
	assert!(
		!plan.pager.shields_remote("d00", Uuid::from_u128(1)),
		"the stage recorded the uploads"
	);
}

/// A plan of `entry` alone, a directory deletion, staged with the deletions it carries.
fn carrying(entry: &PlannedEntry, carried: &[String]) -> (tempfile::TempDir, PagedPlan) {
	let (dir, stage) = Stage::in_temp_dir();
	let mut writer = PlanWriter::new(stage).unwrap();
	writer.push(entry).unwrap();
	writer.carry(&entry.action, carried).unwrap();
	let plan = PagedPlan::new(writer.finish().unwrap(), Uuid::from_u128(u128::MAX));
	(dir, plan)
}

/// A store holding one pair, with a row at each of `paths`.
fn store_with<'p>(paths: impl IntoIterator<Item = &'p str>) -> (BaselineStore, PairId) {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/local", Uuid::from_u128(1), super::super::SyncMode::TwoWay)
		.unwrap();
	upsert_files(&store, pair, paths);
	(store, pair)
}

/// Write a file row at each of `paths` into `pair`.
fn upsert_files<'p>(store: &BaselineStore, pair: PairId, paths: impl IntoIterator<Item = &'p str>) {
	let rows: Vec<BaselineEntry> = paths
		.into_iter()
		.map(|path| baseline_file(path, None))
		.collect();
	let upserts: Vec<BaselineChange<'_>> = rows.iter().map(BaselineChange::Upsert).collect();
	store.apply_changes(pair, &upserts).unwrap();
}

fn rows_of(store: &BaselineStore, pair: PairId) -> Vec<String> {
	store
		.entries(pair)
		.unwrap()
		.into_iter()
		.map(|entry| entry.rel_path)
		.collect()
}

/// A directory deletion's commit drops its own row and the rows of exactly the deletions it
/// carried, read out of the stage: a row under the directory that no deletion of the plan names
/// stays. A local deletion's commit and a remote trash's alike, the trash's journal row with it.
#[test]
fn a_directory_deletion_drops_exactly_the_rows_it_carried() {
	let carried = ["docs/a.txt".to_string(), "docs/sub/b.txt".to_string()];
	let paths = [
		"docs",
		"docs/a.txt",
		"docs/kept.txt",
		"docs/sub/b.txt",
		"other.txt",
	];

	let deleted = delete_local("docs", NodeKind::Dir);
	let (store, pair) = store_with(paths);
	let (_dir, plan) = carrying(&PlannedEntry::bare(deleted.clone()), &carried);
	let rows = plan.pager.carried(&deleted).unwrap();
	store
		.apply_changes_carrying(pair, &[BaselineChange::Delete("docs")], |drop| {
			rows.each(drop)
		})
		.unwrap();
	assert_eq!(rows_of(&store, pair), ["docs/kept.txt", "other.txt"]);

	let trashed = trash("docs", NodeKind::Dir);
	let (store, pair) = store_with(paths);
	let (_dir, plan) = carrying(&PlannedEntry::bare(trashed.clone()), &carried);
	let rows = plan.pager.carried(&trashed).unwrap();
	let journal = PendingRow {
		pair,
		uuid: Uuid::from_u128(2),
		kind: PendingKind::Trashed {
			path: "docs".to_string(),
		},
		recorded_at: 1,
	};
	store
		.record_pending_carrying(&journal, &[BaselineChange::Delete("docs")], &[], |drop| {
			rows.each(drop)
		})
		.unwrap();
	assert_eq!(rows_of(&store, pair), ["docs/kept.txt", "other.txt"]);
	assert_eq!(store.load_pending(1, 1_000).unwrap(), [journal]);
}

/// A directory deletion that carries 20,000 others streams their rows from the stage into its
/// commit: what it holds at once is a path, not the subtree's paths.
#[test]
fn a_directory_deletion_streams_the_rows_it_carried() {
	const CARRIED: u32 = 20_000;
	let carried: Vec<String> = (0..CARRIED)
		.map(|n| format!("docs/d{:02}/f{n:05}.txt", n % 100))
		.collect();
	let (store, pair) = store_with(
		["docs"]
			.into_iter()
			.chain(carried.iter().map(String::as_str)),
	);
	let deleted = delete_local("docs", NodeKind::Dir);
	let (_dir, plan) = carrying(&PlannedEntry::bare(deleted.clone()), &carried);
	let rows = plan.pager.carried(&deleted).unwrap();
	let ((), peak) = alloc_meter::peak_bytes(|| {
		store
			.apply_changes_carrying(pair, &[BaselineChange::Delete("docs")], |drop| {
				rows.each(drop)
			})
			.unwrap();
	});
	assert_eq!(rows_of(&store, pair), Vec::<String>::new());
	assert!(
		peak < 64 * 1024,
		"dropping the carried rows held {peak} bytes at once"
	);
}

/// A move nested inside another commits the rows of its own step only: after the outer move the
/// inner directory's rows sit where the disk has them until the inner move runs, nothing stays
/// behind under the outer source, and an inner move that never runs leaves the store agreeing
/// with the disk.
#[test]
fn nested_dir_moves_commit_each_step_from_the_store() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/local", Uuid::new_v4(), super::super::SyncMode::TwoWay)
		.unwrap();
	for path in ["D", "D/in", "D/in/x"] {
		store
			.upsert_entry(pair, &dir_entry(path, Some(Uuid::new_v4()), None))
			.unwrap();
	}
	let paths = |store: &BaselineStore| -> Vec<String> {
		store
			.entries(pair)
			.unwrap()
			.into_iter()
			.map(|entry| entry.rel_path)
			.collect()
	};
	let step = |from, to| {
		store
			.apply_changes(pair, &[BaselineChange::MoveSubtree { from, to }])
			.unwrap();
	};

	step("D", "E");
	assert_eq!(
		paths(&store),
		["E", "E/in", "E/in/x"],
		"the outer move committed more than its own step"
	);
	step("E/in", "E/in2");
	assert_eq!(paths(&store), ["E", "E/in2", "E/in2/x"]);
}

#[test]
fn confine_allows_paths_under_the_root() {
	let root = temp_dir();
	let target = confined_local_target(&root, "sub/file.txt").expect("in-root path allowed");
	assert!(target.starts_with(&root));
	std::fs::remove_dir_all(&root).ok();
}

#[test]
fn confine_rejects_lexical_traversal() {
	let root = temp_dir();
	assert!(
		confined_local_target(&root, "../escape.txt").is_err(),
		".. component must be refused"
	);
	assert!(confined_local_target(&root, "a/../../b").is_err());
	std::fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
#[test]
fn confine_rejects_writing_through_a_symlink_escaping_the_root() {
	let root = temp_dir();
	let outside = temp_dir(); // a sibling dir, NOT under root
	// `root/link` -> `outside`. Writing `root/link/x` would land in `outside` without confinement.
	std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

	let result = confined_local_target(&root, "link/x.txt");
	assert!(
		result.is_err(),
		"writing through a symlink that escapes the root must be refused, got {result:?}"
	);

	std::fs::remove_dir_all(&root).ok();
	std::fs::remove_dir_all(&outside).ok();
}

#[cfg(unix)]
#[test]
fn confine_allows_a_symlink_that_stays_within_the_root() {
	let root = temp_dir();
	std::fs::create_dir_all(root.join("real")).unwrap();
	std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
	// `root/link` -> `root/real` (inside) — writing through it stays confined.
	assert!(confined_local_target(&root, "link/x.txt").is_ok());
	std::fs::remove_dir_all(&root).ok();
}

#[test]
fn quarantine_makes_a_unique_dest_on_collision_never_overwriting() {
	let root = temp_dir();
	let trash = root.join(QUARANTINE_DIR);

	std::fs::write(root.join("x.txt"), b"first").unwrap();
	assert_eq!(
		quarantine_local(&root, "x.txt").unwrap(),
		Some(trash.join("x.txt"))
	);
	assert_eq!(std::fs::read(trash.join("x.txt")).unwrap(), b"first");

	// A second deletion at the same rel path must NOT clobber the first quarantined copy.
	std::fs::write(root.join("x.txt"), b"second").unwrap();
	assert_eq!(
		quarantine_local(&root, "x.txt").unwrap(),
		Some(trash.join("x.txt (1)"))
	);
	assert_eq!(
		std::fs::read(trash.join("x.txt")).unwrap(),
		b"first",
		"first copy preserved"
	);
	assert_eq!(
		std::fs::read(trash.join("x.txt (1)")).unwrap(),
		b"second",
		"second copy under a unique name"
	);
	std::fs::remove_dir_all(&root).ok();
}

fn local_node(rel: &str, mtime: i64, hash: Option<Blake3Hash>) -> LocalNode {
	LocalNode {
		rel_path: rel.to_string(),
		kind: NodeKind::File,
		size: 3,
		mtime_millis: mtime,
		content_hash: hash,
	}
}

fn remote_node(rel: &str, hash: Option<Blake3Hash>) -> RemoteNode {
	let uuid = Uuid::new_v4();
	RemoteNode {
		rel_path: rel.to_string(),
		kind: NodeKind::File,
		remote_uuid: uuid,
		stable_uuid: Some(StableUuid::new_for_test(uuid)),
		content_hash: hash,
		size: 3,
		modified_millis: 900,
	}
}

fn baseline_file(rel: &str, hash: Option<Blake3Hash>) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel.to_string(),
		kind: NodeKind::File,
		remote_uuid: Some(Uuid::new_v4()),
		content_hash: hash,
		size: Some(3),
		local_mtime: Some(10),
		remote_modified: Some(10),
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: None,
		agreed_hash: None,
	}
}

/// A file on disk plus a baseline row that vouches for exactly it — its content hash included,
/// which is what a pull-over decision compares once the stat alone says "unchanged".
fn synced_file(root: &Path, bytes: &[u8]) -> (PathBuf, BaselineEntry) {
	let path = root.join("a.txt");
	std::fs::write(&path, bytes).unwrap();
	let mut base = baseline_file("a.txt", Some(blake3::hash(bytes).into()));
	base.size = Some(bytes.len() as u64);
	base.local_mtime = local_mtime_of(&path);
	(path, base)
}

#[test]
fn a_pull_over_an_unmodified_local_file_does_not_quarantine() {
	let root = temp_dir();
	let (path, base) = synced_file(&root, b"abc");
	assert!(!local_holds_unsynced_content(&path, Some(&base)));
	std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_pull_over_a_file_edited_after_the_scan_quarantines_first() {
	// The scan snapshot still says this file matches its baseline; the file on disk no longer
	// does. A pass waits on the drive lock between the scan and the download, so the decision
	// has to be read from disk.
	let root = temp_dir();
	let (path, base) = synced_file(&root, b"abc");
	std::fs::write(&path, b"edited after the scan").unwrap();
	assert!(
		local_holds_unsynced_content(&path, Some(&base)),
		"a file that no longer matches its baseline row must be stashed"
	);
	assert!(
		local_holds_unsynced_content(&path, None),
		"a file the baseline never recorded must be stashed"
	);
	std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_pull_over_a_local_dir_quarantines_first() {
	let root = temp_dir();
	let path = root.join("a.txt");
	std::fs::create_dir(&path).unwrap();
	let base = baseline_file("a.txt", Some(Blake3Hash::from([1; 32])));
	assert!(
		local_holds_unsynced_content(&path, Some(&base)),
		"a directory where a file is being pulled must be stashed, not written over"
	);
	std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_pull_with_nothing_local_does_not_quarantine() {
	let root = temp_dir();
	assert!(!local_holds_unsynced_content(
		&root.join("missing.txt"),
		None
	));
	std::fs::remove_dir_all(&root).ok();
}

#[test]
fn adopt_records_the_scan_time_mtime_not_a_fresh_stat() {
	// The scan hashed the file at mtime 4242. Pairing that hash with any LATER mtime would
	// make a same-size edit made after the scan invisible to the fast-path forever.
	let hash = Blake3Hash::from([7; 32]);
	let local = local_node("a.txt", 4242, Some(hash));
	let remote = remote_node("a.txt", Some(hash));
	let AdoptOutcome::Record(entry) = adopt_outcome("a.txt", Some(&local), Some(&remote), true)
	else {
		panic!("both sides present must record a row");
	};
	assert_eq!(entry.local_mtime, Some(4242), "scan-time mtime recorded");
	assert_eq!(entry.content_hash, Some(hash));
	assert_eq!(entry.remote_uuid, Some(remote.remote_uuid));
	assert_eq!(entry.remote_modified, Some(900));
	// Adopting a converged path is the observation that BOTH sides hold this content, so it is
	// also what records the agreed content the concurrent-edit rule reads.
	assert_eq!(entry.agreed_hash, entry.content_hash);
	assert!(entry.agreed_hash.is_some());
}

/// A directory has no content, so nothing for the two sides to agree on.
#[test]
fn adopting_a_directory_records_no_agreed_content() {
	let local = local_node("d", 1, None);
	let mut remote = remote_node("d", None);
	remote.kind = NodeKind::Dir;
	match adopt_outcome("d", Some(&local), Some(&remote), true) {
		AdoptOutcome::Record(entry) => assert_eq!(entry.agreed_hash, None),
		other => panic!("expected a recorded row, got {other:?}"),
	}
}

#[test]
fn adopt_of_a_convergent_delete_drops_the_row() {
	assert_eq!(
		adopt_outcome("gone.txt", None, None, true),
		AdoptOutcome::DropRow
	);
}

#[test]
fn adopt_keeps_the_row_when_the_absence_evidence_is_untrusted() {
	// Incomplete scan / un-converged or empty remote view: both sides only LOOK gone. Dropping
	// the row here makes the next healthy pass see the surviving side as Created and resurrect
	// the item instead of propagating the deletion.
	assert_eq!(
		adopt_outcome("gone.txt", None, None, false),
		AdoptOutcome::Keep
	);
}

/// A local move whose destination is occupied by a file the baseline cannot vouch for must
/// stash that file first: `rename` would overwrite it and lose it for good.
#[tokio::test]
async fn a_local_move_onto_an_unsynced_file_stashes_it_first() {
	let root = temp_dir();
	std::fs::write(root.join("dest.txt"), b"never synced").unwrap();

	let _stashed = stash_local_target(&root, "dest.txt", None).await.unwrap();

	assert!(
		!root.join("dest.txt").exists(),
		"the destination is cleared for the rename"
	);
	assert_eq!(
		std::fs::read(root.join(QUARANTINE_DIR).join("dest.txt")).unwrap(),
		b"never synced",
		"and the file the baseline could not vouch for is recoverable"
	);

	std::fs::remove_dir_all(&root).ok();
}

/// Clear a move's destination exactly as the `MoveLocal` arm does, and collect what the pass
/// said while doing it.
async fn cleared_move_target(
	root: &Path,
	to_path: &str,
	base: Option<&BaselineEntry>,
) -> (SyncReport, Vec<SyncEvent>) {
	let mut report = SyncReport::default();
	let mut events = Vec::new();
	stash_move_target(root, to_path, base, &mut report, &mut |event| {
		events.push(event)
	})
	.await
	.expect("clearing a move's destination");
	(report, events)
}

/// A case-only rename resolves both ends of the move to one file on a case-insensitive
/// filesystem: stashing "the destination" would carry the source away and leave the rename
/// with nothing to rename.
#[tokio::test]
async fn a_case_only_rename_is_not_treated_as_an_occupied_destination() {
	let root = temp_dir();
	std::fs::write(root.join("report.txt"), b"the source").unwrap();

	let (report, events) = cleared_move_target(&root, "REPORT.TXT", None).await;
	assert!(
		report.quarantined.is_empty() && events.is_empty(),
		"nothing of its own sits at the destination, so nothing is moved and nothing is said: {report:?} {events:?}"
	);

	assert_eq!(
		std::fs::read(root.join("report.txt")).unwrap(),
		b"the source",
		"the file the rename is about to move is left where it is"
	);
	assert!(
		!root.join(QUARANTINE_DIR).exists(),
		"nothing is quarantined"
	);
	std::fs::remove_dir_all(&root).ok();
}

/// Where the filesystem DOES keep the two spellings apart, the destination is somebody else's
/// file and the rename would destroy it: case-folding the two paths would have waved it
/// through as a self-move.
#[tokio::test]
async fn a_destination_that_only_looks_like_the_source_is_still_stashed() {
	let root = temp_dir();
	std::fs::write(root.join("REPORT.TXT"), b"never synced").unwrap();

	let (report, events) = cleared_move_target(&root, "REPORT.TXT", None).await;
	assert_eq!(
		report.quarantined,
		vec!["REPORT.TXT".to_string()],
		"somebody else's file was moved aside, and the pass has to say so"
	);
	assert_eq!(
		events,
		vec![SyncEvent::Quarantined {
			rel_path: "REPORT.TXT".to_string(),
			bin_path: root.join(QUARANTINE_DIR).join("REPORT.TXT"),
		}]
	);

	assert_eq!(
		std::fs::read(root.join(QUARANTINE_DIR).join("REPORT.TXT")).unwrap(),
		b"never synced"
	);
	std::fs::remove_dir_all(&root).ok();
}

/// The other side of it: a destination holding exactly what the baseline records is just an
/// unmodified copy, and moving over it stashes nothing.
#[tokio::test]
async fn a_local_move_onto_a_synced_copy_stashes_nothing() {
	let root = temp_dir();
	let (path, base) = synced_file(&root, b"abc");

	let stashed = stash_local_target(&root, "a.txt", Some(&base))
		.await
		.unwrap();
	assert_eq!(stashed, None, "nothing was stashed, so nothing to put back");

	assert!(path.exists(), "an unmodified copy is left where it is");
	assert!(
		!root.join(QUARANTINE_DIR).exists(),
		"nothing is quarantined"
	);

	std::fs::remove_dir_all(&root).ok();
}

/// The edit no stat can see: same length, with the mtime put back (`touch -r`, an `rsync -a`
/// restore, an editor that restores the timestamp). The scan's fast-path reads such a file as
/// unchanged for ever, so no plan ever names that path and nothing else re-reads its bytes —
/// this decision has to hash them, or the next remote change is pulled straight over the edit.
#[tokio::test]
async fn a_pull_over_a_size_and_mtime_preserving_edit_quarantines_first() {
	let root = temp_dir();
	let (path, base) = synced_file(&root, b"abc");
	let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

	std::fs::write(&path, b"XYZ").unwrap();
	std::fs::File::options()
		.write(true)
		.open(&path)
		.unwrap()
		.set_times(std::fs::FileTimes::new().set_modified(mtime))
		.unwrap();
	assert_eq!(
		(base.size, base.local_mtime),
		(Some(3), local_mtime_of(&path)),
		"the stat has to be indistinguishable from the synced copy, or this proves nothing"
	);

	assert!(
		local_holds_unsynced_content(&path, Some(&base)),
		"only the bytes tell this edit from the copy the row vouches for"
	);
	let stashed = stash_local_target(&root, "a.txt", Some(&base))
		.await
		.unwrap()
		.expect("an edit no stat can see must still be stashed");
	assert_eq!(
		std::fs::read(&stashed).unwrap(),
		b"XYZ",
		"the edit stays recoverable instead of going under the download"
	);
	assert!(
		!path.exists(),
		"the edit is moved out of the download's way"
	);

	std::fs::remove_dir_all(&root).ok();
}

/// A row recording no content hash vouches for nothing: the stat it does record is exactly what
/// a size- and mtime-preserving edit preserves, so there is no evidence the copy on disk is the
/// one that was synced. It is quarantined rather than written over.
#[tokio::test]
async fn a_row_without_a_recorded_hash_is_not_vouched_for() {
	let root = temp_dir();
	let (path, mut base) = synced_file(&root, b"abc");
	base.content_hash = None;

	assert!(local_holds_unsynced_content(&path, Some(&base)));
	let stashed = stash_local_target(&root, "a.txt", Some(&base))
		.await
		.unwrap()
		.expect("a copy no row vouches for must be kept");
	assert_eq!(std::fs::read(&stashed).unwrap(), b"abc");
	assert!(!path.exists(), "it moved out of the download's way");

	std::fs::remove_dir_all(&root).ok();
}

/// A `KeepLocal` resolution clears the row's local half on purpose and leaves it that way until
/// the re-push. A move must carry that emptiness across: filling it in from the DESTINATION's
/// remote state would record bytes nobody ever compared, and where the kept copy happens to be
/// the same length as the remote's the scanner's `(size, mtime)` fast path would vouch for them
/// forever — the push the row is waiting for never happens, and the next foreign edit overwrites
/// the kept copy without even a quarantine.
#[test]
fn moving_a_row_with_no_local_content_records_none_at_the_new_path() {
	let kept = BaselineEntry {
		content_hash: None,
		size: None,
		local_mtime: None,
		..baseline_file("a.txt", None)
	};
	let remote = remote_node("b.txt", Some(Blake3Hash::from([9; 32])));

	let row = moved_file_row("b.txt", Some(&kept), Some(&remote), Some(4242));
	assert_eq!(
		row.content_hash, None,
		"the destination's remote hash is not a stand-in for local content nobody recorded"
	);
	assert_eq!(
		row.local_mtime, None,
		"nor is there an mtime to fast-path on"
	);
	assert_ne!(
		row.size,
		Some(remote.size),
		"nor may the remote's size stand in for it"
	);
	// The remote anchor still follows the moved row: that half of it IS on record.
	assert_eq!(row.remote_uuid, kept.remote_uuid);

	// The negative: an ordinary move carries the local half it does have across untouched.
	let synced = baseline_file("a.txt", Some(Blake3Hash::from([1; 32])));
	let row = moved_file_row("b.txt", Some(&synced), Some(&remote), Some(4242));
	assert_eq!(row.content_hash, synced.content_hash);
	assert_eq!(row.size, synced.size);
	assert_eq!(row.local_mtime, Some(4242), "the renamed file's own mtime");
}

#[test]
fn quarantine_of_a_missing_source_is_a_noop() {
	let root = temp_dir();
	// e.g. the item already moved as part of an ancestor's quarantine.
	assert_eq!(
		quarantine_local(&root, "already/gone.txt").expect("missing source is a no-op"),
		None
	);
	std::fs::remove_dir_all(&root).ok();
}

/// A download the pause dropped (or one that failed) never writes at its target, so the copy
/// stashed out of its way goes BACK: left in the bin, the path is empty, and the next pass
/// reads a baseline row with nothing on disk as a local deletion — a conflict over a deletion
/// nobody made, for a file whose only copy is in the quarantine bin.
#[tokio::test]
async fn a_write_that_never_landed_puts_its_stashed_target_back() {
	let root = temp_dir();
	std::fs::write(root.join("a.txt"), b"a local edit").unwrap();

	let stashed = stash_local_target(&root, "a.txt", None).await.unwrap();
	assert!(
		stashed.is_some(),
		"a copy the baseline cannot vouch for must be stashed before the download"
	);
	assert!(!root.join("a.txt").exists());

	restore_stashed(stashed, &root.join("a.txt"));

	assert_eq!(
		std::fs::read(root.join("a.txt")).unwrap(),
		b"a local edit",
		"the interrupted download left the path empty"
	);
	assert!(
		!root.join(QUARANTINE_DIR).join("a.txt").exists(),
		"and nothing is left duplicated in the bin"
	);
	std::fs::remove_dir_all(&root).ok();
}

/// Unless something is back at the path: that is somebody's newer write, and the stashed copy
/// stays recoverable in the bin rather than being put on top of it.
#[tokio::test]
async fn a_restore_never_overwrites_what_took_the_path() {
	let root = temp_dir();
	std::fs::write(root.join("a.txt"), b"a local edit").unwrap();
	let stashed = stash_local_target(&root, "a.txt", None).await.unwrap();
	std::fs::write(root.join("a.txt"), b"written since").unwrap();

	restore_stashed(stashed, &root.join("a.txt"));

	assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"written since");
	assert_eq!(
		std::fs::read(root.join(QUARANTINE_DIR).join("a.txt")).unwrap(),
		b"a local edit",
		"the stashed copy must stay recoverable"
	);
	std::fs::remove_dir_all(&root).ok();
}

/// A copy in the bin nobody is told about is a file that vanished from the tree. Pinned on the
/// edit only a hash can see, which is the case where nothing else the pass says names that
/// path: the report lists it AND the caller hears it as it happens.
#[tokio::test]
async fn a_pull_over_an_invisible_edit_announces_the_copy_it_moved_aside() {
	let root = temp_dir();
	let (path, base) = synced_file(&root, b"abc");
	let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
	// Same length, mtime put back: indistinguishable from the synced copy by stat alone.
	std::fs::write(&path, b"XYZ").unwrap();
	std::fs::File::options()
		.write(true)
		.open(&path)
		.unwrap()
		.set_times(std::fs::FileTimes::new().set_modified(mtime))
		.unwrap();

	let bin = stash_local_target(&root, "a.txt", Some(&base))
		.await
		.unwrap()
		.expect("an edit no stat can see must still be stashed");

	let mut report = SyncReport::default();
	let mut events = Vec::new();
	note_quarantined(
		&mut report,
		&mut |event| events.push(event),
		Some(("a.txt".to_string(), bin.clone())),
	);

	assert_eq!(
		events,
		vec![SyncEvent::Quarantined {
			rel_path: "a.txt".to_string(),
			bin_path: bin.clone(),
		}],
		"the caller has to HEAR about the copy, not only find it in the final report"
	);
	assert_eq!(
		report.quarantined,
		vec!["a.txt".to_string()],
		"and the report still lists it"
	);
	assert_eq!(
		std::fs::read(&bin).unwrap(),
		b"XYZ",
		"the announced path must be where the edit actually went"
	);

	std::fs::remove_dir_all(&root).ok();
}

/// The other write that moves a copy aside — a move onto a destination something took since
/// the scan — has quarantined silently for longer, and clearing the destination is what says
/// so: the occupant is announced by the same call that moves it, with the path it went to.
#[tokio::test]
async fn a_move_onto_an_occupied_destination_announces_it_too() {
	let root = temp_dir();
	std::fs::write(root.join("dest.txt"), b"never synced").unwrap();

	let (report, events) = cleared_move_target(&root, "dest.txt", None).await;

	let bin = root.join(QUARANTINE_DIR).join("dest.txt");
	assert_eq!(
		events,
		vec![SyncEvent::Quarantined {
			rel_path: "dest.txt".to_string(),
			bin_path: bin.clone(),
		}],
		"the rename cleared the destination without a word"
	);
	assert_eq!(report.quarantined, vec!["dest.txt".to_string()]);
	assert!(
		!root.join("dest.txt").exists(),
		"the destination is cleared for the rename"
	);
	assert_eq!(
		std::fs::read(&bin).unwrap(),
		b"never synced",
		"the occupant is recoverable at the announced path"
	);

	std::fs::remove_dir_all(&root).ok();
}

/// A page wants the file each download, file trash and file move names, and the directory the
/// view listed at the path of each directory trash and move; nothing for what only lands in a
/// directory, nor for a directory action the view listed no directory for.
#[test]
fn a_page_wants_the_objects_its_actions_act_on() {
	let uuid = Uuid::from_u128;
	let at_dir = |remote_uuid| {
		Some(Box::new(RemoteNode {
			kind: NodeKind::Dir,
			remote_uuid,
			..remote_node("d", None)
		}))
	};
	let entry = |action, start| PlannedEntry { action, start };
	let page = [
		PlannedEntry::bare(SyncAction::DownloadFile {
			rel_path: "a".to_string(),
			remote_uuid: uuid(1),
		}),
		PlannedEntry::bare(SyncAction::TrashRemote {
			rel_path: "b".to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid(2),
		}),
		PlannedEntry::bare(SyncAction::MoveRemote {
			from_path: "c0".to_string(),
			to_path: "c".to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid(3),
		}),
		entry(
			SyncAction::TrashRemote {
				rel_path: "d".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: uuid(14),
			},
			PassStart::Remote {
				remote: at_dir(uuid(4)),
			},
		),
		entry(
			SyncAction::MoveRemote {
				from_path: "e0".to_string(),
				to_path: "e".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: uuid(5),
			},
			PassStart::MoveRemoteDir {
				remote: at_dir(uuid(5)),
				parent_uuid: None,
			},
		),
		PlannedEntry::bare(SyncAction::TrashRemote {
			rel_path: "f".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: uuid(6),
		}),
		entry(
			upload("g"),
			PassStart::Upload {
				local: None,
				remote: at_dir(uuid(7)),
				base: None,
				parent_uuid: None,
			},
		),
		entry(
			SyncAction::CreateRemoteDir {
				rel_path: "h".to_string(),
			},
			PassStart::CreateRemoteDir {
				local: None,
				remote: at_dir(uuid(8)),
				parent_uuid: None,
			},
		),
	];
	// A trash sends only its uuid: neither the file nor the directory one wants its object.
	assert_eq!(
		Wanted::of(page.iter()),
		Wanted {
			files: HashSet::from([uuid(1), uuid(3)]),
			dirs: HashSet::from([uuid(5)]),
		}
	);
}

/// What lands in a directory lands in the sync root at the top, in the directory this pass
/// created at its parent once that create has run, and otherwise in the one the view listed
/// there; with none of the three it has nowhere to go.
#[test]
fn an_item_lands_in_the_root_a_directory_this_pass_created_or_the_views() {
	let (root, made, listed) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
	let (_dir, stage) = Stage::in_temp_dir();
	let mut writer = PlanWriter::new(stage).unwrap();
	for action in [
		SyncAction::CreateRemoteDir {
			rel_path: "made".to_string(),
		},
		SyncAction::CreateRemoteDir {
			rel_path: "pending".to_string(),
		},
		upload("uploaded"),
	] {
		writer.push(&PlannedEntry::bare(action)).unwrap();
	}
	let mut plan = PagedPlan::new(writer.finish().unwrap(), root);
	plan.record_created("made", made).unwrap();
	// Only a directory create records what it made.
	plan.record_created("uploaded", Uuid::from_u128(4)).unwrap();
	let (viewed, unlisted) = (Some(listed), None);
	let landing = [
		plan.parent_of("top.txt", viewed).unwrap(),
		plan.parent_of("made/x.txt", viewed).unwrap(),
		plan.parent_of("made/x.txt", unlisted).unwrap(),
		plan.parent_of("pending/x.txt", viewed).unwrap(),
		plan.parent_of("pending/x.txt", unlisted).unwrap(),
		plan.parent_of("uploaded/x.txt", unlisted).unwrap(),
		plan.parent_of("made/deeper/x.txt", unlisted).unwrap(),
	];
	assert_eq!(
		landing,
		[
			Some(root),
			Some(made),
			Some(made),
			Some(listed),
			None,
			None,
			None
		]
	);
	let target = plan
		.transfer_target(&PlannedEntry::bare(upload("made/x.txt")))
		.unwrap();
	assert_eq!((target.file, target.parent), (None, Some(made)));
}

/// Hydrating a 20,000-download pull page by page holds the objects of a page at a time, not the
/// pull's: every download finds its file in the page it is in. The plan is the apply's own
/// [`PagedPlan`], turned and read through the methods the apply uses; only the cache read is
/// made here, directly, where the apply makes it on a blocking thread the meter cannot see. The
/// cache holds every file, so the server is never asked.
#[test]
fn hydrating_a_page_at_a_time_holds_a_page_of_the_objects() {
	const DOWNLOADS: u32 = 20_000;
	let cache_dir = tempfile::tempdir().unwrap();
	let cache = cache_dir.path().join("cache.db");
	let root = Uuid::from_u128(u128::MAX);
	let file_uuid = |n: u32| Uuid::from_u128(u128::from(n) + 1);
	let name = |n: u32| format!("f{n:05}.txt");
	let mut state = CacheState::new_on_path(&cache, root);
	state
		.upsert_files((0..DOWNLOADS).map(|n| cacheable_file(file_uuid(n), root, &name(n))))
		.unwrap();
	let (_dir, stage) = Stage::in_temp_dir();
	let mut writer = PlanWriter::new(stage).unwrap();
	for n in 0..DOWNLOADS {
		let remote = RemoteNode {
			remote_uuid: file_uuid(n),
			..remote_node(&name(n), None)
		};
		writer
			.push(&PlannedEntry {
				action: SyncAction::DownloadFile {
					rel_path: name(n),
					remote_uuid: file_uuid(n),
				},
				start: PassStart::Download {
					remote: Some(Box::new(remote)),
					base: None,
				},
			})
			.unwrap();
	}
	let staged = writer.finish().unwrap();
	let (found, peak) = alloc_meter::peak_bytes(|| {
		let mut plan = PagedPlan::new(staged, root);
		let mut found = 0u32;
		loop {
			if let Some(wanted) = plan.turn().unwrap() {
				let uuids: Vec<Uuid> = wanted.files.iter().chain(&wanted.dirs).copied().collect();
				plan.objects
					.extend(hydrate_by_uuids(&cache, &uuids).unwrap());
			}
			let Some(staged) = plan.pager.next_in(Run::Transfers).unwrap() else {
				break;
			};
			let SyncAction::DownloadFile { remote_uuid, .. } = staged.entry.action else {
				panic!("only downloads were staged");
			};
			found += u32::from(plan.file(remote_uuid).is_some());
		}
		found
	});
	assert_eq!(found, DOWNLOADS);
	assert!(
		peak < 2 * 1024 * 1024,
		"hydrating held {peak} bytes at once"
	);
}

/// What an [`ApplyContext`] borrows, for steps of the apply run with no account behind them: an
/// offline client, an empty baseline, an empty local root and a gate nothing pauses.
struct Borrowed {
	client: Client,
	local: tempfile::TempDir,
	store: SharedStore,
	pending: PendingWrites,
	observed: Observations,
	gate: PassGate,
}

impl Borrowed {
	fn new() -> Self {
		Self {
			client: offline_client(),
			local: tempfile::tempdir().unwrap(),
			store: Arc::new(Mutex::new(BaselineStore::open_in_memory().unwrap())),
			pending: PendingWrites::default(),
			observed: Observations::default(),
			gate: PassGate::default(),
		}
	}

	/// A pass over the sync root `root` that took `cache_db` as its cache.
	fn ctx(&self, root: Uuid, cache_db: Option<PathBuf>) -> ApplyContext<'_> {
		ApplyContext {
			client: &self.client,
			local_root: self.local.path(),
			pair: 1,
			mode: SyncMode::TwoWay,
			store: &self.store,
			root_remote: RemoteDirectory::from(cacheable_dir(root, root, "root")),
			cache_db,
			absence_trusted: true,
			pending: &self.pending,
			observed: &self.observed,
			gate: &self.gate,
			lock_budget: LockBudget::default(),
		}
	}
}

/// A page turn reads the cache DB the pass took at its start, and never waits on the client's
/// cache slot: a flush holds that until the cache worker exits, which can wait on the request
/// permits of the transfers in flight, and only the loop turning the page polls those.
#[tokio::test]
async fn a_page_turn_reads_the_cache_while_the_client_holds_its_cache_slot() {
	let cache_dir = tempfile::tempdir().unwrap();
	let cache = cache_dir.path().join("cache.db");
	let (root, file) = (Uuid::from_u128(1), Uuid::from_u128(2));
	let mut state = CacheState::new_on_path(&cache, root);
	state
		.upsert_files(std::iter::once(cacheable_file(file, root, "a.txt")))
		.unwrap();
	let (_dir, stage) = Stage::in_temp_dir();
	let mut writer = PlanWriter::new(stage).unwrap();
	writer
		.push(&PlannedEntry::bare(SyncAction::DownloadFile {
			rel_path: "a.txt".to_string(),
			remote_uuid: file,
		}))
		.unwrap();
	let mut plan = PagedPlan::new(writer.finish().unwrap(), root);
	let borrowed = Borrowed::new();
	let ctx = borrowed.ctx(root, Some(cache));
	// Held through the turn, as a flush waiting out the cache worker holds it.
	let _slot = borrowed.client.cache_slot.lock().await;
	let staged = tokio::time::timeout(Duration::from_secs(30), plan.next_in(&ctx, Run::Transfers))
		.await
		.expect("the page turn waited on the client's cache slot")
		.expect("reading the plan's one page")
		.expect("the download is the plan's one transfer");
	assert_eq!(staged.entry.action.rel_path(), "a.txt");
	assert!(
		plan.file(file).is_some(),
		"the page did not hydrate the file its download fetches"
	);
}

/// Where an upload lands, when the pass's stage cannot be read, fails that upload — as a record
/// the pass could not keep, so it starts nothing more — rather than panicking in the loop that
/// holds the other transfers in flight.
#[test]
fn a_landing_the_stage_cannot_read_fails_the_upload_and_the_pass() {
	let root = Uuid::from_u128(1);
	let (_dir, stage) = Stage::in_temp_dir();
	let mut writer = PlanWriter::new(stage).unwrap();
	writer
		.push(&PlannedEntry::bare(SyncAction::CreateRemoteDir {
			rel_path: "made".to_string(),
		}))
		.unwrap();
	let plan = PagedPlan::new(writer.finish().unwrap(), root);
	plan.pager.break_stage();
	let entry = PlannedEntry::bare(upload("made/x.txt"));
	let Err(error) = plan.transfer_target(&entry) else {
		panic!("an upload with nowhere to land found somewhere to");
	};
	let mut report = SyncReport::default();
	let mut events = Vec::new();
	note_action_failed(
		&entry.action,
		&error,
		&FullSides::default(),
		&mut report,
		&mut |event| events.push(event),
	);
	assert!(matches!(
		events.as_slice(),
		[SyncEvent::ActionFailed { rel_path, .. }] if rel_path == "made/x.txt"
	));
	assert_eq!(report.failed_paths.len(), 1);
	assert_eq!(report.failed_paths[0].0, "made/x.txt");
	assert_eq!(report.halted, None);
	assert_eq!(
		report.store_failed,
		Some(StoreFailure::RecordNotWritten(ErrorKind::Internal))
	);
	assert!(!admits_more(&report));
}

/// A page of the plan the stage cannot read fails the pass, rather than panicking in the loop
/// that holds the transfers in flight as it turns to that page: each transfer already started
/// finishes and records what it did, and the action on the unread page is owed to the next
/// pass.
///
/// A page of downloads and one more, so the loop turns to the second page once, with the default
/// concurrency's worth of the first page's downloads still in flight. Each download finds its
/// file in the cache and fails before it reaches the network, on a path no write may take.
#[tokio::test]
async fn a_page_the_stage_cannot_read_fails_the_pass_once_the_transfers_in_flight_finish() {
	let downloads = PAGE_ROWS + 1;
	let cache_dir = tempfile::tempdir().unwrap();
	let cache = cache_dir.path().join("cache.db");
	let root = Uuid::from_u128(u128::MAX);
	let file_uuid = |n: usize| Uuid::from_u128(u128::try_from(n).unwrap() + 1);
	let rel_path = |n: usize| format!("up/../f{n}");
	let mut state = CacheState::new_on_path(&cache, root);
	state
		.upsert_files((0..downloads).map(|n| cacheable_file(file_uuid(n), root, &format!("f{n}"))))
		.unwrap();
	let (_dir, stage) = Stage::in_temp_dir();
	let mut writer = PlanWriter::new(stage).unwrap();
	for n in 0..downloads {
		writer
			.push(&PlannedEntry::bare(SyncAction::DownloadFile {
				rel_path: rel_path(n),
				remote_uuid: file_uuid(n),
			}))
			.unwrap();
	}
	let plan = writer.finish().unwrap();
	plan.spoil_from(PAGE_ROWS);
	let borrowed = Borrowed::new();
	let mut report = SyncReport::default();
	let mut events = Vec::new();
	apply(
		borrowed.ctx(root, Some(cache)),
		plan,
		&mut report,
		&mut |event| events.push(event),
	)
	.await;
	let mut failed: Vec<&str> = report
		.failed_paths
		.iter()
		.map(|(path, _)| path.as_str())
		.collect();
	failed.sort_unstable();
	let mut started: Vec<String> = (0..PAGE_ROWS).map(rel_path).collect();
	started.sort_unstable();
	assert_eq!(failed, started);
	assert_eq!(
		events
			.iter()
			.filter(|event| matches!(event, SyncEvent::ActionFailed { .. }))
			.count(),
		PAGE_ROWS
	);
	assert_eq!(report.downloaded, 0);
	assert_eq!(report.interrupted, 1);
	assert!(matches!(
		events.last(),
		Some(SyncEvent::Interrupted { actions: 1 })
	));
	assert_eq!(
		report.store_failed,
		Some(StoreFailure::UnreadPlan(ErrorKind::Internal))
	);
	assert!(
		report
			.errors
			.iter()
			.any(|line| line.contains("reading a page of the pass's staged plan")),
		"{:?}",
		report.errors.last()
	);
}

/// A pass that could not take the drive lock fails with the lock's own answer, found among
/// the per-path failures it carries alongside.
#[test]
fn a_pass_that_could_not_take_the_drive_lock_fails_with_the_locks_answer() {
	let mut report = SyncReport {
		errors: vec!["docs/a.txt: upload failed".to_string()],
		..SyncReport::default()
	};
	let cause = crate::Error::custom(ErrorKind::RetryFailed, "held by another device");
	note_lock_failure(&mut report, 2, &cause, &mut |_| {});
	assert_eq!(
		report.pass_error(),
		Some(format!("failed to acquire the drive lock: {cause}"))
	);
}

/// A lock failure whose line is missing from `errors` still fails the pass, with the bare
/// lock line.
#[test]
fn a_lock_failure_without_its_line_fails_with_the_bare_lock_line() {
	let report = SyncReport {
		lock_failed: Some(ErrorKind::RetryFailed),
		..SyncReport::default()
	};
	assert_eq!(
		report.pass_error().as_deref(),
		Some("failed to acquire the drive lock")
	);
}

/// A pass that found a side full fails with the line saying which transfers it held back.
#[test]
fn a_halted_pass_fails_with_what_it_held_back() {
	let report = SyncReport {
		halted: Some(HaltReason::LocalStorageFull),
		..SyncReport::default()
	};
	assert_eq!(
		report.pass_error().as_deref(),
		Some("the local disk is full: no further downloads were started this pass")
	);
}

/// A pass that lost a record of work done fails with the store's line.
#[test]
fn a_pass_whose_record_did_not_land_fails_with_the_store_line() {
	let report = SyncReport {
		store_failed: Some(StoreFailure::RecordNotWritten(ErrorKind::IO)),
		..SyncReport::default()
	};
	assert_eq!(
		report.pass_error().as_deref(),
		Some("a record of work already done could not be written")
	);
}

/// A lock failure outranks a full side, and a full side outranks a lost record.
#[test]
fn a_lock_failure_outranks_a_halt_and_a_halt_outranks_a_store_failure() {
	let mut report = SyncReport {
		lock_failed: Some(ErrorKind::RetryFailed),
		halted: Some(HaltReason::RemoteStorageFull),
		store_failed: Some(StoreFailure::UnreadPlan(ErrorKind::IO)),
		..SyncReport::default()
	};
	assert_eq!(
		report.pass_error().as_deref(),
		Some("failed to acquire the drive lock")
	);
	report.lock_failed = None;
	assert_eq!(
		report.pass_error().as_deref(),
		Some("the account's storage is full: no further uploads were started this pass")
	);
}

/// Per-path failures alone do not fail a pass.
#[test]
fn a_pass_with_only_per_path_failures_has_no_pass_error() {
	let report = SyncReport {
		errors: vec!["docs/a.txt: upload failed".to_string()],
		..SyncReport::default()
	};
	assert_eq!(report.pass_error(), None);
}

/// A hold of the drive lock for tests that never reach a server: built in a runtime only the
/// test drives, so when the lock drops, its release is queued there and never sent.
struct UndrivenHold {
	// Dropped first, while the runtime it queues its release on still exists.
	lock: Arc<ResourceLock>,
	runtime: Runtime,
}

impl UndrivenHold {
	fn new() -> Self {
		let runtime = undriven_runtime();
		let lock = held_lock(&runtime);
		Self { lock, runtime }
	}
}

const ROOT: Uuid = Uuid::from_u128(1000);

/// A remote directory create at `rel_path`, where the view listed `parent_uuid` at its parent.
fn create_at(rel_path: &str, parent_uuid: Option<Uuid>) -> PlannedEntry {
	PlannedEntry {
		action: SyncAction::CreateRemoteDir {
			rel_path: rel_path.to_string(),
		},
		start: PassStart::CreateRemoteDir {
			local: None,
			remote: None,
			parent_uuid,
		},
	}
}

/// `entries` staged as they stand, over the sync root [`ROOT`].
fn staged_entries(entries: &[PlannedEntry]) -> (tempfile::TempDir, PagedPlan) {
	let (dir, stage) = Stage::in_temp_dir();
	let mut writer = PlanWriter::new(stage).unwrap();
	for entry in entries {
		writer.push(entry).unwrap();
	}
	(dir, PagedPlan::new(writer.finish().unwrap(), ROOT))
}

/// What a chunk of creates left behind.
struct Landed {
	composed: Vec<NewDirParent>,
	report: SyncReport,
	events: Vec<SyncEvent>,
	settled: usize,
}

/// Compose `chunk` over `plan` and, given `answers`, land it, with the links and shares of
/// [`ROOT`] and of each of `listed` kept empty for `hold`, so no lookup leaves the process.
fn run_chunk(
	borrowed: &Borrowed,
	hold: &UndrivenHold,
	listed: &[Uuid],
	chunk: &[PlannedEntry],
	plan: &mut PagedPlan,
	answers: Option<Vec<Result<CreatedDir, crate::Error>>>,
) -> Landed {
	let looked_up = HashMap::new();
	run_chunk_looked_up(borrowed, hold, listed, chunk, plan, answers, looked_up)
}

/// [`run_chunk`], with the lookups of `looked_up` made already.
fn run_chunk_looked_up(
	borrowed: &Borrowed,
	hold: &UndrivenHold,
	listed: &[Uuid],
	chunk: &[PlannedEntry],
	plan: &mut PagedPlan,
	answers: Option<Vec<Result<CreatedDir, crate::Error>>>,
	mut looked_up: HashMap<Uuid, Result<(), Arc<crate::Error>>>,
) -> Landed {
	for dir in std::iter::once(&ROOT).chain(listed) {
		borrowed
			.client
			.keep_held_targets(&hold.lock, *dir, ConnectedTargets::default());
	}
	let ctx = borrowed.ctx(ROOT, None);
	let full = FullSides::default();
	let chunk_ctx = ChunkContext {
		ctx: &ctx,
		hold: &hold.lock,
		full: &full,
	};
	let mut report = SyncReport::default();
	let mut events = Vec::new();
	let mut observer = |event| events.push(event);
	let (composed, settled) = hold.runtime.block_on(async {
		let (composed, mut settled) = compose_creates(
			&chunk_ctx,
			chunk,
			plan,
			&FailedDirMoves::default(),
			&mut looked_up,
			&mut report,
			&mut observer,
		)
		.await;
		if let Some(answers) = answers {
			settled += land_creates(
				&chunk_ctx,
				&composed,
				answers,
				plan,
				&mut report,
				&mut observer,
			)
			.await;
		}
		(
			composed.iter().map(|create| create.dir.parent).collect(),
			settled,
		)
	});
	Landed {
		composed,
		report,
		events,
		settled,
	}
}

fn created_dir(uuid: Uuid, merged: bool) -> Result<CreatedDir, crate::Error> {
	Ok(CreatedDir {
		uuid,
		merged,
		timestamp: DateTime::UNIX_EPOCH,
	})
}

fn failures(events: &[SyncEvent]) -> usize {
	events
		.iter()
		.filter(|event| matches!(event, SyncEvent::ActionFailed { .. }))
		.count()
}

/// A borrowed context whose store holds the pair [`Borrowed::ctx`] names.
fn borrowed_with_pair() -> Borrowed {
	let borrowed = Borrowed::new();
	let (pair, _) = locked(&borrowed.store)
		.create_pair("/local", ROOT, SyncMode::TwoWay)
		.unwrap();
	assert_eq!(pair, borrowed.ctx(ROOT, None).pair);
	borrowed
}

#[test]
fn a_run_of_creates_names_each_parent_by_its_place_in_the_chunk() {
	let chunk = [
		create_at("a", None),
		create_at("a/b", None),
		create_at("a/b/c", None),
		create_at("d", None),
	];
	let (_dir, mut plan) = staged_entries(&chunk);
	let (borrowed, hold) = (Borrowed::new(), UndrivenHold::new());
	let landed = run_chunk(&borrowed, &hold, &[], &chunk, &mut plan, None);
	assert_eq!(
		landed.composed,
		[
			NewDirParent::Existing(ROOT),
			NewDirParent::Earlier(0),
			NewDirParent::Earlier(1),
			NewDirParent::Existing(ROOT),
		]
	);
	assert_eq!(landed.settled, 0);
}

#[test]
fn a_create_under_a_directory_an_earlier_chunk_created_names_its_answered_uuid() {
	let answered = Uuid::from_u128(1);
	let (_dir, mut plan) = staged_entries(&[create_at("a", None), create_at("a/b", None)]);
	plan.record_created("a", answered).unwrap();
	let (borrowed, hold) = (Borrowed::new(), UndrivenHold::new());
	let chunk = [create_at("a/b", None)];
	let landed = run_chunk(&borrowed, &hold, &[answered], &chunk, &mut plan, None);
	assert_eq!(landed.composed, [NewDirParent::Existing(answered)]);
}

#[test]
fn a_create_under_a_listed_directory_names_the_listed_uuid() {
	let listed = Uuid::from_u128(1);
	let chunk = [create_at("a/b", Some(listed))];
	let (_dir, mut plan) = staged_entries(&chunk);
	let (borrowed, hold) = (Borrowed::new(), UndrivenHold::new());
	let landed = run_chunk(&borrowed, &hold, &[listed], &chunk, &mut plan, None);
	assert_eq!(landed.composed, [NewDirParent::Existing(listed)]);
}

#[test]
fn a_create_with_no_parent_fails_alone_and_takes_its_children() {
	let chunk = [
		create_at("s", None),
		create_at("x/y", None),
		create_at("x/y/z", None),
	];
	let (_dir, mut plan) = staged_entries(&chunk);
	let (borrowed, hold) = (Borrowed::new(), UndrivenHold::new());
	let landed = run_chunk(&borrowed, &hold, &[], &chunk, &mut plan, None);
	assert_eq!(landed.composed, [NewDirParent::Existing(ROOT)]);
	assert_eq!(landed.settled, 2);
	let failed: Vec<(&str, &str)> = landed
		.report
		.failed_paths
		.iter()
		.map(|(path, error)| (path.as_str(), error.as_str()))
		.collect();
	let error = internal("remote parent dir not yet created").to_string();
	assert_eq!(failed, [("x/y", error.as_str()), ("x/y/z", error.as_str())]);
	assert_eq!(failures(&landed.events), 2);
}

#[test]
fn a_failed_lookup_fails_every_create_under_its_directory_before_anything_is_sent() {
	let listed = Uuid::from_u128(1);
	let chunk = [
		create_at("p/a", Some(listed)),
		create_at("p/a/b", None),
		create_at("p/c", Some(listed)),
		create_at("d", None),
	];
	let (_dir, mut plan) = staged_entries(&chunk);
	let (borrowed, hold) = (Borrowed::new(), UndrivenHold::new());
	let looked_up = HashMap::from([(listed, Err(Arc::new(internal("lookup refused"))))]);
	let landed = run_chunk_looked_up(&borrowed, &hold, &[], &chunk, &mut plan, None, looked_up);
	assert_eq!(landed.composed, [NewDirParent::Existing(ROOT)]);
	assert_eq!(landed.settled, 3);
	let failed: Vec<(&str, &str)> = landed
		.report
		.failed_paths
		.iter()
		.map(|(path, error)| (path.as_str(), error.as_str()))
		.collect();
	let error = internal("lookup refused").to_string();
	assert_eq!(
		failed,
		[
			("p/a", error.as_str()),
			("p/a/b", error.as_str()),
			("p/c", error.as_str())
		]
	);
	assert_eq!(failures(&landed.events), 3);
}

#[test]
fn a_create_after_one_whose_lookup_failed_names_its_parent_by_its_new_place() {
	let listed = Uuid::from_u128(1);
	let chunk = [
		create_at("p/a", Some(listed)),
		create_at("d", None),
		create_at("d/e", None),
	];
	let (_dir, mut plan) = staged_entries(&chunk);
	let (borrowed, hold) = (Borrowed::new(), UndrivenHold::new());
	let looked_up = HashMap::from([(listed, Err(Arc::new(internal("lookup refused"))))]);
	let landed = run_chunk_looked_up(&borrowed, &hold, &[], &chunk, &mut plan, None, looked_up);
	assert_eq!(
		landed.composed,
		[NewDirParent::Existing(ROOT), NewDirParent::Earlier(0)]
	);
	assert_eq!(landed.settled, 1);
}

/// The paths of the chunks the apply's loop takes out of `run`: the first of each on its own, and
/// after one that `same` holds for, the rest of its run, at most `max` more.
fn chunks(
	hold: &UndrivenHold,
	ctx: &ApplyContext<'_>,
	plan: &mut PagedPlan,
	run: Run,
	max: usize,
	same: fn(&SyncAction) -> bool,
) -> Vec<Vec<String>> {
	hold.runtime.block_on(async {
		let mut chunks = Vec::new();
		while let Some(first) = plan.next_in(ctx, run).await.unwrap() {
			let more = if same(&first.entry.action) {
				plan.take_run(ctx, run, max, same).await.unwrap()
			} else {
				Vec::new()
			};
			chunks.push(
				std::iter::once(first)
					.chain(more)
					.map(|staged| staged.entry.action.rel_path().to_string())
					.collect(),
			);
		}
		chunks
	})
}

#[test]
fn a_run_stops_at_another_kind_and_at_its_limit() {
	let borrowed = Borrowed::new();
	let ctx = borrowed.ctx(ROOT, None);
	let hold = UndrivenHold::new();
	let runs = |plan: &mut PagedPlan, max: usize| {
		chunks(&hold, &ctx, plan, Run::Before, max, is_remote_dir_create)
	};

	let (_dir, mut plan) = staged_entries(&[
		create_at("a", None),
		create_at("b", None),
		PlannedEntry::bare(SyncAction::CreateLocalDir {
			rel_path: "c".to_string(),
		}),
		create_at("d", None),
	]);
	assert_eq!(
		runs(&mut plan, SYNC_BULK_CHUNK - 1),
		[vec!["a", "b"], vec!["c"], vec!["d"]]
	);

	let many: Vec<PlannedEntry> = (0..120)
		.map(|n| create_at(&format!("d{n:03}"), None))
		.collect();
	let (_dir, mut plan) = staged_entries(&many);
	let lengths: Vec<usize> = runs(&mut plan, SYNC_BULK_CHUNK - 1)
		.iter()
		.map(Vec::len)
		.collect();
	assert_eq!(lengths, [50, 50, 20]);

	// A hold with room for ten writes, the first of the run already counted against it.
	let budget = LockBudget {
		batch: 10,
		..LockBudget::default()
	};
	let lease = Lease::Held {
		lock: Arc::clone(&hold.lock),
		since: Instant::now(),
		writes: 1,
	};
	let (_dir, mut plan) = staged_entries(&many);
	let room = chunk_room(&lease, &budget);
	let lengths: Vec<usize> = runs(&mut plan, room).iter().map(Vec::len).collect();
	assert_eq!(lengths, [10; 12]);
	assert_eq!(Lease::Unheld.room(&budget), 0);
}

#[test]
fn an_answered_uuid_is_what_the_baseline_the_journal_and_the_stage_record() {
	let (merged, replaced) = (Uuid::from_u128(1), Uuid::from_u128(2));
	let mut create = create_at("a", None);
	create.start = PassStart::CreateRemoteDir {
		local: None,
		remote: Some(Box::new(RemoteNode {
			remote_uuid: replaced,
			..remote_node("a", None)
		})),
		parent_uuid: None,
	};
	let chunk = [create];
	let (_dir, mut plan) = staged_entries(&chunk);
	let (borrowed, hold) = (borrowed_with_pair(), UndrivenHold::new());
	let landed = run_chunk(
		&borrowed,
		&hold,
		&[],
		&chunk,
		&mut plan,
		Some(vec![created_dir(merged, true)]),
	);
	assert_eq!(landed.settled, 1);
	assert_eq!(landed.report.remote_dirs_created, 1);
	let store = locked(&borrowed.store);
	let rows: Vec<(String, Option<Uuid>)> = store
		.entries(1)
		.unwrap()
		.into_iter()
		.map(|row| (row.rel_path, row.remote_uuid))
		.collect();
	assert_eq!(rows, [("a".to_string(), Some(merged))]);
	let journal: Vec<(Uuid, PendingKind)> = store
		.load_pending(0, 0)
		.unwrap()
		.into_iter()
		.map(|row| (row.uuid, row.kind))
		.collect();
	assert_eq!(
		journal,
		[(
			merged,
			PendingKind::Created {
				path: "a".to_string(),
				replaced: Some(replaced),
			}
		)]
	);
	assert_eq!(plan.pager.created_at("a").unwrap(), Some(merged));
}

#[test]
fn a_failed_create_records_nothing() {
	let chunk = [create_at("a", None), create_at("a/b", None)];
	let (_dir, mut plan) = staged_entries(&chunk);
	let (borrowed, hold) = (borrowed_with_pair(), UndrivenHold::new());
	let landed = run_chunk(
		&borrowed,
		&hold,
		&[],
		&chunk,
		&mut plan,
		Some(vec![
			Err(internal("refused")),
			Err(CreateDirsError::ParentNotCreated { item: 1, parent: 0 }.into()),
		]),
	);
	assert_eq!(landed.settled, 2);
	assert!(locked(&borrowed.store).entries(1).unwrap().is_empty());
	let failed: Vec<&str> = landed
		.report
		.failed_paths
		.iter()
		.map(|(path, _)| path.as_str())
		.collect();
	assert_eq!(failed, ["a", "a/b"]);
	assert_eq!(failures(&landed.events), 2);
	assert_eq!(landed.report.remote_dirs_created, 0);
	assert_eq!(plan.pager.created_at("a").unwrap(), None);
}

#[test]
fn a_whole_request_failure_fails_each_item_once() {
	let chunk = [
		create_at("a", None),
		create_at("a/b", None),
		create_at("c", None),
	];
	let (_dir, mut plan) = staged_entries(&chunk);
	let (borrowed, hold) = (borrowed_with_pair(), UndrivenHold::new());
	let landed = run_chunk(
		&borrowed,
		&hold,
		&[],
		&chunk,
		&mut plan,
		Some(all_failed(internal("lock refused"), chunk.len())),
	);
	assert_eq!(landed.settled, 3);
	let failed: Vec<(&str, &str)> = landed
		.report
		.failed_paths
		.iter()
		.map(|(path, error)| (path.as_str(), error.as_str()))
		.collect();
	let error = internal("lock refused").to_string();
	assert_eq!(
		failed,
		[
			("a", error.as_str()),
			("a/b", error.as_str()),
			("c", error.as_str())
		]
	);
	assert_eq!(failures(&landed.events), 3);
	assert!(locked(&borrowed.store).entries(1).unwrap().is_empty());
}

#[test]
fn a_fresh_directory_inherits_its_parents_targets() {
	let (fresh, nested) = (Uuid::from_u128(1), Uuid::from_u128(2));
	let chunk = [create_at("a", None), create_at("a/b", None)];
	let (_dir, mut plan) = staged_entries(&chunk);
	let (borrowed, hold) = (borrowed_with_pair(), UndrivenHold::new());
	let landed = run_chunk(
		&borrowed,
		&hold,
		&[],
		&chunk,
		&mut plan,
		Some(vec![created_dir(fresh, false), created_dir(nested, false)]),
	);
	assert_eq!(landed.report.remote_dirs_created, 2);
	assert!(
		borrowed
			.client
			.kept_held_targets(&hold.lock, fresh)
			.is_some()
	);
	// Only what `fresh` inherited answers the lookup of its own directory.
	assert!(
		borrowed
			.client
			.kept_held_targets(&hold.lock, nested)
			.is_some()
	);
}

/// A merged directory is added to the links and shares of its parent it is missing from, which
/// changes those of its subtree: what the hold kept is forgotten, and the merged directory
/// inherits nothing.
#[test]
fn a_merged_directory_inherits_nothing_and_forgets_what_the_hold_kept() {
	let merged = Uuid::from_u128(3);
	let chunk = [create_at("c", None)];
	let (_dir, mut plan) = staged_entries(&chunk);
	let (borrowed, hold) = (borrowed_with_pair(), UndrivenHold::new());
	let landed = run_chunk(
		&borrowed,
		&hold,
		&[],
		&chunk,
		&mut plan,
		Some(vec![created_dir(merged, true)]),
	);
	assert_eq!(landed.report.remote_dirs_created, 1);
	assert!(
		borrowed
			.client
			.kept_held_targets(&hold.lock, merged)
			.is_none()
	);
	assert!(
		borrowed
			.client
			.kept_held_targets(&hold.lock, ROOT)
			.is_none()
	);
}

#[test]
fn a_directory_its_parents_targets_did_not_all_take_inherits_none_of_them() {
	let (listed, fresh) = (Uuid::from_u128(1), Uuid::from_u128(2));
	let chunk = [create_at("p/a", Some(listed))];
	let (_dir, mut plan) = staged_entries(&chunk);
	let (borrowed, hold) = (borrowed_with_pair(), UndrivenHold::new());
	// A 512-bit key is too small for the share's OAEP padding, so sharing the new directory with
	// this user fails before any request is made.
	borrowed
		.client
		.keep_held_targets(&hold.lock, listed, ConnectedTargets::with_test_users(1));
	let landed = run_chunk(
		&borrowed,
		&hold,
		&[],
		&chunk,
		&mut plan,
		Some(vec![created_dir(fresh, false)]),
	);
	assert_eq!(landed.report.remote_dirs_created, 1);
	assert!(
		borrowed
			.client
			.kept_held_targets(&hold.lock, fresh)
			.is_none()
	);
}

#[test]
fn a_record_that_cannot_be_written_stops_the_chunk_but_not_its_propagation() {
	let (first, second) = (Uuid::from_u128(1), Uuid::from_u128(2));
	let chunk = [create_at("a", None), create_at("b", None)];
	let (_dir, mut plan) = staged_entries(&chunk);
	// No pair in the store, so no row of it can be written.
	let (borrowed, hold) = (Borrowed::new(), UndrivenHold::new());
	let landed = run_chunk(
		&borrowed,
		&hold,
		&[],
		&chunk,
		&mut plan,
		Some(vec![created_dir(first, false), created_dir(second, false)]),
	);
	assert_eq!(landed.settled, 1);
	assert_eq!(
		landed.report.store_failed,
		Some(StoreFailure::RecordNotWritten(ErrorKind::Internal))
	);
	assert_eq!(landed.report.remote_dirs_created, 0);
	assert!(
		borrowed
			.client
			.kept_held_targets(&hold.lock, second)
			.is_some()
	);
}

/// A remote trash at `rel_path` of the item `remote_uuid`, where the view listed the directory
/// `listed`, if any.
fn trash_at(
	rel_path: &str,
	kind: NodeKind,
	remote_uuid: Uuid,
	listed: Option<Uuid>,
) -> PlannedEntry {
	let action = SyncAction::TrashRemote {
		rel_path: rel_path.to_string(),
		kind,
		remote_uuid,
	};
	let Some(listed) = listed else {
		return PlannedEntry::bare(action);
	};
	PlannedEntry {
		action,
		start: PassStart::Remote {
			remote: Some(Box::new(RemoteNode {
				kind: NodeKind::Dir,
				remote_uuid: listed,
				..remote_node(rel_path, None)
			})),
		},
	}
}

/// What composing a chunk of trashes, and landing it, left behind.
struct Trashed {
	/// Each item sent, as its kind and uuid.
	sent: Vec<(&'static str, Uuid)>,
	report: SyncReport,
	events: Vec<SyncEvent>,
	settled: usize,
}

/// Compose `chunk` over `plan` and, given `answers`, land it.
async fn run_trashes(
	borrowed: &Borrowed,
	chunk: &[PlannedEntry],
	plan: &PagedPlan,
	answers: Option<Vec<Result<(), crate::Error>>>,
) -> Trashed {
	let ctx = borrowed.ctx(ROOT, None);
	let full = FullSides::default();
	let mut report = SyncReport::default();
	let mut events = Vec::new();
	let mut observer = |event| events.push(event);
	let (composed, mut settled) = compose_trashes(
		chunk,
		plan,
		&FailedDirMoves::default(),
		&full,
		&mut report,
		&mut observer,
	);
	let sent = composed
		.iter()
		.map(|trash| match &trash.item {
			FileOrDir::File(request) => ("file", request.uuid),
			FileOrDir::Dir(request) => ("dir", request.uuid),
		})
		.collect();
	if let Some(answers) = answers {
		settled += land_trashes(
			&ctx,
			composed,
			answers,
			plan,
			&full,
			&mut report,
			&mut observer,
		)
		.await;
	}
	Trashed {
		sent,
		report,
		events,
		settled,
	}
}

/// The paths the report failed, with their errors.
fn failed_paths(report: &SyncReport) -> Vec<(&str, &str)> {
	report
		.failed_paths
		.iter()
		.map(|(path, error)| (path.as_str(), error.as_str()))
		.collect()
}

#[tokio::test]
async fn a_run_of_trashes_sends_each_item_by_uuid_in_plan_order() {
	let (file, dir, listed) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
	let chunk = [
		trash_at("f", NodeKind::File, file, None),
		trash_at("d", NodeKind::Dir, dir, Some(listed)),
	];
	let (_dir, plan) = staged_entries(&chunk);
	let trashed = run_trashes(&Borrowed::new(), &chunk, &plan, None).await;
	assert_eq!(trashed.sent, [("file", file), ("dir", listed)]);
	assert_eq!(trashed.settled, 0);
	assert_eq!(failures(&trashed.events), 0);
}

#[tokio::test]
async fn a_shielded_trash_fails_alone_and_the_rest_still_goes() {
	let (created, other) = (Uuid::from_u128(1), Uuid::from_u128(2));
	let create = SyncAction::CreateRemoteDir {
		rel_path: "docs".to_string(),
	};
	let trashes = || {
		[
			trash_at("Docs", NodeKind::Dir, created, Some(created)),
			trash_at("x.txt", NodeKind::File, other, None),
		]
	};
	let chunk = trashes();
	let (_dir, mut plan) = staged_entries(
		&std::iter::once(PlannedEntry::bare(create.clone()))
			.chain(trashes())
			.collect::<Vec<_>>(),
	);
	plan.record_created("docs", created).unwrap();
	plan.pager.note_written(&create).unwrap();
	let trashed = run_trashes(&Borrowed::new(), &chunk, &plan, None).await;
	assert_eq!(trashed.sent, [("file", other)]);
	assert_eq!(trashed.settled, 1);
	let refusal = internal(
		"refusing to trash \"Docs\": this pass already wrote that item, or under that name",
	)
	.to_string();
	assert_eq!(failed_paths(&trashed.report), [("Docs", refusal.as_str())]);
	assert_eq!(failures(&trashed.events), 1);
}

#[tokio::test]
async fn a_directory_trash_without_its_listed_directory_fails_alone() {
	let chunk = [
		trash_at("d", NodeKind::Dir, Uuid::from_u128(1), None),
		trash_at("f", NodeKind::File, Uuid::from_u128(2), None),
	];
	let (_dir, plan) = staged_entries(&chunk);
	let trashed = run_trashes(&Borrowed::new(), &chunk, &plan, None).await;
	assert_eq!(trashed.sent, [("file", Uuid::from_u128(2))]);
	assert_eq!(trashed.settled, 1);
	let error = internal("trash target dir missing from the snapshot").to_string();
	assert_eq!(failed_paths(&trashed.report), [("d", error.as_str())]);
}

#[test]
fn a_run_of_trashes_stops_at_another_kind_and_at_its_limit() {
	let borrowed = Borrowed::new();
	let ctx = borrowed.ctx(ROOT, None);
	let hold = UndrivenHold::new();
	let runs =
		|plan: &mut PagedPlan, max: usize| chunks(&hold, &ctx, plan, Run::After, max, is_trash);

	// Deletions run children first, so the plan holds these in this order.
	let (_dir, mut plan) = staged_entries(&[
		PlannedEntry::bare(trash("c", NodeKind::File)),
		PlannedEntry::bare(delete_local("b", NodeKind::File)),
		PlannedEntry::bare(trash("a", NodeKind::File)),
	]);
	assert_eq!(
		runs(&mut plan, SYNC_BULK_CHUNK - 1),
		[vec!["c"], vec!["b"], vec!["a"]]
	);

	let many: Vec<PlannedEntry> = (0..120)
		.map(|n| PlannedEntry::bare(trash(&format!("f{n:03}"), NodeKind::File)))
		.collect();
	let (_dir, mut plan) = staged_entries(&many);
	let lengths: Vec<usize> = runs(&mut plan, SYNC_BULK_CHUNK - 1)
		.iter()
		.map(Vec::len)
		.collect();
	assert_eq!(lengths, [50, 50, 20]);

	// A hold with room for ten writes, the first of the run already counted against it.
	let budget = LockBudget {
		batch: 10,
		..LockBudget::default()
	};
	let lease = Lease::Held {
		lock: Arc::clone(&hold.lock),
		since: Instant::now(),
		writes: 1,
	};
	let (_dir, mut plan) = staged_entries(&many);
	let room = chunk_room(&lease, &budget);
	let lengths: Vec<usize> = runs(&mut plan, room).iter().map(Vec::len).collect();
	assert_eq!(lengths, [10; 12]);
}

/// The pair's baseline rows, and its journal, as (uuid, kind).
fn rows_and_journal(borrowed: &Borrowed) -> (Vec<String>, Vec<(Uuid, PendingKind)>) {
	let store = locked(&borrowed.store);
	let journal = store
		.load_pending(0, 0)
		.unwrap()
		.into_iter()
		.map(|row| (row.uuid, row.kind))
		.collect();
	(rows_of(&store, 1), journal)
}

/// A borrowed context whose store holds the pair, with a row at each of `paths`.
fn borrowed_with_rows(paths: &[&str]) -> Borrowed {
	let borrowed = borrowed_with_pair();
	upsert_files(&locked(&borrowed.store), 1, paths.iter().copied());
	borrowed
}

#[tokio::test]
async fn a_landed_trash_drops_its_row_and_the_rows_it_carried() {
	let docs = Uuid::from_u128(1);
	let borrowed = borrowed_with_rows(&["docs", "docs/a.txt", "docs/sub/b.txt", "other.txt"]);
	let entry = trash_at("docs", NodeKind::Dir, docs, Some(docs));
	let (_dir, plan) = carrying(
		&entry,
		&["docs/a.txt".to_string(), "docs/sub/b.txt".to_string()],
	);
	let trashed = run_trashes(&borrowed, &[entry], &plan, Some(vec![Ok(())])).await;
	assert_eq!(trashed.settled, 1);
	assert_eq!(trashed.report.remotely_trashed, 1);
	assert_eq!(failures(&trashed.events), 0);
	let (rows, journal) = rows_and_journal(&borrowed);
	assert_eq!(rows, ["other.txt"]);
	assert_eq!(
		journal,
		[(
			docs,
			PendingKind::Trashed {
				path: "docs".to_string()
			}
		)]
	);
}

#[tokio::test]
async fn a_record_that_cannot_be_written_stops_the_chunk_of_trashes() {
	let chunk = [
		trash_at("a.txt", NodeKind::File, Uuid::from_u128(1), None),
		trash_at("b.txt", NodeKind::File, Uuid::from_u128(2), None),
	];
	let (_dir, plan) = staged_entries(&chunk);
	// No pair in the store, so no row of it can be written.
	let trashed = run_trashes(&Borrowed::new(), &chunk, &plan, Some(vec![Ok(()), Ok(())])).await;
	assert_eq!(trashed.settled, 1);
	assert_eq!(
		trashed.report.store_failed,
		Some(StoreFailure::RecordNotWritten(ErrorKind::Internal))
	);
	assert_eq!(trashed.report.remotely_trashed, 0);
	assert_eq!(failures(&trashed.events), 1);
	let failed: Vec<&str> = failed_paths(&trashed.report)
		.into_iter()
		.map(|(path, _)| path)
		.collect();
	assert_eq!(failed, ["a.txt"]);
}

#[tokio::test]
async fn a_failed_trash_keeps_its_row() {
	let borrowed = borrowed_with_rows(&["a.txt"]);
	let chunk = [trash_at("a.txt", NodeKind::File, Uuid::from_u128(1), None)];
	let (_dir, plan) = staged_entries(&chunk);
	let trashed = run_trashes(
		&borrowed,
		&chunk,
		&plan,
		Some(vec![Err(internal("refused"))]),
	)
	.await;
	assert_eq!(trashed.settled, 1);
	assert_eq!(trashed.report.remotely_trashed, 0);
	let error = internal("refused").to_string();
	assert_eq!(failed_paths(&trashed.report), [("a.txt", error.as_str())]);
	assert_eq!(failures(&trashed.events), 1);
	assert_eq!(
		rows_and_journal(&borrowed),
		(vec!["a.txt".to_string()], vec![])
	);
}

/// A trash the server answers `file_is_versioned` keeps its row and is owed to the next pass, as
/// a single move answered so is, and is not reported as a failure.
#[tokio::test]
async fn a_trash_of_a_file_replaced_since_the_pass_read_it_is_left_to_the_next_pass() {
	let borrowed = borrowed_with_rows(&["a.txt"]);
	let chunk = [trash_at("a.txt", NodeKind::File, Uuid::from_u128(1), None)];
	let (_dir, plan) = staged_entries(&chunk);
	let versioned = || -> crate::Error {
		filen_types::error::ResponseError::ApiError {
			message: None,
			code: Some("file_is_versioned".to_string()),
		}
		.into()
	};
	let trashed = run_trashes(&borrowed, &chunk, &plan, Some(vec![Err(versioned())])).await;
	assert_eq!(trashed.settled, 1);
	assert_eq!(trashed.report.remotely_trashed, 0);
	assert!(
		trashed.report.errors.is_empty(),
		"{:?}",
		trashed.report.errors
	);
	let error = versioned().to_string();
	assert_eq!(failed_paths(&trashed.report), [("a.txt", error.as_str())]);
	assert_eq!(failures(&trashed.events), 0);
	assert_eq!(
		rows_and_journal(&borrowed),
		(vec!["a.txt".to_string()], vec![])
	);
}

#[tokio::test]
async fn a_whole_trash_request_failure_fails_each_item_once() {
	let borrowed = borrowed_with_rows(&["a.txt", "b.txt"]);
	let chunk = [
		trash_at("a.txt", NodeKind::File, Uuid::from_u128(1), None),
		trash_at("b.txt", NodeKind::File, Uuid::from_u128(2), None),
	];
	let (_dir, plan) = staged_entries(&chunk);
	let trashed = run_trashes(
		&borrowed,
		&chunk,
		&plan,
		Some(all_failed(internal("lock refused"), chunk.len())),
	)
	.await;
	assert_eq!(trashed.settled, 2);
	assert_eq!(trashed.report.remotely_trashed, 0);
	let error = internal("lock refused").to_string();
	assert_eq!(
		failed_paths(&trashed.report),
		[("a.txt", error.as_str()), ("b.txt", error.as_str())]
	);
	assert_eq!(failures(&trashed.events), 2);
	assert_eq!(
		rows_and_journal(&borrowed),
		(vec!["a.txt".to_string(), "b.txt".to_string()], vec![])
	);
}
