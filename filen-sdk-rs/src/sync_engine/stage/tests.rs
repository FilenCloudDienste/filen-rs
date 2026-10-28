use std::collections::{BTreeMap, HashSet};

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use rand::{Rng, SeedableRng, rngs::StdRng};
use uuid::Uuid;

use super::{
	super::{
		apply::{PassStart, PlannedEntry},
		baseline::{BaselineEntry, BaselineState, NodeKind},
		plan::{RemoteNode, SyncAction, is_under, replaced_paths},
		scan::LocalNode,
	},
	plan::StagedInputs,
	*,
};
use crate::alloc_meter;

fn upload(rel_path: &str) -> SyncAction {
	SyncAction::UploadFile {
		rel_path: rel_path.to_string(),
	}
}

fn download(rel_path: &str) -> SyncAction {
	SyncAction::DownloadFile {
		rel_path: rel_path.to_string(),
		remote_uuid: Uuid::from_u128(7),
	}
}

fn create_dir(rel_path: &str) -> SyncAction {
	SyncAction::CreateRemoteDir {
		rel_path: rel_path.to_string(),
	}
}

fn trash(rel_path: &str, kind: NodeKind) -> SyncAction {
	SyncAction::TrashRemote {
		rel_path: rel_path.to_string(),
		kind,
		remote_uuid: Uuid::from_u128(8),
	}
}

fn move_dir(from_path: &str, to_path: &str) -> SyncAction {
	SyncAction::MoveRemote {
		from_path: from_path.to_string(),
		to_path: to_path.to_string(),
		kind: NodeKind::Dir,
		remote_uuid: Uuid::from_u128(9),
	}
}

/// A synced file's row at `rel_path`, with no remote item, hash or stamp of its own: what a staged
/// side's reads and edits by row are asked with, and what a rule's hit is a root of.
pub(super) fn row(rel_path: &str) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel_path.to_owned(),
		kind: NodeKind::File,
		remote_uuid: None,
		content_hash: None,
		size: Some(0),
		local_mtime: Some(0),
		remote_modified: None,
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: None,
		agreed_hash: None,
	}
}

/// [`row`], of a file both sides agree on: a remote item, a hash and its stamps.
fn file_row(rel_path: &str) -> BaselineEntry {
	BaselineEntry {
		remote_uuid: Some(Uuid::from_u128(11)),
		content_hash: Some(Blake3Hash::from([1; 32])),
		size: Some(3),
		local_mtime: Some(10),
		remote_modified: Some(20),
		agreed_hash: Some(Blake3Hash::from([1; 32])),
		..row(rel_path)
	}
}

/// `plan` staged as it stands, with nothing read for any action, and handed back in apply order:
/// each action with the run it is in and whether the lock may be let go before it.
fn staged_order(plan: Vec<SyncAction>) -> Vec<(SyncAction, Run, bool)> {
	let (_dir, stage) = Stage::in_temp_dir();
	let mut writer = PlanWriter::new(stage).unwrap();
	for action in plan {
		writer.push(&PlannedEntry::bare(action)).unwrap();
	}
	let mut pager = writer.finish().unwrap().pager();
	let mut order = Vec::new();
	for run in [Run::Before, Run::Transfers, Run::After] {
		while let Some(staged) = pager.next_in(run).unwrap() {
			order.push((staged.entry.action, staged.run, staged.releasable));
		}
	}
	assert_eq!(
		order.len(),
		pager.len(),
		"every staged action is handed out"
	);
	order
}

/// What `staged_order` says, without the runs.
fn releases(plan: Vec<SyncAction>) -> Vec<bool> {
	staged_order(plan)
		.into_iter()
		.map(|(_, _, releasable)| releasable)
		.collect()
}

/// The order the apply ran its plan in before the plan was staged: the split into what runs before,
/// during and after the transfers, `order_transfers` and `release_points`, as the apply had them. Kept
/// as what the staged order is held to.
fn resident_order(plan: &[SyncAction]) -> Vec<(SyncAction, Run, bool)> {
	let replaced = replaced_paths(plan);
	let (mut pre, mut transfers, mut post) = (Vec::new(), Vec::new(), Vec::new());
	for action in plan {
		if action.is_delete() && replaced.contains(action.rel_path()) {
			pre.push(action);
		} else if matches!(
			action,
			SyncAction::UploadFile { .. } | SyncAction::DownloadFile { .. }
		) {
			transfers.push(action);
		} else if action.is_delete() {
			post.push(action);
		} else {
			pre.push(action);
		}
	}
	resident_order_transfers(&pre, &mut transfers);
	let runs = [
		(Run::Before, pre),
		(Run::Transfers, transfers),
		(Run::After, post),
	];
	let order: Vec<(&SyncAction, Run)> = runs
		.into_iter()
		.flat_map(|(run, actions)| actions.into_iter().map(move |action| (action, run)))
		.collect();
	let actions: Vec<&SyncAction> = order.iter().map(|(action, _)| *action).collect();
	let releasable = resident_release_points(&actions, &replaced);
	order
		.into_iter()
		.zip(releasable)
		.map(|((action, run), releasable)| (action.clone(), run, releasable))
		.collect()
}

fn resident_order_transfers(pre: &[&SyncAction], transfers: &mut [&SyncAction]) {
	let replaced: HashSet<&str> = pre
		.iter()
		.filter(|action| action.is_delete())
		.map(|action| action.rel_path())
		.collect();
	let moved_dirs: Vec<&str> = pre
		.iter()
		.filter_map(|action| resident_moved_dir(action))
		.collect();
	transfers.sort_by_key(|action| {
		let path = action.rel_path();
		let completes_step = replaced.contains(path)
			|| moved_dirs
				.iter()
				.any(|dir| path == *dir || is_under(path, dir));
		(
			!completes_step,
			matches!(action, SyncAction::UploadFile { .. }),
		)
	});
}

fn resident_moved_dir(action: &SyncAction) -> Option<&str> {
	match action {
		SyncAction::MoveRemote {
			to_path,
			kind: NodeKind::Dir,
			..
		}
		| SyncAction::MoveLocal {
			to_path,
			kind: NodeKind::Dir,
			..
		} => Some(to_path),
		_ => None,
	}
}

fn resident_release_points(order: &[&SyncAction], replaced: &HashSet<String>) -> Vec<bool> {
	let mut last: BTreeMap<&str, usize> = BTreeMap::new();
	for (position, action) in order.iter().enumerate() {
		if !action.is_delete() {
			let (from, to) = action.endpoints();
			last.insert(from, position);
			last.insert(to, position);
		}
	}
	let mut reach: Option<usize> = None;
	order
		.iter()
		.enumerate()
		.map(|(position, action)| {
			let releasable = reach.is_none_or(|reach| reach < position);
			let tied_until = if action.is_delete() && replaced.contains(action.rel_path()) {
				last.get(action.rel_path()).copied()
			} else if let Some(dir) = resident_moved_dir(action) {
				let prefix = format!("{dir}/");
				last.range(prefix.as_str()..)
					.take_while(|(path, _)| path.starts_with(&prefix))
					.map(|(_, &position)| position)
					.max()
			} else {
				None
			};
			reach = reach.max(tied_until);
			releasable
		})
		.collect()
}

/// Every pragma the module docs name, read back off a stage's connection.
#[test]
fn a_stage_runs_on_the_pragmas_it_names() {
	let (_dir, stage) = Stage::in_temp_dir();
	let int = |name: &str| -> i64 {
		stage
			.conn
			.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
			.unwrap()
	};
	let text = |name: &str| -> String {
		stage
			.conn
			.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
			.unwrap()
	};
	assert_eq!(int("page_size"), 8192);
	assert_eq!(text("journal_mode"), "off");
	assert_eq!(int("synchronous"), 0);
	assert_eq!(text("locking_mode"), "exclusive");
	assert_eq!(int("cache_size"), -2048);
	// Read back, it is 0 when off and otherwise the threshold in pages, which is the cache's own
	// size: what one page costs the cache depends on the build, so only "on" is asserted here, and
	// the next test asserts what it does.
	assert_ne!(int("cache_spill"), 0, "spilling is off");
	assert_eq!(int("temp_store"), 1, "TEMP in a file");
	assert_eq!(int("mmap_size"), 0);
}

/// A transaction larger than the page cache writes its pages to the file as it goes: with 8 MiB of
/// rows in one open transaction against a 2 MiB cache, most of them are already in the file before
/// the commit.
#[test]
fn a_transaction_larger_than_the_cache_spills_to_the_file_before_it_commits() {
	let (_dir, stage) = Stage::in_temp_dir();
	stage
		.conn
		.execute_batch("CREATE TABLE filler (body BLOB NOT NULL); BEGIN;")
		.unwrap();
	for _ in 0..2048 {
		stage
			.conn
			.execute("INSERT INTO filler VALUES (zeroblob(4096))", [])
			.unwrap();
	}
	let spilled = std::fs::metadata(stage.path()).unwrap().len();
	assert!(
		spilled >= 4 * 1024 * 1024,
		"only {spilled} bytes reached the file before the commit"
	);
	stage.conn.execute_batch("COMMIT").unwrap();
}

/// The file goes with the stage, and a path that is taken is refused without touching what is
/// there.
#[test]
fn a_stage_file_goes_with_the_stage_and_a_taken_path_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	let path = stage_path(&dir.path().join("baseline.db"), 3, 4);
	assert_eq!(path, dir.path().join("baseline.db-stage-3-4"));
	let stage = Stage::create(path.clone()).unwrap();
	stage.conn.execute_batch("CREATE TABLE kept (v)").unwrap();
	assert!(path.exists());

	assert!(Stage::create(path.clone()).is_err());
	assert!(path.exists(), "a refused stage removed the file it found");
	stage
		.conn
		.execute_batch("INSERT INTO kept VALUES (1)")
		.unwrap();

	drop(stage);
	assert!(!path.exists(), "the stage's file outlived it");
}

/// Opening a baseline removes the stages earlier engines on it left behind, and nothing else: not a
/// stage of another baseline in the same directory, nor the baseline's own files.
#[test]
fn the_sweep_removes_the_stages_of_its_own_baseline_only() {
	let dir = tempfile::tempdir().unwrap();
	let db = dir.path().join("baseline.db");
	let names = [
		"baseline.db",
		"baseline.db-wal",
		"baseline.db-stage-1-0",
		"baseline.db-stage-12-345",
		"other.db-stage-1-0",
		"baseline.db.bak",
	];
	for name in names {
		std::fs::write(dir.path().join(name), b"x").unwrap();
	}
	sweep(&db);
	let mut left: Vec<String> = std::fs::read_dir(dir.path())
		.unwrap()
		.map(|entry| entry.unwrap().file_name().into_string().unwrap())
		.collect();
	left.sort();
	assert_eq!(
		left,
		[
			"baseline.db",
			"baseline.db-wal",
			"baseline.db.bak",
			"other.db-stage-1-0"
		]
	);
}

/// Every kind of action reads back as it was staged, with every input its entry can carry: a node
/// whose own path is not the action's included.
#[test]
fn every_entry_reads_back_as_it_was_staged() {
	let uuid = Uuid::from_u128(5);
	let local = LocalNode {
		rel_path: "elsewhere/l.txt".to_string(),
		kind: NodeKind::File,
		size: u64::try_from(i64::MAX).unwrap(),
		mtime_millis: -3,
		mtime_nanos: 0,
		content_hash: Some(Blake3Hash::from([2; 32])),
	};
	let remote = RemoteNode {
		rel_path: "r".to_string(),
		kind: NodeKind::Dir,
		remote_uuid: uuid,
		stable_uuid: Some(StableUuid::new_for_test(Uuid::from_u128(6))),
		content_hash: Some(Blake3Hash::from([3; 32])),
		size: 0,
		modified_millis: 1_700_000_000_000,
	};
	let conflicted = BaselineEntry {
		state: BaselineState::Overwritten,
		local_kind: Some(NodeKind::File),
		remote_kind: Some(NodeKind::Dir),
		remote_hash: Some(Blake3Hash::from([4; 32])),
		remote_size: Some(9),
		remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
		..file_row("r")
	};
	let full = || StagedInputs {
		local: Some(Box::new(local.clone())),
		remote: Some(Box::new(remote.clone())),
		base: Some(Box::new(conflicted.clone())),
		base_from: Some(Box::new(file_row("from"))),
		parent_uuid: Some(uuid),
	};
	let actions = vec![
		SyncAction::CreateLocalDir {
			rel_path: "r".to_string(),
		},
		download("r"),
		SyncAction::DeleteLocal {
			rel_path: "r".to_string(),
			kind: NodeKind::Dir,
		},
		create_dir("r"),
		upload("r"),
		trash("r", NodeKind::File),
		SyncAction::TrashRemote {
			rel_path: "r".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: uuid,
		},
		SyncAction::Conflict {
			rel_path: "r".to_string(),
		},
		SyncAction::AdoptBaseline {
			rel_path: "r".to_string(),
		},
		SyncAction::MoveRemote {
			from_path: "from".to_string(),
			to_path: "r".to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid,
		},
		SyncAction::MoveRemote {
			from_path: "from".to_string(),
			to_path: "r".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: uuid,
		},
		SyncAction::MoveLocal {
			from_path: "from".to_string(),
			to_path: "r".to_string(),
			kind: NodeKind::File,
		},
		SyncAction::MoveLocal {
			from_path: "from".to_string(),
			to_path: "r".to_string(),
			kind: NodeKind::Dir,
		},
	];
	let mut entries: Vec<PlannedEntry> = actions
		.iter()
		.cloned()
		.map(|action| PlannedEntry {
			start: PassStart::of(&action, &mut full()),
			action,
		})
		.collect();
	entries.push(PlannedEntry::bare(upload("bare")));

	let (_dir, stage) = Stage::in_temp_dir();
	let mut writer = PlanWriter::new(stage).unwrap();
	for entry in &entries {
		writer.push(entry).unwrap();
	}
	let mut read = Vec::new();
	writer
		.finish()
		.unwrap()
		.pager()
		.sweep(None, |entry| read.push(entry))
		.unwrap();
	// The apply's order, not the plan's: compare each staged entry against the one it was made from.
	assert_eq!(read.len(), entries.len());
	for entry in &entries {
		assert_eq!(
			read.iter().filter(|staged| *staged == entry).count(),
			1,
			"{entry:?} did not read back as it was staged"
		);
	}
}

/// What a directory deletion carries is staged under it, by its side and its path, and in no run:
/// the apply is handed the directory's deletion alone, and reads what it carries out of the stage.
#[test]
fn a_directory_deletion_reads_back_the_deletions_it_carries() {
	let delete_docs = SyncAction::DeleteLocal {
		rel_path: "docs".to_string(),
		kind: NodeKind::Dir,
	};
	let plan = [
		(
			trash("docs", NodeKind::Dir),
			vec!["docs/a.txt", "docs/sub", "docs/sub/b.txt"],
		),
		(delete_docs, vec!["docs/c.txt"]),
		(trash("other", NodeKind::Dir), vec![]),
		(upload("docs2/x"), vec![]),
	];
	let (_dir, stage) = Stage::in_temp_dir();
	let mut writer = PlanWriter::new(stage).unwrap();
	for (action, carried) in &plan {
		writer.push(&PlannedEntry::bare(action.clone())).unwrap();
		let carried: Vec<String> = carried.iter().map(|path| (*path).to_string()).collect();
		writer.carry(action, &carried).unwrap();
	}
	let mut pager = writer.finish().unwrap().pager();
	for (action, carried) in &plan {
		let read = pager.carried(action).map(|rows| {
			let mut read = Vec::new();
			rows.each(&mut |path| {
				read.push(path.to_string());
				Ok(())
			})
			.unwrap();
			read.sort();
			read
		});
		let staged: Option<Vec<String>> =
			(!carried.is_empty()).then(|| carried.iter().map(|path| (*path).to_string()).collect());
		assert_eq!(read, staged, "{action:?}");
	}
	let mut handed = Vec::new();
	for run in [Run::Before, Run::Transfers, Run::After] {
		while let Some(staged) = pager.next_in(run).unwrap() {
			handed.push(staged.entry.action);
		}
	}
	assert_eq!(handed.len(), plan.len(), "{handed:?}");
	assert_eq!(pager.len(), plan.len());
}

/// Between independent actions — and across runs no tie crosses — the lock may go anywhere.
#[test]
fn independent_actions_may_release_between_any_two() {
	let plan = vec![
		create_dir("a"),
		create_dir("a/b"),
		upload("a/x"),
		download("y"),
		trash("old", NodeKind::File),
	];
	assert_eq!(releases(plan), vec![true; 5]);
}

/// A type flip empties its path until the create at it runs: no release may land in between,
/// whatever runs between the two halves.
#[test]
fn a_release_never_splits_a_replace_delete_from_its_create() {
	let plan = vec![
		trash("x", NodeKind::File),
		create_dir("a"),
		create_dir("x"),
		create_dir("z"),
	];
	assert_eq!(releases(plan), vec![true, false, false, true]);

	// The directory half of a flip is recreated by a TRANSFER, in the next run.
	let plan = vec![trash("x", NodeKind::Dir), upload("x"), upload("y")];
	assert_eq!(
		staged_order(plan),
		vec![
			(trash("x", NodeKind::Dir), Run::Before, true),
			(upload("x"), Run::Transfers, false),
			(upload("y"), Run::Transfers, true),
		]
	);
}

/// What the plan names under a moved directory — a nested move, a create, a transfer — assumes the
/// move just happened; a deletion under it names its item by uuid and waits on nothing.
#[test]
fn a_release_never_splits_a_directory_move_from_what_it_carries() {
	let plan = vec![
		move_dir("d", "e"),
		move_dir("e/in", "e/in2"),
		create_dir("a"),
		create_dir("e/in2/new"),
		upload("e/in2/f"),
		upload("g"),
		trash("e/old", NodeKind::File),
	];
	assert_eq!(
		releases(plan),
		vec![true, false, false, false, false, true, true]
	);

	// A sibling that only shares the name's prefix is not under the moved directory.
	let plan = vec![move_dir("d", "e"), upload("ex"), upload("e2/f")];
	assert_eq!(releases(plan), vec![true, true, true]);
}

/// The transfers that finish a compound step go first, so the step — and the stretch the lock cannot
/// be let go in — ends as early as it can; then the downloads, which need no lock; then the uploads.
#[test]
fn transfers_that_finish_a_compound_step_run_first_then_downloads_then_uploads() {
	let plan = vec![
		trash("flip", NodeKind::Dir),
		move_dir("d", "e"),
		upload("a"),
		download("b"),
		upload("e/child"),
		download("c"),
		upload("flip"),
	];
	let paths: Vec<String> = staged_order(plan)
		.into_iter()
		.map(|(action, _, _)| action.rel_path().to_owned())
		.collect();
	assert_eq!(paths, ["flip", "e", "e/child", "flip", "b", "c", "a"]);
}

/// A random action over a namespace small enough that plans flip types, nest moves and share
/// prefixes often.
fn random_action(rng: &mut StdRng) -> SyncAction {
	let mut path = || {
		(0..rng.random_range(1..=3))
			.map(|_| ["a", "b", "e", "ex"][rng.random_range(0..4)])
			.collect::<Vec<_>>()
			.join("/")
	};
	let (rel_path, from_path) = (path(), path());
	let kind = if rng.random_bool(0.5) {
		NodeKind::Dir
	} else {
		NodeKind::File
	};
	let remote_uuid = Uuid::from_u128(rng.random());
	match rng.random_range(0..10) {
		0 => SyncAction::CreateLocalDir { rel_path },
		1 => SyncAction::DownloadFile {
			rel_path,
			remote_uuid,
		},
		2 => SyncAction::DeleteLocal { rel_path, kind },
		3 => SyncAction::CreateRemoteDir { rel_path },
		4 => SyncAction::UploadFile { rel_path },
		5 => SyncAction::TrashRemote {
			rel_path,
			kind,
			remote_uuid,
		},
		6 => SyncAction::Conflict { rel_path },
		7 => SyncAction::AdoptBaseline { rel_path },
		8 => SyncAction::MoveRemote {
			from_path,
			to_path: rel_path,
			kind,
			remote_uuid,
		},
		_ => SyncAction::MoveLocal {
			from_path,
			to_path: rel_path,
			kind,
		},
	}
}

/// The staged order is the order the apply ran its plan in before it was staged — which run each
/// action is in, where it runs, and whether the lock may be let go before it — over plans of every
/// shape, in any order.
#[test]
fn the_staged_order_is_the_resident_one_over_generated_plans() {
	for seed in 0..300u64 {
		let mut rng = StdRng::seed_from_u64(seed);
		let len = rng.random_range(0..=40);
		let plan: Vec<SyncAction> = (0..len).map(|_| random_action(&mut rng)).collect();
		assert_eq!(
			staged_order(plan.clone()),
			resident_order(&plan),
			"seed {seed}: {plan:?}"
		);
	}
}

/// A directory move ties the lock to its subtree's last transfer even when the page turns before
/// it: the transfers under the move run at positions 1 to `PAGE_ROWS + 1`, across the turn at
/// `PAGE_ROWS`, and the lock may go only before the move and after them.
#[test]
fn a_directory_move_holds_the_lock_across_a_page_turn() {
	let mut plan = vec![move_dir("d", "e")];
	plan.extend((0..=PAGE_ROWS).map(|n| upload(&format!("e/f{n}"))));
	plan.push(upload("z"));
	let expected: Vec<(SyncAction, Run, bool)> = plan
		.iter()
		.enumerate()
		.map(|(at, action)| {
			let run = if at == 0 { Run::Before } else { Run::Transfers };
			(action.clone(), run, at == 0 || at > PAGE_ROWS + 1)
		})
		.collect();
	assert_eq!(staged_order(plan), expected);
}

/// A type flip ties the lock from its deletion to its create even when the page turns between
/// them: the deletion runs at 0 and the create at `PAGE_ROWS + 1`, after `PAGE_ROWS` other creates,
/// and the transfers begin on the second page too.
#[test]
fn a_type_flip_holds_the_lock_across_a_page_turn() {
	let mut plan = vec![trash("x", NodeKind::File)];
	plan.extend((0..PAGE_ROWS).map(|n| create_dir(&format!("p{n}"))));
	plan.extend([create_dir("x"), create_dir("z"), upload("y")]);
	let expected: Vec<(SyncAction, Run, bool)> = plan
		.iter()
		.enumerate()
		.map(|(at, action)| {
			let run = if at == PAGE_ROWS + 3 {
				Run::Transfers
			} else {
				Run::Before
			};
			(action.clone(), run, at == 0 || at > PAGE_ROWS + 1)
		})
		.collect();
	assert_eq!(staged_order(plan), expected);
}

/// What the apply holds of a 20_000-upload plan while it pages through it is a page of it, not the
/// plan: each upload carries its local node, its row and its parent, as a re-upload's does.
#[test]
fn paging_through_a_plan_holds_a_page_of_it() {
	const UPLOADS: u32 = 20_000;
	let (_dir, stage) = Stage::in_temp_dir();
	let mut writer = PlanWriter::new(stage).unwrap();
	for n in 0..UPLOADS {
		let path = format!("d{:02}/f{n:05}.txt", n % 100);
		let local = LocalNode {
			rel_path: path.clone(),
			kind: NodeKind::File,
			size: 3,
			mtime_millis: 20,
			mtime_nanos: 0,
			content_hash: Some(Blake3Hash::from([1; 32])),
		};
		writer
			.push(&PlannedEntry {
				start: PassStart::Upload {
					local: Some(Box::new(local)),
					remote: None,
					base: Some(Box::new(file_row(&path))),
					parent_uuid: Some(Uuid::from_u128(u128::from(n % 100))),
				},
				action: upload(&path),
			})
			.unwrap();
	}
	let plan = writer.finish().unwrap();
	let (read, peak) = alloc_meter::peak_bytes(|| {
		let mut pager = plan.pager();
		let mut read = 0u32;
		while let Some(staged) = pager.next_in(Run::Transfers).unwrap() {
			assert!(matches!(
				staged.entry.start,
				PassStart::Upload { base: Some(_), .. }
			));
			read += 1;
		}
		read
	});
	assert_eq!(read, UPLOADS);
	assert!(
		peak < 1024 * 1024,
		"paging held {peak} bytes of the plan at once"
	);
}
