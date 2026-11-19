use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
	time::Duration,
};

use filen_mobile_native_cache::{
	CacheError,
	ffi::{FfiFile, FfiNonRootObject},
};
use filen_types::fs::Uuid;
use tempfile::TempDir;
use tokio::{
	runtime::Handle,
	time::{Instant, sleep},
};

use super::super::{
	FileVersion, LocalConfig, LocalEngine, LocalReport, jobs, lock,
	queue::{Delay, Kind, Kinds, Outcome, Queue},
	report::{ConflictKind, KeptReason, RemoteApplied, UndoneOp, VetoedOp},
};
use crate::{
	dispatch::CloudFilesCallback,
	failure::Failure,
	identity::PlaceholderIdentity,
	os::{Completed, LocalEntry, LocalError, LocalEvent, LocalFs, PinAttr},
	populate::{SkipReason, SkippedItem},
	root_path::RootPath,
	test_support::{DriveCall, DriveOp, FakeDrive, FakeLocal, LocalOp, root_path},
};

const ROOT_UUID: Uuid = Uuid::from_u128(0x100);
const ROOT: PlaceholderIdentity = PlaceholderIdentity::Root { root: ROOT_UUID };

/// Records every report.
#[derive(Default)]
struct Reports {
	local: Mutex<Vec<LocalReport>>,
	skipped: Mutex<Vec<(PlaceholderIdentity, Vec<SkippedItem>)>>,
}

impl CloudFilesCallback for Reports {
	fn on_skipped_items(&self, directory: PlaceholderIdentity, items: &[SkippedItem]) {
		lock(&self.skipped).push((directory, items.to_vec()));
	}

	fn on_local_report(&self, report: &LocalReport) {
		lock(&self.local).push(report.clone());
	}
}

/// A drive and a sync root whose root is populated, with an engine between them.
struct World {
	drive: Arc<FakeDrive>,
	local: FakeLocal,
	engine: LocalEngine<FakeDrive, FakeLocal>,
	reports: Arc<Reports>,
	/// Where the engine keeps its records, when it keeps any, as the provider's does.
	_records: Option<TempDir>,
}

fn root_dir() -> String {
	ROOT_UUID.to_string()
}

/// A world whose drive and root hold nothing yet, the root populated, its engine not started.
fn unstarted() -> World {
	let drive = Arc::new(FakeDrive::new(ROOT_UUID));
	drive.list(&root_dir(), Vec::new());
	let local = FakeLocal::new(ROOT);
	local.set_populated(&RootPath::root(), true);
	drive.link_local(&local);
	let reports = Arc::new(Reports::default());
	let engine = LocalEngine::new(
		Arc::clone(&drive),
		Arc::new(local.clone()),
		Handle::current(),
		LocalConfig::default(),
		Arc::clone(&reports) as Arc<dyn CloudFilesCallback>,
	);
	World {
		drive,
		local,
		engine,
		reports,
		_records: None,
	}
}

/// As [`unstarted`], the engine keeping its records in a directory of its own, as the provider's
/// does: it saves the lineage and drops the records of the renames it confirmed.
fn unstarted_keeping_records() -> World {
	let world = unstarted();
	let records = TempDir::new().expect("a temporary directory");
	let engine = LocalEngine::new_keeping_records(
		Arc::clone(&world.drive),
		Arc::new(world.local.clone()),
		Handle::current(),
		LocalConfig::default(),
		Arc::clone(&world.reports) as Arc<dyn CloudFilesCallback>,
		records.path().to_path_buf(),
	);
	World {
		engine,
		_records: Some(records),
		..world
	}
}

impl World {
	/// Starts the engine and lets it settle.
	async fn started(self) -> Self {
		self.engine.start();
		self.run(Duration::from_secs(5)).await;
		self
	}

	/// Lets `span` pass on the paused clock, handing over the watcher's events every second.
	async fn run(&self, span: Duration) {
		let deadline = Instant::now() + span;
		while Instant::now() < deadline {
			for event in self.local.take_watch_events() {
				self.engine.on_local_event(event);
			}
			sleep(Duration::from_secs(1)).await;
		}
		for event in self.local.take_watch_events() {
			self.engine.on_local_event(event);
		}
	}

	async fn settle(&self) {
		self.run(Duration::from_secs(30)).await;
	}

	fn reports(&self) -> Vec<LocalReport> {
		lock(&self.reports.local).clone()
	}

	fn remote(&self) -> Vec<RemoteApplied> {
		self.reports()
			.into_iter()
			.filter_map(|report| match report {
				LocalReport::Remote(applied) => Some(applied),
				_ => None,
			})
			.collect()
	}

	/// A remote file `name` holding `bytes` in the directory `dir` (a cache id), with its
	/// placeholder at `path`, hydrated or not.
	fn placed(
		&self,
		dir: &str,
		path: &str,
		bytes: &[u8],
		hydrated: bool,
	) -> (FfiFile, PlaceholderIdentity) {
		let name = root_path(path).name().unwrap().as_str().to_owned();
		let file = self.drive.add_remote_file(dir, &name, bytes);
		let identity = identity_of(&file);
		self.local
			.place_file_with(&root_path(path), identity, bytes, hydrated);
		(file, identity)
	}

	fn identity(&self, path: &str) -> Option<PlaceholderIdentity> {
		self.local.entry(&root_path(path))?.identity
	}

	fn state(&self, path: &str) -> Option<crate::os::PlaceholderState> {
		match self.local.inspect(&root_path(path)).unwrap() {
			LocalEntry::Placeholder { state, .. } => Some(state),
			_ => None,
		}
	}

	fn local_ops(&self, keep: impl Fn(&LocalOp) -> bool) -> Vec<LocalOp> {
		self.local.ops().into_iter().filter(keep).collect()
	}
}

fn identity_of(file: &FfiFile) -> PlaceholderIdentity {
	let (stable, version) = jobs::file_ids(file).unwrap();
	PlaceholderIdentity::File { stable, version }
}

fn stable_of(file: &FfiFile) -> Uuid {
	Uuid::parse_str(&file.stable_uuid).unwrap()
}

fn dir_id(uuid: &str) -> String {
	format!("stable/{uuid}")
}

// ---- the queue's kinds ----

fn queue() -> Queue {
	Queue::new(8, Duration::from_secs(1), 4)
}

#[tokio::test(start_paused = true)]
async fn kinds_pushed_on_one_path_merge_and_run_together() {
	let mut queue = queue();
	let path = root_path("a.txt");
	let now = Instant::now();
	queue.push_kind(&path, now, Delay::Now, Kind::Remote);
	queue.push_kind(&path, now, Delay::Now, Kind::Local);
	let taken = queue.take_due(now, 8, usize::MAX);
	assert_eq!(
		taken,
		vec![(path, Kinds::of(Kind::Remote).with(Kind::Local))]
	);
}

#[tokio::test(start_paused = true)]
async fn a_job_that_did_not_finish_owes_its_kinds_and_one_that_did_owes_none() {
	let mut queue = queue();
	let path = root_path("a.txt");
	let now = Instant::now();
	queue.push_kind(&path, now, Delay::Now, Kind::Remote);
	queue.take_due(now, 8, usize::MAX);
	queue.finish(
		&path,
		&Outcome::Busy,
		Kinds::of(Kind::Remote),
		now,
		Duration::from_secs(2),
	);
	let later = now + Duration::from_secs(2);
	assert_eq!(
		queue.take_due(later, 8, usize::MAX),
		vec![(path.clone(), Kinds::of(Kind::Remote))]
	);
	queue.finish(
		&path,
		&Outcome::Done,
		Kinds::NONE,
		later,
		Duration::from_secs(2),
	);
	assert_eq!(queue.next_due(true), None);
}

#[tokio::test(start_paused = true)]
async fn an_event_during_a_job_queues_its_own_kind_after_it() {
	let mut queue = queue();
	let path = root_path("a.txt");
	let now = Instant::now();
	queue.push_kind(&path, now, Delay::Now, Kind::Remote);
	queue.take_due(now, 8, usize::MAX);
	queue.push_kind(&path, now, Delay::Now, Kind::Local);
	queue.finish(
		&path,
		&Outcome::Done,
		Kinds::NONE,
		now,
		Duration::from_secs(2),
	);
	let later = now + Duration::from_secs(1);
	assert_eq!(
		queue.take_due(later, 8, usize::MAX),
		vec![(path, Kinds::of(Kind::Local))]
	);
}

#[tokio::test(start_paused = true)]
async fn a_rebase_carries_the_kinds_a_path_waits_for() {
	let mut queue = queue();
	let now = Instant::now();
	queue.push_kind(&root_path(r"d\a.txt"), now, Delay::Now, Kind::Children);
	queue.rebase(&root_path("d"), &root_path("e"), now);
	assert_eq!(
		queue.take_due(now, 8, usize::MAX),
		vec![(root_path(r"e\a.txt"), Kinds::of(Kind::Children))]
	);
}

#[tokio::test(start_paused = true)]
async fn a_running_job_at_or_under_a_path_makes_it_busy() {
	let mut queue = queue();
	let now = Instant::now();
	queue.push_kind(&root_path(r"d\a.txt"), now, Delay::Now, Kind::Local);
	queue.push_kind(&root_path("b.txt"), now, Delay::Now, Kind::Local);
	queue.take_due(now, 8, usize::MAX);
	assert!(queue.busy_at_or_under(&root_path("d"), &root_path("x")));
	assert!(!queue.busy_at_or_under(&root_path("e"), &root_path("x")));
	assert!(!queue.busy_at_or_under(&root_path("b.txt"), &root_path("B.TXT")));
}

// ---- new remote items ----

#[tokio::test(start_paused = true)]
async fn a_new_remote_file_in_a_populated_directory_gets_a_dehydrated_placeholder() {
	let world = unstarted().started().await;
	let file = world.drive.add_remote_file(&root_dir(), "a.txt", b"hello");
	world.settle().await;
	assert_eq!(world.identity("a.txt"), Some(identity_of(&file)));
	let state = world.state("a.txt").unwrap();
	assert!(state.in_sync());
	assert_eq!(state.on_disk(), 0);
	assert_eq!(world.drive.writes(), Vec::new());
	assert_eq!(
		world.remote(),
		vec![RemoteApplied::Created {
			path: root_path("a.txt"),
			identity: identity_of(&file),
		}]
	);
}

#[tokio::test(start_paused = true)]
async fn a_new_remote_file_in_a_directory_never_populated_gets_nothing() {
	let world = unstarted();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir_unpopulated(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let world = world.started().await;
	world
		.drive
		.add_remote_file(&dir_id(&dir.uuid), "a.txt", b"hello");
	world.settle().await;
	assert_eq!(world.local.entry(&root_path(r"d\a.txt")), None);
}

#[tokio::test(start_paused = true)]
async fn a_new_remote_directory_comes_up_unpopulated() {
	let world = unstarted().started().await;
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	world.settle().await;
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	assert_eq!(world.identity("d"), Some(PlaceholderIdentity::Dir { uuid }));
	assert!(world.state("d").unwrap().partial());
}

#[tokio::test(start_paused = true)]
async fn the_start_up_walk_places_what_the_drive_got_while_the_provider_was_down() {
	let world = unstarted();
	let file = world.drive.add_remote_file(&root_dir(), "a.txt", b"hello");
	let world = world.started().await;
	world.settle().await;
	assert_eq!(world.identity("a.txt"), Some(identity_of(&file)));
	assert_eq!(world.drive.reported_dirs().0, vec![root_dir()]);
}

#[tokio::test(start_paused = true)]
async fn a_remote_name_held_by_a_plain_file_of_other_content_keeps_both() {
	let world = unstarted().started().await;
	world.local.user_create(&root_path("a.txt"), b"mine");
	let file = world.drive.add_remote_file(&root_dir(), "a.txt", b"theirs");
	world.settle().await;
	assert_eq!(world.identity("a.txt"), Some(identity_of(&file)));
	let kept = world
		.local
		.entries()
		.into_iter()
		.find(|entry| entry.bytes == b"mine")
		.expect("the user's file is kept");
	assert_ne!(kept.path, root_path("a.txt"));
	assert!(world.reports().iter().any(|report| matches!(
		report,
		LocalReport::Conflict {
			kind: ConflictKind::CreateVsExisting,
			..
		}
	)));
	let placeholders = world
		.local
		.entries()
		.into_iter()
		.filter(|entry| entry.identity == Some(identity_of(&file)))
		.count();
	assert_eq!(placeholders, 1, "never two placeholders for one item");
}

#[tokio::test(start_paused = true)]
async fn a_remote_name_held_by_a_plain_file_of_the_same_content_is_adopted() {
	let world = unstarted().started().await;
	world.local.user_create(&root_path("a.txt"), b"same");
	let file = world.drive.add_remote_file(&root_dir(), "a.txt", b"same");
	world.settle().await;
	assert_eq!(world.identity("a.txt"), Some(identity_of(&file)));
	assert_eq!(world.local.entries().len(), 1);
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_case_collision_loser_is_skipped_and_reported() {
	let world = unstarted().started().await;
	let first = world.drive.add_remote_file(&root_dir(), "a.txt", b"one");
	let second = world.drive.add_remote_file(&root_dir(), "A.TXT", b"two");
	world.settle().await;
	let (kept, lost) = if stable_of(&first) < stable_of(&second) {
		(&first, &second)
	} else {
		(&second, &first)
	};
	assert_eq!(world.local.entries().len(), 1);
	assert_eq!(world.local.entries()[0].identity, Some(identity_of(kept)));
	let skipped = lock(&world.reports.skipped).clone();
	assert_eq!(skipped.len(), 1);
	assert_eq!(skipped[0].0, ROOT);
	assert_eq!(skipped[0].1.len(), 1);
	assert_eq!(skipped[0].1[0].id(), lost.stable_uuid);
	assert!(matches!(
		skipped[0].1[0].reason(),
		SkipReason::CaseCollision { .. }
	));
}

// ---- content changes ----

#[tokio::test(start_paused = true)]
async fn a_remote_edit_of_a_dehydrated_file_updates_its_placeholder() {
	let world = unstarted();
	let (file, old) = world.placed(&root_dir(), "a.txt", b"old", false);
	let world = world.started().await;
	let version = world.drive.bump_head(stable_of(&file), b"newer!");
	world.settle().await;
	let new = PlaceholderIdentity::File {
		stable: stable_of(&file),
		version,
	};
	assert_eq!(world.identity("a.txt"), Some(new));
	assert_eq!(
		world.local.entry(&root_path("a.txt")).unwrap().bytes.len(),
		6
	);
	assert!(world.state("a.txt").unwrap().in_sync());
	assert_ne!(old, new);
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_remote_edit_of_a_hydrated_file_dehydrates_it() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"old", true);
	let world = world.started().await;
	world.drive.bump_head(stable_of(&file), b"new");
	world.settle().await;
	let state = world.state("a.txt").unwrap();
	assert_eq!(state.on_disk(), 0);
	assert!(state.in_sync());
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_remote_edit_waits_for_a_read_of_the_file_to_end() {
	let world = unstarted();
	let (file, old) = world.placed(&root_dir(), "a.txt", b"old", false);
	let world = world.started().await;
	let PlaceholderIdentity::File { stable, version } = old else {
		panic!("a file placeholder");
	};
	let reading = world
		.engine
		.hints()
		.reading(FileVersion { stable, version }, false);
	world.drive.bump_head(stable_of(&file), b"new");
	world.settle().await;
	assert_eq!(
		world.identity("a.txt"),
		Some(old),
		"not while a read is in flight"
	);
	drop(reading);
	world.run(Duration::from_secs(5)).await;
	assert_ne!(world.identity("a.txt"), Some(old));
}

#[tokio::test(start_paused = true)]
async fn a_remote_edit_never_dehydrates_a_hydrated_file_another_process_holds_open() {
	let world = unstarted();
	let (file, old) = world.placed(&root_dir(), "a.txt", b"old", true);
	let world = world.started().await;
	let reader = world.local.user_open_for_read(&root_path("a.txt"), true);
	world.drive.bump_head(stable_of(&file), b"new");
	world.settle().await;
	assert_eq!(
		world.identity("a.txt"),
		Some(old),
		"not under an open reader"
	);
	assert_eq!(world.state("a.txt").unwrap().on_disk(), 3);
	drop(reader);
	world.run(Duration::from_secs(120)).await;
	assert_ne!(world.identity("a.txt"), Some(old));
	assert_eq!(world.state("a.txt").unwrap().on_disk(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_remote_edit_updates_a_dehydrated_file_another_process_holds_open() {
	let world = unstarted();
	let (file, old) = world.placed(&root_dir(), "a.txt", b"old", false);
	let world = world.started().await;
	let _reader = world.local.user_open_for_read(&root_path("a.txt"), true);
	world.drive.bump_head(stable_of(&file), b"new");
	world.settle().await;
	assert_ne!(
		world.identity("a.txt"),
		Some(old),
		"no byte on disk to mix with the new version"
	);
}

#[tokio::test(start_paused = true)]
async fn a_remote_edit_of_a_locally_edited_file_keeps_both() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"old", true);
	let world = world.started().await;
	let version = world.drive.bump_head(stable_of(&file), b"theirs");
	world
		.local
		.user_write(&root_path("a.txt"), b"mine")
		.unwrap();
	world.settle().await;
	assert_eq!(
		world.identity("a.txt"),
		Some(PlaceholderIdentity::File {
			stable: stable_of(&file),
			version
		})
	);
	assert!(
		world
			.local
			.entries()
			.iter()
			.any(|entry| entry.bytes == b"mine" && entry.path != root_path("a.txt")),
		"the local edit is kept aside"
	);
	assert!(world.reports().iter().any(|report| matches!(
		report,
		LocalReport::Conflict {
			kind: ConflictKind::EditVsEdit,
			..
		}
	)));
	assert!(
		world
			.local_ops(|op| matches!(op, LocalOp::RemoteUpdated { .. }))
			.is_empty(),
		"the remote side never touches an edited file"
	);
	assert_eq!(world.drive.content(stable_of(&file)), b"theirs");
}

#[tokio::test(start_paused = true)]
async fn a_remote_edit_after_an_append_only_write_is_not_applied_over_it() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"old", true);
	let world = world.started().await;
	world.drive.bump_head(stable_of(&file), b"theirs");
	world
		.local
		.user_append_only(&root_path("a.txt"), b"+mine")
		.unwrap();
	world.settle().await;
	assert!(
		world
			.local_ops(|op| matches!(op, LocalOp::RemoteUpdated { .. }))
			.is_empty()
	);
	assert!(
		world
			.local
			.entries()
			.iter()
			.any(|entry| entry.bytes == b"old+mine"),
		"the appended bytes survive"
	);
}

#[tokio::test(start_paused = true)]
async fn a_cached_head_older_than_the_providers_own_upload_never_replaces_the_file() {
	let world = unstarted();
	let (file, old) = world.placed(&root_dir(), "a.txt", b"old", true);
	let world = world.started().await;
	// The cache's reads keep the row as it was before the upload, as a listing fetched before it,
	// or the echo of an earlier upload, written over the upload's row leaves it.
	world.drive.stale_cache(true);
	world
		.local
		.user_write(&root_path("a.txt"), b"mine")
		.unwrap();
	world.settle().await;
	let uploaded = world
		.reports()
		.into_iter()
		.find_map(|report| match report {
			LocalReport::Uploaded { identity, .. } => Some(identity),
			_ => None,
		})
		.expect("the edit was uploaded");
	assert_ne!(uploaded, old);
	assert_eq!(world.drive.content(stable_of(&file)), b"mine");
	// The cache tells of a change; every job on the path reads the older head.
	world.drive.echo_again();
	world.local.lose_watch_events();
	world.settle().await;
	assert_eq!(world.identity("a.txt"), Some(uploaded));
	assert_eq!(
		world.local.entry(&root_path("a.txt")).unwrap().bytes,
		b"mine"
	);
	assert!(world.state("a.txt").unwrap().on_disk() > 0, "still on disk");
	assert!(
		world
			.local_ops(|op| matches!(op, LocalOp::RemoteUpdated { .. }))
			.is_empty(),
		"never moved to the older head"
	);
}

#[tokio::test(start_paused = true)]
async fn a_remote_edit_after_the_providers_own_upload_is_applied_once_the_server_confirms_it() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"old", true);
	let world = world.started().await;
	world
		.local
		.user_write(&root_path("a.txt"), b"mine")
		.unwrap();
	world.settle().await;
	let version = world.drive.bump_head(stable_of(&file), b"theirs");
	world.settle().await;
	assert_eq!(
		world.identity("a.txt"),
		Some(PlaceholderIdentity::File {
			stable: stable_of(&file),
			version
		})
	);
	assert_eq!(world.state("a.txt").unwrap().on_disk(), 0);
	assert!(
		world
			.drive
			.ops()
			.contains(&DriveOp::Refresh(dir_id(&file.stable_uuid))),
		"the server was asked"
	);
}

#[tokio::test(start_paused = true)]
async fn a_read_refused_as_out_of_date_applies_the_update_at_once() {
	let world = unstarted();
	let (file, old) = world.placed(&root_dir(), "a.txt", b"old", false);
	let world = world.started().await;
	world.drive.quiet_signals(true);
	world.drive.bump_head(stable_of(&file), b"new");
	world.run(Duration::from_secs(2)).await;
	assert_eq!(world.identity("a.txt"), Some(old), "no signal, no tick yet");
	world.engine.hints().stale(stable_of(&file));
	world.run(Duration::from_secs(2)).await;
	assert_ne!(world.identity("a.txt"), Some(old));
}

#[tokio::test(start_paused = true)]
async fn a_quiet_change_is_applied_on_the_tick() {
	let world = unstarted();
	let (file, old) = world.placed(&root_dir(), "a.txt", b"old", false);
	let world = world.started().await;
	world.drive.quiet_signals(true);
	world.drive.bump_head(stable_of(&file), b"new");
	world.run(super::feed::TICK + Duration::from_secs(5)).await;
	assert_ne!(world.identity("a.txt"), Some(old));
}

// ---- renames and moves ----

#[tokio::test(start_paused = true)]
async fn a_remote_rename_renames_the_placeholder_with_no_drive_write() {
	let world = unstarted();
	let (file, identity) = world.placed(&root_dir(), "a.txt", b"x", true);
	let world = world.started().await;
	world.drive.remote_rename(stable_of(&file), "b.txt");
	world.settle().await;
	assert_eq!(world.local.entry(&root_path("a.txt")), None);
	assert_eq!(world.identity("b.txt"), Some(identity));
	assert!(world.state("b.txt").unwrap().in_sync());
	assert_eq!(
		world.state("b.txt").unwrap().on_disk(),
		1,
		"its bytes are kept"
	);
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_case_only_remote_rename_is_followed() {
	let world = unstarted();
	let (file, identity) = world.placed(&root_dir(), "a.txt", b"x", false);
	let world = world.started().await;
	world.drive.remote_rename(stable_of(&file), "A.txt");
	world.settle().await;
	let entry = world.local.entry(&root_path("a.txt")).unwrap();
	assert_eq!(entry.path.name().unwrap().as_str(), "A.txt");
	assert_eq!(entry.identity, Some(identity));
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_case_only_remote_rename_keeps_the_hydrated_placeholder_and_its_bytes() {
	let world = unstarted();
	let (file, identity) = world.placed(&root_dir(), "a.txt", b"xyz", true);
	let world = world.started().await;
	world.drive.remote_rename(stable_of(&file), "A.txt");
	world.settle().await;
	let entry = world.local.entry(&root_path("a.txt")).unwrap();
	assert_eq!(entry.path.name().unwrap().as_str(), "A.txt");
	assert_eq!(entry.identity, Some(identity));
	assert_eq!(
		world.state("A.txt").unwrap().on_disk(),
		3,
		"its bytes are kept"
	);
	assert_eq!(
		world.local_ops(|op| matches!(op, LocalOp::Removed(_))),
		Vec::new(),
		"the item's only placeholder is not taken for an extra one"
	);
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn an_edit_in_a_placeholder_renamed_remotely_in_case_only_stays_the_items() {
	let world = unstarted();
	let (file, identity) = world.placed(&root_dir(), "a.txt", b"xyz", true);
	let world = world.started().await;
	world
		.local
		.user_write(&root_path("a.txt"), b"edit")
		.unwrap();
	world.drive.remote_rename(stable_of(&file), "A.txt");
	world.settle().await;
	let entry = world.local.entry(&root_path("a.txt")).unwrap();
	assert!(
		matches!(entry.identity, Some(PlaceholderIdentity::File { stable, .. })
			if stable == stable_of(&file)),
		"the placeholder stays the item's: {:?} (was {identity:?})",
		entry.identity
	);
	assert!(
		!world
			.reports()
			.iter()
			.any(|report| matches!(report, LocalReport::Conflict { .. })),
		"no conflict: {:?}",
		world.reports()
	);
	assert_eq!(
		world.local.entries().len(),
		1,
		"{:?}",
		world.local.entries()
	);
}

#[tokio::test(start_paused = true)]
async fn a_remote_move_between_populated_directories_moves_the_placeholder() {
	let world = unstarted();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let (file, identity) = world.placed(&root_dir(), "a.txt", b"x", true);
	let world = world.started().await;
	world
		.drive
		.remote_move(stable_of(&file), &dir_id(&dir.uuid));
	world.settle().await;
	assert_eq!(world.identity(r"d\a.txt"), Some(identity));
	assert_eq!(world.local.entry(&root_path("a.txt")), None);
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_remote_move_into_an_unpopulated_directory_keeps_the_bytes() {
	let world = unstarted();
	let dir = world.drive.add_remote_dir(&root_dir(), "u");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir_unpopulated(&root_path("u"), PlaceholderIdentity::Dir { uuid });
	let (file, identity) = world.placed(&root_dir(), "a.txt", b"xyz", true);
	let world = world.started().await;
	world
		.drive
		.remote_move(stable_of(&file), &dir_id(&dir.uuid));
	world.settle().await;
	assert_eq!(world.identity(r"u\a.txt"), Some(identity));
	assert_eq!(world.state(r"u\a.txt").unwrap().on_disk(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_remote_move_out_of_the_populated_directories_removes_a_clean_placeholder() {
	let world = unstarted();
	let dir = world.drive.add_remote_dir(&root_dir(), "u");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir_unpopulated(&root_path("u"), PlaceholderIdentity::Dir { uuid });
	let deep = world.drive.add_remote_dir(&dir_id(&dir.uuid), "x");
	let (file, _) = world.placed(&root_dir(), "a.txt", b"x", true);
	let world = world.started().await;
	world
		.drive
		.remote_move(stable_of(&file), &dir_id(&deep.uuid));
	world.settle().await;
	assert_eq!(world.local.entry(&root_path("a.txt")), None);
	assert_eq!(world.drive.writes(), Vec::new());
	assert!(world.remote().contains(&RemoteApplied::Removed {
		path: root_path("a.txt")
	}));
}

#[tokio::test(start_paused = true)]
async fn a_remote_directory_move_carries_its_children() {
	let world = unstarted();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let (_, identity) = world.placed(&dir_id(&dir.uuid), r"d\a.txt", b"x", true);
	let world = world.started().await;
	world.drive.remote_rename(uuid, "e");
	world.settle().await;
	assert_eq!(world.identity(r"e\a.txt"), Some(identity));
	assert_eq!(world.local.entry(&root_path("d")), None);
	assert_eq!(world.drive.writes(), Vec::new());
}

/// A remote rename onto a name a link holds locally: the link is never touched, and the rename
/// waits for the local side, reported once it has waited ten minutes and looked at once a minute
/// from then on; once the link is gone, the placeholder follows the rename.
#[tokio::test(start_paused = true)]
async fn a_remote_rename_onto_a_link_waits_is_reported_and_follows_once_it_is_gone() {
	let world = unstarted();
	let (file, identity) = world.placed(&root_dir(), "a.txt", b"x", true);
	let world = world.started().await;
	world.local.user_create(&root_path("b.txt"), b"link");
	world.local.make_symlink(&root_path("b.txt"));
	world.run(Duration::from_secs(5)).await;
	world.drive.remote_rename(stable_of(&file), "b.txt");
	world.run(Duration::from_secs(9 * 60)).await;
	let parked = |world: &World| {
		world
			.reports()
			.iter()
			.filter(|report| {
				matches!(report, LocalReport::Parked { path, failure: Failure::Transient, .. }
					if *path == root_path("a.txt"))
			})
			.count()
	};
	assert_eq!(parked(&world), 0);
	world.run(Duration::from_secs(2 * 60)).await;
	assert_eq!(parked(&world), 1);
	world.run(Duration::from_secs(10 * 60)).await;
	assert_eq!(parked(&world), 1, "reported once");
	assert_eq!(world.identity("a.txt"), Some(identity));
	world.local.vanish(&root_path("b.txt"));
	world.run(Duration::from_secs(2 * 60)).await;
	assert_eq!(world.identity("b.txt"), Some(identity));
	assert_eq!(world.local.entry(&root_path("a.txt")), None);
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_remote_rename_onto_a_plain_file_moves_the_file_aside_and_uploads_it() {
	let world = unstarted();
	let (file, identity) = world.placed(&root_dir(), "a.txt", b"x", true);
	let world = world.started().await;
	world.local.user_create(&root_path("b.txt"), b"mine");
	world.drive.remote_rename(stable_of(&file), "b.txt");
	world.settle().await;
	assert_eq!(world.identity("b.txt"), Some(identity));
	let kept = world
		.local
		.entries()
		.into_iter()
		.find(|entry| entry.bytes == b"mine")
		.expect("the user's file is kept");
	assert!(kept.placeholder, "uploaded and converted");
	assert!(
		world
			.drive
			.writes()
			.iter()
			.all(|op| matches!(op, DriveOp::Upload { .. } | DriveOp::Rename { .. })),
		"{:?}",
		world.drive.writes()
	);
}

#[tokio::test(start_paused = true)]
async fn a_remote_swap_of_two_names_converges() {
	let world = unstarted();
	let (a, a_identity) = world.placed(&root_dir(), "a.txt", b"a", false);
	let (b, b_identity) = world.placed(&root_dir(), "b.txt", b"b", false);
	let world = world.started().await;
	world.drive.remote_rename(stable_of(&a), "tmp");
	world.drive.remote_rename(stable_of(&b), "a.txt");
	world.drive.remote_rename(stable_of(&a), "b.txt");
	world.settle().await;
	assert_eq!(world.identity("a.txt"), Some(b_identity));
	assert_eq!(world.identity("b.txt"), Some(a_identity));
	assert_eq!(world.local.entries().len(), 2);
	assert_eq!(world.drive.writes(), Vec::new());
}

// ---- deletes and restores ----

#[tokio::test(start_paused = true)]
async fn a_remote_trash_removes_a_clean_file_with_no_drive_write() {
	for hydrated in [false, true] {
		let world = unstarted();
		let (file, _) = world.placed(&root_dir(), "a.txt", b"x", hydrated);
		let world = world.started().await;
		world.drive.remote_trash(stable_of(&file));
		world.settle().await;
		assert_eq!(world.local.entry(&root_path("a.txt")), None);
		assert_eq!(world.drive.writes(), Vec::new(), "nothing trashed back");
		assert_eq!(
			world.local_ops(|op| matches!(op, LocalOp::Removed(_))),
			vec![LocalOp::Removed(root_path("a.txt"))]
		);
	}
}

#[tokio::test(start_paused = true)]
async fn a_reconcile_during_a_local_delete_does_not_bring_the_file_back() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"x", false);
	let world = world.started().await;
	let (request, responder) = world.local.user_delete(&root_path("a.txt")).asked();
	world.engine.on_notify_delete(request, responder);
	// A change in the same directory reconciles it while the delete waits to be confirmed.
	world.drive.add_remote_file(&root_dir(), "b.txt", b"y");
	world.settle().await;
	assert_eq!(
		world.local.entry(&root_path("a.txt")),
		None,
		"stays deleted"
	);
	assert!(world.local.entry(&root_path("b.txt")).is_some());
	assert_eq!(
		world.drive.writes(),
		vec![DriveOp::Trash(dir_id(&file.stable_uuid))],
		"the delete reached the drive"
	);
}

/// `ReplaceFileW` failing as measured in the VM: the rename of the old file onto the backup name it
/// reserved was allowed and never happened. Neither the remote job the provider's own rename on the
/// drive queues for the item nor a reconcile of its directory reads the rename's target while the
/// rename waits, as that opens the reserved file, whose open handle fails the rename over it; the
/// directory is reconciled once the rename settled.
#[tokio::test(start_paused = true)]
async fn a_rename_target_is_never_read_while_the_rename_waits() {
	let world = unstarted();
	world.placed(&root_dir(), "replace.txt", b"x", true);
	let world = world.started().await;
	world.local.fail_completion(&root_path("replace.txt"));
	let (request, responder) = world
		.local
		.user_rename(
			&root_path("replace.txt"),
			&root_path("replace.txt~RFc0bd647.TMP"),
		)
		.unwrap()
		.asked();
	world.engine.on_notify_rename(request, responder);
	world.drive.add_remote_file(&root_dir(), "b.txt", b"y");
	world.run(Duration::from_secs(5)).await;
	assert_eq!(
		world.local.entry(&root_path("b.txt")),
		None,
		"not reconciled while the rename waits"
	);
	assert_eq!(
		world
			.local
			.inspections(&root_path("replace.txt~RFc0bd647.TMP")),
		0,
		"the rename's target is never read while it waits"
	);
	world.settle().await;
	assert!(
		world.reports().contains(&LocalReport::Undone {
			path: root_path("replace.txt"),
			op: UndoneOp::Rename,
		}),
		"{:?}",
		world.reports()
	);
	assert!(
		world.local.entry(&root_path("b.txt")).is_some(),
		"reconciled once the rename settled"
	);
}

#[tokio::test(start_paused = true)]
async fn a_remote_trash_of_an_edited_file_keeps_it_and_uploads_it_anew() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"x", true);
	let world = world.started().await;
	world
		.local
		.user_write(&root_path("a.txt"), b"mine")
		.unwrap();
	world.drive.remote_trash(stable_of(&file));
	world.settle().await;
	let entry = world.local.entry(&root_path("a.txt")).expect("kept");
	assert_eq!(entry.bytes, b"mine");
	assert_eq!(kept_reports(&world), 1, "reported once");
	let uploaded = world
		.drive
		.child(&root_dir(), "a.txt")
		.expect("uploaded as a new file");
	let FfiNonRootObject::File(uploaded) = uploaded else {
		panic!("a file");
	};
	assert_ne!(uploaded.stable_uuid, file.stable_uuid);
	assert_eq!(entry.identity, Some(identity_of(&uploaded)));
}

#[tokio::test(start_paused = true)]
async fn an_edit_a_remote_trash_hands_to_the_local_side_counts_from_the_hand_over() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"x", true);
	let world = world.started().await;
	let gate = world.drive.gate_uploads();
	world
		.local
		.user_write(&root_path("a.txt"), b"mine")
		.unwrap();
	// Written through a handle that raised nothing: only the remote job finds the edit.
	world.local.take_watch_events();
	world.drive.remote_trash(stable_of(&file));
	// The remote job handed the path to the local side, which waits out the settle time.
	sleep(Duration::from_secs(1)).await;
	assert_eq!(world.engine.unsynced_count(), Some(1), "settling");
	world.run(Duration::from_secs(10)).await;
	assert!(
		world.drive.child(&root_dir(), "a.txt").is_none(),
		"held at the gate"
	);
	assert_eq!(world.engine.unsynced_count(), Some(1), "on its way up");
	gate.add_permits(1);
	world.settle().await;
	assert_eq!(kept_reports(&world), 1);
	assert_eq!(world.engine.unsynced_count(), Some(0));
}

fn kept_reports(world: &World) -> usize {
	world
		.remote()
		.iter()
		.filter(|applied| {
			**applied
				== RemoteApplied::KeptLocal {
					path: root_path("a.txt"),
					reason: KeptReason::RemoteGone,
				}
		})
		.count()
}

#[tokio::test(start_paused = true)]
async fn an_edit_whose_upload_is_under_way_when_the_file_is_trashed_is_reported_kept() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"x", true);
	let world = world.started().await;
	world
		.local
		.user_write(&root_path("a.txt"), b"mine")
		.unwrap();
	// The local side has the edit settling when the trash arrives.
	world.run(Duration::from_secs(1)).await;
	world.drive.remote_trash(stable_of(&file));
	world.settle().await;
	let entry = world.local.entry(&root_path("a.txt")).expect("kept");
	assert_eq!(entry.bytes, b"mine");
	assert_ne!(entry.identity, Some(identity_of(&file)));
	assert_eq!(kept_reports(&world), 1, "reported once");
}

#[tokio::test(start_paused = true)]
async fn a_permanent_delete_waits_for_the_server_to_confirm_it() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"x", true);
	let world = world.started().await;
	world.drive.set_offline(true);
	world.drive.remote_delete(stable_of(&file));
	world.settle().await;
	assert!(
		world.local.entry(&root_path("a.txt")).is_some(),
		"a cache miss is not proof while the server cannot be asked"
	);
	world.drive.set_offline(false);
	world.run(Duration::from_secs(300)).await;
	assert_eq!(world.local.entry(&root_path("a.txt")), None);
}

#[tokio::test(start_paused = true)]
async fn placeholders_of_items_deleted_while_the_provider_was_down_are_removed_once_confirmed() {
	let (world, uuid, _) = with_dir_and_file();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"x", false);
	world.drive.remote_delete(uuid);
	world.drive.remote_delete(stable_of(&file));
	let world = world.started().await;
	world.run(Duration::from_secs(120)).await;
	assert_eq!(world.local.entries(), Vec::new());
	assert_eq!(world.drive.writes(), Vec::new());
	assert!(
		!world
			.reports()
			.iter()
			.any(|report| matches!(report, LocalReport::Parked { .. })),
		"{:?}",
		world.reports()
	);
	let refreshes = |id: String| {
		world
			.drive
			.ops()
			.into_iter()
			.filter(|op| *op == DriveOp::Refresh(id.clone()))
			.count()
	};
	assert_eq!(refreshes(dir_id(&uuid.to_string())), 1);
	assert_eq!(refreshes(dir_id(&file.stable_uuid)), 1);
}

#[tokio::test(start_paused = true)]
async fn a_remote_directory_trash_keeps_what_holds_local_data_under_a_new_directory() {
	let world = unstarted();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	world.placed(&dir_id(&dir.uuid), r"d\clean.txt", b"c", false);
	world.placed(&dir_id(&dir.uuid), r"d\edited.txt", b"e", true);
	let sub = world.drive.add_remote_dir(&dir_id(&dir.uuid), "sub");
	world.local.place_dir_unpopulated(
		&root_path(r"d\sub"),
		PlaceholderIdentity::Dir {
			uuid: Uuid::parse_str(&sub.uuid).unwrap(),
		},
	);
	let world = world.started().await;
	world
		.local
		.user_write(&root_path(r"d\edited.txt"), b"mine")
		.unwrap();
	world
		.local
		.user_create(&root_path(r"d\plain.txt"), b"plain");
	world.drive.remote_trash(uuid);
	world.settle().await;
	world.settle().await;
	assert_eq!(world.local.entry(&root_path(r"d\clean.txt")), None);
	assert_eq!(world.local.entry(&root_path(r"d\sub")), None);
	assert_eq!(
		world
			.local
			.entry(&root_path(r"d\edited.txt"))
			.unwrap()
			.bytes,
		b"mine"
	);
	assert!(world.local.entry(&root_path(r"d\plain.txt")).is_some());
	let FfiNonRootObject::Dir(anew) = world.drive.child(&root_dir(), "d").expect("re-created")
	else {
		panic!("a directory");
	};
	assert_ne!(anew.uuid, dir.uuid);
	assert!(
		world
			.drive
			.child(&dir_id(&anew.uuid), "edited.txt")
			.is_some()
	);
	assert!(
		world
			.drive
			.child(&dir_id(&anew.uuid), "plain.txt")
			.is_some()
	);
	assert!(world.remote().contains(&RemoteApplied::DirKept {
		path: root_path("d"),
		kept: 2,
	}));
}

#[tokio::test(start_paused = true)]
async fn a_directory_moved_out_of_one_found_trashed_keeps_its_edit_where_it_went() {
	let world = unstarted();
	let d = world.drive.add_remote_dir(&root_dir(), "d");
	let d_uuid = Uuid::parse_str(&d.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid: d_uuid });
	let e = world.drive.add_remote_dir(&dir_id(&d.uuid), "e");
	let e_uuid = Uuid::parse_str(&e.uuid).unwrap();
	world.local.place_dir(
		&root_path(r"d\e"),
		PlaceholderIdentity::Dir { uuid: e_uuid },
	);
	let (file, _) = world.placed(&dir_id(&e.uuid), r"d\e\a.txt", b"old", true);
	let world = world.started().await;
	world.drive.fail_always(DriveCall::UploadVersion, || {
		CacheError::remote("unreachable")
	});
	world
		.local
		.user_write(&root_path(r"d\e\a.txt"), b"mine")
		.unwrap();
	world.run(Duration::from_secs(10)).await;
	// Another device moves `e` out of `d`, then trashes `d`, and the local side finds `d` gone
	// before the remote side hears of either: its job removes `d`.
	world.drive.quiet_signals(true);
	world.drive.remote_move(e_uuid, &root_dir());
	world.drive.remote_trash(d_uuid);
	world.local.user_touch(&root_path("d"));
	world.settle().await;
	world.drive.stop_failing(DriveCall::UploadVersion);
	world.settle().await;
	world.settle().await;
	assert_eq!(world.local.entry(&root_path("d")), None);
	assert_eq!(
		world.local.entry(&root_path(r"e\a.txt")).unwrap().bytes,
		b"mine"
	);
	assert_eq!(
		world.identity("e"),
		Some(PlaceholderIdentity::Dir { uuid: e_uuid })
	);
	assert_eq!(
		world.drive.names(&root_dir()),
		["e"],
		"{:?}",
		world.drive.writes()
	);
	assert_eq!(
		world.drive.content(stable_of(&file)),
		b"mine",
		"the edit is the remote file's new version: {:?}",
		world.drive.writes()
	);
}

/// A world with the populated directory `d` holding the clean file `d\a.txt`, hydrated: the
/// directory and the file.
fn with_dir_and_file() -> (World, Uuid, FfiFile) {
	let world = unstarted();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let (file, _) = world.placed(&dir_id(&dir.uuid), r"d\a.txt", b"old", true);
	(world, uuid, file)
}

#[tokio::test(start_paused = true)]
async fn an_edit_uploading_when_its_directory_is_trashed_is_kept_in_a_directory_created_anew() {
	let (world, uuid, file) = with_dir_and_file();
	let world = world.started().await;
	let gate = world.drive.gate_uploads();
	world
		.local
		.user_write(&root_path(r"d\a.txt"), b"mine")
		.unwrap();
	world.run(Duration::from_secs(10)).await;
	assert!(
		world
			.drive
			.ops()
			.contains(&DriveOp::UploadVersion(dir_id(&file.stable_uuid))),
		"the edit's upload is under way"
	);
	world.drive.remote_trash(uuid);
	world.run(Duration::from_secs(10)).await;
	gate.add_permits(1_000);
	world.settle().await;
	world.settle().await;
	let entry = world
		.local
		.entry(&root_path(r"d\a.txt"))
		.expect("the edit is kept");
	assert_eq!(entry.bytes, b"mine");
	let Some(FfiNonRootObject::Dir(anew)) = world.drive.child(&root_dir(), "d") else {
		panic!("the directory is created anew");
	};
	assert_ne!(anew.uuid, uuid.to_string());
	let Some(FfiNonRootObject::File(uploaded)) = world.drive.child(&dir_id(&anew.uuid), "a.txt")
	else {
		panic!("the edit is on the drive, in the directory created anew");
	};
	assert_eq!(world.drive.content(stable_of(&uploaded)), b"mine");
	assert_eq!(entry.identity, Some(identity_of(&uploaded)));
}

#[tokio::test(start_paused = true)]
async fn a_new_file_uploading_when_its_directory_is_trashed_is_kept_in_a_directory_created_anew() {
	let (world, uuid, _) = with_dir_and_file();
	let world = world.started().await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path(r"d\new.txt"), b"mine");
	world.run(Duration::from_secs(10)).await;
	assert!(
		world
			.drive
			.ops()
			.iter()
			.any(|op| matches!(op, DriveOp::Upload { .. })),
		"the new file's upload is under way"
	);
	world.drive.remote_trash(uuid);
	world.run(Duration::from_secs(10)).await;
	gate.add_permits(1_000);
	world.settle().await;
	world.settle().await;
	let entry = world
		.local
		.entry(&root_path(r"d\new.txt"))
		.expect("the new file is kept");
	assert_eq!(entry.bytes, b"mine");
	assert_eq!(world.local.entry(&root_path(r"d\a.txt")), None);
	let Some(FfiNonRootObject::Dir(anew)) = world.drive.child(&root_dir(), "d") else {
		panic!("the directory is created anew");
	};
	let Some(FfiNonRootObject::File(uploaded)) = world.drive.child(&dir_id(&anew.uuid), "new.txt")
	else {
		panic!("the new file is on the drive, in the directory created anew");
	};
	assert_eq!(entry.identity, Some(identity_of(&uploaded)));
}

#[tokio::test(start_paused = true)]
async fn a_remote_trash_of_a_directory_not_in_sync_removes_its_clean_items() {
	let (world, uuid, _) = with_dir_and_file();
	let world = world.started().await;
	world.local.user_touch(&root_path("d"));
	world.drive.remote_trash(uuid);
	world.settle().await;
	world.settle().await;
	assert_eq!(world.local.entry(&root_path(r"d\a.txt")), None);
	assert_eq!(world.local.entry(&root_path("d")), None);
	assert_eq!(world.drive.writes(), Vec::new());
}

/// Whether anything brought a file's data onto disk.
fn hydrated_any(world: &World) -> bool {
	world
		.local
		.ops()
		.iter()
		.any(|op| matches!(op, LocalOp::Hydrated(_) | LocalOp::HashedPartial(_)))
}

#[tokio::test(start_paused = true)]
async fn a_remote_directory_trash_removes_an_online_only_file_whose_attributes_changed_and_downloads_nothing()
 {
	let world = unstarted();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	world.placed(&dir_id(&dir.uuid), r"d\a.txt", b"one", false);
	let drive = Arc::clone(&world.drive);
	world
		.local
		.hydrate_from(move |stable| drive.content(stable));
	let world = world.started().await;
	// Not in sync with no byte on disk or written, and no job looked at it before the trash.
	world.local.swallow_events(&root_path(r"d\a.txt"));
	world.local.user_touch(&root_path(r"d\a.txt"));
	world.drive.remote_trash(uuid);
	world.settle().await;
	world.settle().await;
	assert_eq!(world.local.entry(&root_path(r"d\a.txt")), None);
	assert_eq!(world.local.entry(&root_path("d")), None);
	assert!(!hydrated_any(&world), "{:?}", world.local.ops());
	assert_eq!(world.drive.writes(), Vec::new());
	assert!(world.drive.reads().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_remote_edit_of_a_partly_downloaded_file_updates_it_and_downloads_none_of_the_rest() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", &[7; 10], false);
	let drive = Arc::clone(&world.drive);
	world
		.local
		.hydrate_from(move |stable| drive.content(stable));
	let world = world.started().await;
	// Partly on disk once the start-up walk is over, which completes such a file.
	world.local.set_on_disk(&root_path("a.txt"), 4);
	let version = world.drive.bump_head(stable_of(&file), b"newer!");
	world.settle().await;
	assert_eq!(
		world.identity("a.txt"),
		Some(PlaceholderIdentity::File {
			stable: stable_of(&file),
			version,
		})
	);
	let state = world.state("a.txt").unwrap();
	assert_eq!(state.on_disk(), 0);
	assert!(state.in_sync());
	assert!(!hydrated_any(&world), "{:?}", world.local.ops());
	assert_eq!(world.drive.writes(), Vec::new());
	assert!(world.drive.reads().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_directory_moving_out_of_reach_keeps_an_edit_with_its_remote_file() {
	let (world, uuid, file) = with_dir_and_file();
	let away = world.drive.add_remote_dir(&root_dir(), "u");
	world.local.place_dir_unpopulated(
		&root_path("u"),
		PlaceholderIdentity::Dir {
			uuid: Uuid::parse_str(&away.uuid).unwrap(),
		},
	);
	let deep = world.drive.add_remote_dir(&dir_id(&away.uuid), "x");
	let world = world.started().await;
	world
		.local
		.user_write(&root_path(r"d\a.txt"), b"mine")
		.unwrap();
	world.drive.remote_move(uuid, &dir_id(&deep.uuid));
	world.settle().await;
	world.settle().await;
	assert_eq!(
		world.drive.content(stable_of(&file)),
		b"mine",
		"the edit is the remote file's new version"
	);
	assert!(
		world
			.drive
			.writes()
			.iter()
			.all(|op| matches!(op, DriveOp::UploadVersion(_))),
		"no second directory: {:?}",
		world.drive.writes()
	);
}

#[tokio::test(start_paused = true)]
async fn a_reconcile_that_keeps_deferring_does_not_hold_back_its_directorys_remote_job() {
	let (world, uuid, _) = with_dir_and_file();
	world
		.drive
		.add_remote_file(&dir_id(&uuid.to_string()), "b.txt", b"b");
	world.local.fail_placements(Some(LocalError::Busy));
	let world = world.started().await;
	world.drive.remote_rename(uuid, "e");
	world.settle().await;
	assert_eq!(world.identity("e"), Some(PlaceholderIdentity::Dir { uuid }));
	assert_eq!(world.local.entry(&root_path("d")), None);
}

#[tokio::test(start_paused = true)]
async fn a_reconcile_that_keeps_deferring_waits_for_the_next_walk() {
	let (world, uuid, _) = with_dir_and_file();
	world
		.drive
		.add_remote_file(&dir_id(&uuid.to_string()), "b.txt", b"b");
	world.local.fail_placements(Some(LocalError::Busy));
	let world = world.started().await;
	world.run(Duration::from_secs(120)).await;
	assert!(
		world.reports().contains(&LocalReport::Parked {
			path: root_path("d"),
			failure: Failure::Transient,
			attempts: 1,
		}),
		"{:?}",
		world.reports()
	);
	world.local.fail_placements(None);
	world.local.lose_watch_events();
	world.run(Duration::from_secs(5)).await;
	assert!(world.local.entry(&root_path(r"d\b.txt")).is_some());
}

#[tokio::test(start_paused = true)]
async fn a_remote_edit_of_a_pinned_file_ends_pinned_and_hydrated_with_the_new_bytes() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"old", true);
	let source = Arc::clone(&world.drive);
	world
		.local
		.hydrate_from(move |stable| source.content(stable));
	world.local.user_pin(&root_path("a.txt"), PinAttr::Pinned);
	let world = world.started().await;
	let version = world.drive.bump_head(stable_of(&file), b"new");
	world.settle().await;
	let entry = world.local.entry(&root_path("a.txt")).unwrap();
	assert_eq!(
		entry.identity,
		Some(PlaceholderIdentity::File {
			stable: stable_of(&file),
			version
		})
	);
	assert_eq!(entry.pin, PinAttr::Pinned);
	assert_eq!(entry.bytes, b"new");
	assert_eq!(entry.on_disk, 3);
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_restore_after_a_removal_brings_the_placeholder_back() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"x", false);
	let world = world.started().await;
	world.drive.remote_trash(stable_of(&file));
	world.settle().await;
	assert_eq!(world.local.entry(&root_path("a.txt")), None);
	world.drive.remote_restore(stable_of(&file));
	world.settle().await;
	assert_eq!(world.identity("a.txt"), Some(identity_of(&file)));
}

// ---- the feed ----

#[tokio::test(start_paused = true)]
async fn an_expired_anchor_reconciles_without_duplicates() {
	let world = unstarted();
	world.placed(&root_dir(), "a.txt", b"x", false);
	let world = world.started().await;
	let file = world.drive.add_remote_file(&root_dir(), "b.txt", b"y");
	world.drive.wipe_feed();
	world.settle().await;
	assert_eq!(world.identity("b.txt"), Some(identity_of(&file)));
	assert_eq!(world.local.entries().len(), 2);
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn many_changes_are_read_over_pages_and_all_applied() {
	let world = unstarted().started().await;
	world.drive.quiet_signals(true);
	for n in 0..1_200 {
		world
			.drive
			.add_remote_file(&root_dir(), &format!("{n}.txt"), b"x");
	}
	world.drive.quiet_signals(false);
	world.drive.add_remote_file(&root_dir(), "last.txt", b"x");
	world.settle().await;
	assert_eq!(world.local.entries().len(), 1_201);
}

#[tokio::test(start_paused = true)]
async fn a_population_reconciles_the_directory_and_is_reported_to_the_cache() {
	let world = unstarted();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir_unpopulated(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let world = world.started().await;
	let file = world
		.drive
		.add_remote_file(&dir_id(&dir.uuid), "a.txt", b"x");
	let reports = world.drive.reported_dirs().1;
	// The platform populates it, minus a child the drive got meanwhile.
	world.local.populate(&root_path("d"), &[]);
	world
		.engine
		.hints()
		.populated(PlaceholderIdentity::Dir { uuid });
	world.run(Duration::from_secs(1)).await;
	assert_eq!(world.drive.reported_dirs().1, reports, "debounced");
	world.run(Duration::from_secs(5)).await;
	assert_eq!(world.identity(r"d\a.txt"), Some(identity_of(&file)));
	let (mut reported, count) = world.drive.reported_dirs();
	reported.sort();
	let mut expected = vec![root_dir(), dir_id(&dir.uuid)];
	expected.sort();
	assert_eq!(reported, expected);
	assert_eq!(count, reports + 1);
}

#[tokio::test(start_paused = true)]
async fn the_engines_own_remote_changes_raise_no_local_change() {
	let world = unstarted();
	let (a, _) = world.placed(&root_dir(), "a.txt", b"a", true);
	let (b, _) = world.placed(&root_dir(), "b.txt", b"b", false);
	let (c, _) = world.placed(&root_dir(), "c.txt", b"c", false);
	let world = world.started().await;
	world.drive.remote_rename(stable_of(&a), "renamed.txt");
	world.drive.bump_head(stable_of(&b), b"newer");
	world.drive.remote_trash(stable_of(&c));
	world.settle().await;
	world.settle().await;
	assert_eq!(world.drive.writes(), Vec::new());
	assert!(
		world
			.reports()
			.iter()
			.all(|report| matches!(report, LocalReport::Remote(_))),
		"{:?}",
		world.reports()
	);
}

#[tokio::test(start_paused = true)]
async fn a_burst_of_signals_is_read_at_once_and_once_more_after_it() {
	let world = unstarted().started().await;
	world.run(Duration::from_secs(1)).await;
	let before = world.drive.pages_read();
	for n in 0..50 {
		world
			.drive
			.add_remote_file(&root_dir(), &format!("{n}.txt"), b"x");
		sleep(Duration::from_millis(10)).await;
	}
	sleep(Duration::from_secs(2)).await;
	let reads = world.drive.pages_read() - before;
	assert!((1..=2).contains(&reads), "{reads} reads");
}

#[tokio::test(start_paused = true)]
async fn a_user_delete_while_a_remote_edit_waits_trashes_nothing_and_the_edit_comes_back() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"old", false);
	let world = world.started().await;
	world.drive.quiet_signals(true);
	let version = world.drive.bump_head(stable_of(&file), b"new");
	let (request, responder) = world.local.user_delete(&root_path("a.txt")).asked();
	world.engine.on_notify_delete(request, responder);
	world.settle().await;
	assert_eq!(
		world.identity("a.txt"),
		Some(PlaceholderIdentity::File {
			stable: stable_of(&file),
			version
		}),
		"the newer remote version is placed again"
	);
	assert!(
		world.drive.trashed().is_empty(),
		"the newer remote version stays"
	);
	assert!(
		world
			.reports()
			.iter()
			.any(|report| matches!(report, LocalReport::RemoteOutlivedDelete { .. }))
	);
}

#[tokio::test(start_paused = true)]
async fn a_rescan_reconciles_the_populated_directories() {
	let world = unstarted().started().await;
	world.drive.quiet_signals(true);
	let file = world.drive.add_remote_file(&root_dir(), "a.txt", b"x");
	world.local.lose_watch_events();
	world.run(Duration::from_secs(5)).await;
	assert_eq!(world.identity("a.txt"), Some(identity_of(&file)));
}

// ---- the applier's other branches ----

fn parked(world: &World, path: &str, failure: Failure) -> bool {
	world.reports().iter().any(|report| {
		matches!(report, LocalReport::Parked { path: at, failure: why, .. }
			if *at == root_path(path) && *why == failure)
	})
}

#[tokio::test(start_paused = true)]
async fn a_placeholder_whose_remote_name_no_longer_decrypts_stays() {
	let world = unstarted();
	let (file, identity) = world.placed(&root_dir(), "a.txt", b"x", true);
	let world = world.started().await;
	world.drive.remote_garble(stable_of(&file));
	world.settle().await;
	assert_eq!(world.identity("a.txt"), Some(identity));
	assert_eq!(world.state("a.txt").unwrap().on_disk(), 1);
	let skipped = lock(&world.reports.skipped).clone();
	assert!(skipped.iter().any(|(_, items)| items.iter().any(|item| {
		item.id() == file.stable_uuid && *item.reason() == SkipReason::Undecryptable
	})));
}

#[tokio::test(start_paused = true)]
async fn a_remote_rename_to_a_name_windows_cannot_hold_removes_the_placeholder_and_reports_it() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"x", false);
	let world = world.started().await;
	world.drive.remote_rename(stable_of(&file), "a:b.txt");
	world.settle().await;
	assert_eq!(world.local.entries(), Vec::new());
	assert_eq!(world.drive.writes(), Vec::new());
	let skipped = lock(&world.reports.skipped).clone();
	assert!(
		skipped
			.iter()
			.any(|(_, items)| items.iter().any(|item| item.id() == file.stable_uuid)),
		"{skipped:?}"
	);
}

#[tokio::test(start_paused = true)]
async fn a_trashed_directory_whose_clean_file_is_held_waits_for_the_holder() {
	let (world, uuid, _) = with_dir_and_file();
	let world = world.started().await;
	let reader = world
		.local
		.user_open_for_read(&root_path(r"d\a.txt"), false);
	world.drive.remote_trash(uuid);
	world.settle().await;
	assert!(
		world.local.entry(&root_path(r"d\a.txt")).is_some(),
		"not while held"
	);
	drop(reader);
	world.run(Duration::from_secs(90)).await;
	assert_eq!(world.local.entry(&root_path(r"d\a.txt")), None);
	assert_eq!(world.local.entry(&root_path("d")), None);
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_trashed_directory_holding_an_edit_and_a_held_clean_file_waits_for_the_holder() {
	let (world, uuid, _) = with_dir_and_file();
	world.placed(&dir_id(&uuid.to_string()), r"d\e.txt", b"e", true);
	let world = world.started().await;
	world
		.local
		.user_write(&root_path(r"d\e.txt"), b"mine")
		.unwrap();
	let reader = world
		.local
		.user_open_for_read(&root_path(r"d\a.txt"), false);
	world.drive.remote_trash(uuid);
	world.settle().await;
	assert!(
		world.local.entry(&root_path(r"d\a.txt")).is_some(),
		"not while held"
	);
	drop(reader);
	world.run(Duration::from_secs(120)).await;
	assert_eq!(
		world.local.entry(&root_path(r"d\a.txt")),
		None,
		"the clean file goes with its directory once its holder let go"
	);
	assert_eq!(
		world.local.entry(&root_path(r"d\e.txt")).unwrap().bytes,
		b"mine"
	);
}

#[tokio::test(start_paused = true)]
async fn a_remote_directory_rename_waits_for_a_job_under_it() {
	let (world, uuid, file) = with_dir_and_file();
	let world = world.started().await;
	let gate = world.drive.gate_uploads();
	world
		.local
		.user_write(&root_path(r"d\a.txt"), b"mine")
		.unwrap();
	world.run(Duration::from_secs(10)).await;
	world.drive.remote_rename(uuid, "e");
	world.run(Duration::from_secs(10)).await;
	assert_eq!(
		world.identity("d"),
		Some(PlaceholderIdentity::Dir { uuid }),
		"not while the upload under it runs"
	);
	gate.add_permits(1_000);
	world.settle().await;
	assert_eq!(world.local.entry(&root_path("d")), None);
	assert_eq!(
		world.local.entry(&root_path(r"e\a.txt")).unwrap().bytes,
		b"mine"
	);
	assert_eq!(world.drive.content(stable_of(&file)), b"mine");
}

#[tokio::test(start_paused = true)]
async fn a_file_placeholder_naming_a_remote_directory_is_parked() {
	let world = unstarted();
	let dir = world.drive.add_remote_dir(&root_dir(), "x");
	let identity = PlaceholderIdentity::File {
		stable: Uuid::parse_str(&dir.uuid).unwrap(),
		version: Uuid::new_v4(),
	};
	world.local.place_file(&root_path("a.txt"), identity, b"x");
	let world = world.started().await;
	world.settle().await;
	assert!(
		parked(&world, "a.txt", Failure::Internal),
		"{:?}",
		world.reports()
	);
	assert_eq!(world.identity("a.txt"), Some(identity));
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_placeholder_gone_unseen_has_the_root_walked_and_comes_back() {
	let world = unstarted();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"x", false);
	let world = world.started().await;
	world.local.vanish(&root_path("a.txt"));
	let version = world.drive.bump_head(stable_of(&file), b"newer");
	world.settle().await;
	assert_eq!(
		world.identity("a.txt"),
		Some(PlaceholderIdentity::File {
			stable: stable_of(&file),
			version
		})
	);
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_walk_that_cannot_read_the_root_keeps_the_placeholders_it_knew() {
	let world = unstarted();
	let (file, identity) = world.placed(&root_dir(), "a.txt", b"x", false);
	let world = world.started().await;
	world
		.local
		.fail_inspect(&RootPath::root(), Some(LocalError::Busy));
	// A lost event has the root walked.
	world.local.lose_watch_events();
	world.run(Duration::from_secs(5)).await;
	world.local.fail_inspect(&RootPath::root(), None);
	world.drive.remote_rename(stable_of(&file), "b.txt");
	world.settle().await;
	assert_eq!(world.identity("b.txt"), Some(identity));
	assert_eq!(world.identity("a.txt"), None);
}

#[tokio::test(start_paused = true)]
async fn a_remote_swap_of_two_hydrated_names_keeps_their_bytes() {
	let world = unstarted();
	let (a, a_identity) = world.placed(&root_dir(), "a.txt", b"aa", true);
	let (b, b_identity) = world.placed(&root_dir(), "b.txt", b"bbb", true);
	let world = world.started().await;
	world.drive.remote_rename(stable_of(&a), "tmp");
	world.drive.remote_rename(stable_of(&b), "a.txt");
	world.drive.remote_rename(stable_of(&a), "b.txt");
	world.settle().await;
	assert_eq!(world.identity("a.txt"), Some(b_identity));
	assert_eq!(world.identity("b.txt"), Some(a_identity));
	assert_eq!(world.state("a.txt").unwrap().on_disk(), 3);
	assert_eq!(world.state("b.txt").unwrap().on_disk(), 2);
	assert_eq!(world.local.entries().len(), 2);
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_remote_rename_onto_an_edited_placeholder_waits_for_its_upload() {
	let world = unstarted();
	let (a, a_identity) = world.placed(&root_dir(), "a.txt", b"a", true);
	let (b, _) = world.placed(&root_dir(), "b.txt", b"b", true);
	let world = world.started().await;
	world
		.local
		.user_write(&root_path("b.txt"), b"mine")
		.unwrap();
	world.drive.remote_rename(stable_of(&b), "c.txt");
	world.drive.remote_rename(stable_of(&a), "b.txt");
	world.run(Duration::from_secs(10 * 60)).await;
	assert_eq!(world.identity("b.txt"), Some(a_identity));
	assert_eq!(
		world.local.entry(&root_path("c.txt")).unwrap().bytes,
		b"mine"
	);
	assert_eq!(world.drive.content(stable_of(&b)), b"mine");
	assert_eq!(world.local.entries().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_failed_read_of_the_feed_is_retried() {
	let world = unstarted().started().await;
	world
		.drive
		.fail_next(DriveCall::Changes, CacheError::remote("the cache is busy"));
	let file = world.drive.add_remote_file(&root_dir(), "a.txt", b"x");
	// Well inside the tick: read again after the backoff, not on the next signal or tick.
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.identity("a.txt"), Some(identity_of(&file)));
}

// ---- atomic saves with the remote side on ----

/// How many delete and rename requests the engine answered.
fn answers(world: &World) -> usize {
	world
		.local
		.ops()
		.iter()
		.filter(|op| {
			matches!(
				op,
				LocalOp::DeleteAnswered { .. } | LocalOp::RenameAnswered { .. }
			)
		})
		.count()
}

/// Waits until the engine answered more requests than `before`.
async fn answered(world: &World, before: usize) {
	for _ in 0..60_000 {
		if answers(world) > before {
			return;
		}
		sleep(Duration::from_millis(1)).await;
	}
	panic!("a request was never answered");
}

/// The user renames the placeholder `from` to `to`, replacing what `to` holds, and the engine
/// answers.
async fn rename_placeholder(world: &World, from: &str, to: &str) {
	let before = answers(world);
	let (request, responder) = world
		.local
		.user_rename_replacing(&root_path(from), &root_path(to))
		.unwrap()
		.asked();
	world.engine.on_notify_rename(request, responder);
	answered(world, before).await;
	// The platform's completion, which came within milliseconds of every allowed rename in the VM.
	if world.local.entry(&root_path(to)).is_some() && world.local.entry(&root_path(from)).is_none()
	{
		world.engine.on_completion(Completed::Renamed {
			from: root_path(from),
			to: root_path(to),
		});
	}
}

/// The user renames `from` to `to`, replacing what `to` holds: an ordinary file at once, with no
/// request, a placeholder once the engine answered, as [`rename_placeholder`].
async fn rename_any(world: &World, from: &str, to: &str) {
	if world
		.local
		.entry(&root_path(from))
		.is_some_and(|entry| entry.identity.is_some())
	{
		rename_placeholder(world, from, to).await;
	} else {
		world
			.local
			.user_rename_replacing(&root_path(from), &root_path(to))
			.unwrap();
	}
}

/// The user deletes the placeholder `path`, and the engine answers.
async fn delete_placeholder(world: &World, path: &str) {
	let before = answers(world);
	let (request, responder) = world.local.user_delete(&root_path(path)).asked();
	world.engine.on_notify_delete(request, responder);
	answered(world, before).await;
}

/// `ReplaceFileW` reserves its backup name by creating that file and renames the old file over it,
/// which fails while any other handle on it is open (fifteenth round). A walk of the root while
/// the rename waits must not open it either.
#[tokio::test(start_paused = true)]
async fn a_walk_never_reads_a_pending_renames_target() {
	let world = unstarted_keeping_records();
	world.placed(&root_dir(), "replace.txt", b"x", true);
	let world = world.started().await;
	let reserved = root_path("replace.txt~RFc0bd647.TMP");
	world.local.user_create(&reserved, b"");
	world.local.fail_completion(&root_path("replace.txt"));
	let before = answers(&world);
	let (request, responder) = world
		.local
		.user_rename_replacing(&root_path("replace.txt"), &reserved)
		.unwrap()
		.asked();
	world.engine.on_notify_rename(request, responder);
	answered(&world, before).await;
	let read = world.local.inspections(&reserved);
	world.engine.on_local_event(LocalEvent::Rescan);
	world.run(Duration::from_secs(5)).await;
	assert_eq!(
		world.local.inspections(&reserved),
		read,
		"the rename's target is never read while it waits"
	);
	world.settle().await;
	assert!(world.reports().contains(&LocalReport::Undone {
		path: root_path("replace.txt"),
		op: UndoneOp::Rename,
	}));
}

/// An Office-style save of `name`, as the VM's saves loop makes it: the new bytes written to a
/// temporary file, `gap` later the old file renamed to `backup`, the temporary file renamed into
/// its name, the backup deleted, each step right after the last.
async fn office_save(world: &World, name: &str, backup: &str, bytes: &[u8], gap: Duration) {
	let temp = "~WRL0001.tmp";
	world.local.user_create(&root_path(temp), bytes);
	world.run(gap).await;
	rename_placeholder(world, name, backup).await;
	rename_any(world, temp, name).await;
	delete_placeholder(world, backup).await;
}

/// What every settled save must leave: one placeholder of each remote item in the root, the
/// drive's root holding exactly `names`, the file `stable` at `name` in sync with `bytes` on both
/// sides, and no request vetoed.
fn assert_saved(world: &World, name: &str, stable: Uuid, bytes: &[u8], names: &[&str]) {
	let mut placed: HashMap<Uuid, RootPath> = HashMap::new();
	for entry in world.local.entries() {
		if let Some(uuid) = entry.identity.and_then(super::populated_uuid)
			&& let Some(other) = placed.insert(uuid, entry.path.clone())
		{
			panic!("two placeholders of {uuid}: {other} and {}", entry.path);
		}
	}
	let mut local: Vec<String> = world
		.local
		.entries()
		.iter()
		.map(|entry| entry.path.to_string())
		.collect();
	local.sort_unstable();
	let mut expected: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
	expected.sort_unstable();
	assert_eq!(local, expected, "the sync root");
	assert_eq!(world.drive.names(&root_dir()), expected, "the drive");
	let entry = world.local.entry(&root_path(name)).expect("the saved file");
	assert!(entry.in_sync, "{entry:?}");
	assert_eq!(entry.bytes, bytes);
	let remote = world.remote_file(name);
	assert_eq!(stable_of(&remote), stable, "the file keeps its identity");
	assert_eq!(entry.identity, Some(identity_of(&remote)));
	assert_eq!(world.drive.content(stable), bytes);
	let vetoed: Vec<LocalReport> = world
		.reports()
		.into_iter()
		.filter(|report| matches!(report, LocalReport::Vetoed { .. }))
		.collect();
	assert_eq!(vetoed, Vec::new(), "no request vetoed");
}

impl World {
	/// The drive's file `name` in the root.
	fn remote_file(&self, name: &str) -> FfiFile {
		match self.drive.child(&root_dir(), name) {
			Some(FfiNonRootObject::File(file)) => file,
			other => panic!("{name} on the drive is {other:?}"),
		}
	}
}

/// The VM's ARM64 saves loop (`harness/results-1794684474`): the save's delete of the backup lets
/// the item go once the new file claims it, and a reconcile of the directory before the save's
/// upload took the item back from the backup name on the drive placed it there again, a second
/// placeholder of one item, which the upload's end then detached and uploaded as a new file.
#[tokio::test(start_paused = true)]
async fn an_office_save_never_places_the_file_again_at_its_backup_name() {
	let world = unstarted_keeping_records();
	let (file, _) = world.placed(&root_dir(), "office.txt", b"one", true);
	let world = world.started().await;
	let stable = stable_of(&file);
	office_save(
		&world,
		"office.txt",
		"office.txt.bak",
		b"two",
		Duration::ZERO,
	)
	.await;
	// The save's upload holds before it takes the file back from its backup name on the drive.
	let gate = world.drive.gate_calls(DriveCall::Refresh);
	world.run(Duration::from_secs(6)).await;
	// Another device's change in the directory has it reconciled meanwhile.
	let other = world.drive.add_remote_file(&root_dir(), "other.txt", b"y");
	world.run(Duration::from_secs(5)).await;
	gate.add_permits(1_000);
	world.settle().await;
	world.settle().await;
	assert_saved(
		&world,
		"office.txt",
		stable,
		b"two",
		&["office.txt", "other.txt"],
	);
	assert_eq!(world.identity("other.txt"), Some(identity_of(&other)));
	assert_eq!(world.drive.trashed(), Vec::new());
}

/// The paths of the placeholders of the item `uuid` in the root, sorted.
fn placeholders_of(world: &World, uuid: Uuid) -> Vec<String> {
	let mut paths: Vec<String> = world
		.local
		.entries()
		.into_iter()
		.filter(|entry| entry.identity.and_then(super::populated_uuid) == Some(uuid))
		.map(|entry| entry.path.to_string())
		.collect();
	paths.sort_unstable();
	paths
}

/// How many times the remote side moved a placeholder.
fn remote_moves(world: &World) -> usize {
	world
		.remote()
		.iter()
		.filter(|applied| matches!(applied, RemoteApplied::Moved { .. }))
		.count()
}

/// The ARM64 saves loop again: two placeholders of one item, which the remote side then kept moving
/// onto the item's name in turn, each moving the other aside under a `.old` name, hundreds of times.
/// A placeholder of an item whose drive name another placeholder of the same item holds is the
/// extra one: clean, it is removed, whatever version it stands for.
#[tokio::test(start_paused = true)]
async fn a_second_clean_placeholder_of_an_item_is_removed_not_swapped_with_the_first() {
	for older in [false, true] {
		let world = unstarted_keeping_records();
		let (file, identity) = world.placed(&root_dir(), "office.txt", b"one", true);
		let stable = stable_of(&file);
		let extra = if older {
			PlaceholderIdentity::File {
				stable,
				version: Uuid::new_v4(),
			}
		} else {
			identity
		};
		world
			.local
			.place_file_with(&root_path("office.old.txt"), extra, b"one", true);
		let world = world.started().await;
		world.settle().await;
		assert_eq!(
			placeholders_of(&world, stable),
			["office.txt"],
			"older {older}"
		);
		assert_eq!(world.identity("office.txt"), Some(identity));
		assert_eq!(remote_moves(&world), 0, "{:?}", world.remote());
		assert_eq!(world.drive.writes(), Vec::new());
		assert!(world.remote().contains(&RemoteApplied::Removed {
			path: root_path("office.old.txt")
		}));
	}
}

/// An extra placeholder not all on disk that only lost its in-sync state, as a rename or a change
/// of attributes clears it, holds no edit: a write hydrates the whole file first. It is removed as
/// a clean one is, never uploaded as a copy of the item and reported as a conflict.
#[tokio::test(start_paused = true)]
async fn a_second_placeholder_of_an_item_with_no_byte_written_is_removed_not_kept_as_a_copy() {
	let world = unstarted_keeping_records();
	let (file, identity) = world.placed(&root_dir(), "office.txt", b"one", true);
	let stable = stable_of(&file);
	world
		.local
		.place_file_with(&root_path("office.old.txt"), identity, b"one", false);
	world.local.user_touch(&root_path("office.old.txt"));
	let world = world.started().await;
	world.settle().await;
	world.settle().await;
	assert_eq!(placeholders_of(&world, stable), ["office.txt"]);
	assert_eq!(world.drive.writes(), Vec::new());
	assert!(
		!world
			.reports()
			.iter()
			.any(|report| matches!(report, LocalReport::Conflict { .. })),
		"{:?}",
		world.reports()
	);
}

/// An extra placeholder of a directory goes with the clean placeholders in it: a directory holding
/// items cannot be removed on its own.
#[tokio::test(start_paused = true)]
async fn a_second_placeholder_of_a_directory_is_removed_with_what_it_holds() {
	let world = unstarted_keeping_records();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	let identity = PlaceholderIdentity::Dir { uuid };
	world.local.place_dir(&root_path("d"), identity);
	world.local.place_dir(&root_path("d.old"), identity);
	let (file, file_identity) = world.placed(&dir_id(&dir.uuid), r"d\x.txt", b"x", true);
	world
		.local
		.place_file_with(&root_path(r"d.old\x.txt"), file_identity, b"x", true);
	let world = world.started().await;
	world.settle().await;
	world.settle().await;
	assert_eq!(world.identity("d"), Some(identity));
	assert_eq!(
		world.local.entry(&root_path("d.old")),
		None,
		"{:?}",
		world.local.entries()
	);
	assert_eq!(placeholders_of(&world, stable_of(&file)), [r"d\x.txt"]);
	assert_eq!(world.drive.writes(), Vec::new());
}

/// The same with the extra placeholder holding an edit: it holds bytes the drive does not have, so
/// it is kept, as a file of its own, and reported as a conflict; the item keeps its name and bytes.
#[tokio::test(start_paused = true)]
async fn a_second_placeholder_of_an_item_holding_an_edit_is_kept_as_a_file_of_its_own() {
	let world = unstarted_keeping_records();
	let (file, identity) = world.placed(&root_dir(), "office.txt", b"one", true);
	let stable = stable_of(&file);
	world
		.local
		.place_file_with(&root_path("office.old.txt"), identity, b"one", true);
	world
		.local
		.user_write(&root_path("office.old.txt"), b"mine")
		.unwrap();
	let world = world.started().await;
	world.settle().await;
	assert_eq!(placeholders_of(&world, stable), ["office.txt"]);
	assert_eq!(world.drive.content(stable), b"one");
	assert_eq!(
		world.local.entry(&root_path("office.txt")).unwrap().bytes,
		b"one"
	);
	let kept = world.remote_file("office.old.txt");
	assert_ne!(stable_of(&kept), stable);
	assert_eq!(world.drive.content(stable_of(&kept)), b"mine");
	assert_eq!(world.identity("office.old.txt"), Some(identity_of(&kept)));
	assert!(
		world.reports().contains(&LocalReport::Conflict {
			path: root_path("office.txt"),
			kept_as: root_path("office.old.txt"),
			kind: ConflictKind::EditVsEdit,
		}),
		"{:?}",
		world.reports()
	);
	assert_eq!(remote_moves(&world), 0);
}

/// Where the loop ended up: the drive held another item at the save's backup name, with its clean
/// placeholder there, and every later save's rename onto it was vetoed. A rename onto the name of
/// a clean placeholder replaces it locally, as the user asked, so the drive's item there goes to
/// the trash, where its bytes stay, and the save goes through.
#[tokio::test(start_paused = true)]
async fn a_save_renaming_onto_a_clean_placeholder_of_another_item_replaces_it() {
	let world = unstarted_keeping_records();
	let (file, _) = world.placed(&root_dir(), "office.txt", b"one", true);
	let (stale, _) = world.placed(&root_dir(), "office.txt.bak", b"old", true);
	let world = world.started().await;
	let stable = stable_of(&file);
	office_save(
		&world,
		"office.txt",
		"office.txt.bak",
		b"two",
		Duration::ZERO,
	)
	.await;
	world.settle().await;
	world.settle().await;
	assert_saved(&world, "office.txt", stable, b"two", &["office.txt"]);
	let trashed: Vec<String> = world
		.drive
		.trashed()
		.iter()
		.map(|item| jobs::object_uuid(item).to_owned())
		.collect();
	assert_eq!(trashed, std::slice::from_ref(&stale.stable_uuid));
	assert_eq!(
		world.drive.content(stable_of(&stale)),
		b"old",
		"its bytes stay"
	);
}

/// The rename onto another item's name is still vetoed when the placeholder there holds an edit,
/// as replacing it would lose the edit on the drive.
#[tokio::test(start_paused = true)]
async fn a_rename_onto_another_items_edited_placeholder_is_still_vetoed() {
	let world = unstarted_keeping_records();
	world.placed(&root_dir(), "a.txt", b"one", true);
	world.placed(&root_dir(), "b.txt", b"old", true);
	let world = world.started().await;
	world
		.local
		.user_write(&root_path("b.txt"), b"mine")
		.unwrap();
	rename_placeholder(&world, "a.txt", "b.txt").await;
	assert!(world.reports().contains(&LocalReport::Vetoed {
		path: root_path("a.txt"),
		op: VetoedOp::Rename,
		failure: Failure::Transient,
	}));
	assert_eq!(world.drive.trashed(), Vec::new());
}

/// Nor is it replaced when the drive holds a newer version than the placeholder there, one the
/// user never saw: trashing it would replace bytes nobody looked at.
#[tokio::test(start_paused = true)]
async fn a_rename_onto_a_placeholder_of_an_older_version_is_vetoed() {
	let world = unstarted_keeping_records();
	world.placed(&root_dir(), "a.txt", b"one", true);
	let (other, _) = world.placed(&root_dir(), "b.txt", b"old", true);
	let world = world.started().await;
	// Uploaded elsewhere, its change not applied to the placeholder yet.
	world.drive.quiet_signals(true);
	world.drive.bump_head(stable_of(&other), b"newer");
	rename_placeholder(&world, "a.txt", "b.txt").await;
	world.drive.quiet_signals(false);
	assert!(world.reports().contains(&LocalReport::Vetoed {
		path: root_path("a.txt"),
		op: VetoedOp::Rename,
		failure: Failure::Transient,
	}));
	assert_eq!(world.drive.trashed(), Vec::new());
	assert_eq!(world.drive.content(stable_of(&other)), b"newer");
}

/// Nor when the cache holds an edit of the file there not yet on the server.
#[tokio::test(start_paused = true)]
async fn a_rename_onto_a_placeholder_of_a_file_with_an_upload_pending_is_vetoed() {
	let world = unstarted_keeping_records();
	world.placed(&root_dir(), "a.txt", b"one", true);
	let (other, _) = world.placed(&root_dir(), "b.txt", b"old", true);
	let world = world.started().await;
	world.drive.set_upload_pending(stable_of(&other), true);
	rename_placeholder(&world, "a.txt", "b.txt").await;
	assert!(world.reports().contains(&LocalReport::Vetoed {
		path: root_path("a.txt"),
		op: VetoedOp::Rename,
		failure: Failure::Transient,
	}));
	assert_eq!(world.drive.trashed(), Vec::new());
}

/// A rename that replaced another file's clean placeholder and then did not happen locally is put
/// back, and the file moved out of its way gets its name back. When another file took that name
/// meanwhile, it gets a free name beside it instead, where it is placed again: never left under the
/// transit name, which nothing places, nor its placeholder renamed to it.
#[tokio::test(start_paused = true)]
async fn a_file_moved_out_of_a_put_back_renames_way_gets_a_free_name_when_its_own_is_taken() {
	let world = unstarted_keeping_records();
	world.placed(&root_dir(), "a.txt", b"one", true);
	let (other, _) = world.placed(&root_dir(), "b.txt", b"old", true);
	let world = world.started().await;
	// The platform never completes the rename it was allowed.
	world.local.fail_completion(&root_path("a.txt"));
	let (request, responder) = world
		.local
		.user_rename_replacing(&root_path("a.txt"), &root_path("b.txt"))
		.unwrap()
		.asked();
	world.engine.on_notify_rename(request, responder);
	world.run(Duration::from_secs(1)).await;
	let taker = world
		.drive
		.add_remote_file(&root_dir(), "b.txt", b"another");
	world.settle().await;
	world.settle().await;
	let names = world.drive.names(&root_dir());
	assert!(
		!names.iter().any(|name| name.starts_with(".filen-upload")),
		"nothing left under a transit name: {names:?}"
	);
	assert_eq!(
		world.drive.content(stable_of(&other)),
		b"old",
		"its bytes are kept"
	);
	let kept = world
		.local
		.entries()
		.into_iter()
		.find(|entry| {
			matches!(entry.identity, Some(PlaceholderIdentity::File { stable, .. })
				if stable == stable_of(&other))
		})
		.expect("the moved file has its placeholder");
	let kept_name = kept.path.name().unwrap().as_str().to_owned();
	assert!(
		!kept_name.starts_with(".filen-upload") && kept_name != "b.txt",
		"{kept_name}"
	);
	assert!(names.contains(&kept_name), "{names:?}");
	assert!(
		world
			.drive
			.child(&root_dir(), "b.txt")
			.is_some_and(|child| {
				matches!(child, FfiNonRootObject::File(file) if file.stable_uuid == taker.stable_uuid)
			})
	);
	assert!(world.local.entry(&root_path("a.txt")).is_some(), "put back");
}

/// A clean placeholder whose file is under a transit name on the drive, on its way to its name, is
/// left where it is: a name nothing places is never one a placeholder takes.
#[tokio::test(start_paused = true)]
async fn a_placeholder_never_follows_its_file_to_a_transit_name() {
	let world = unstarted();
	let (file, identity) = world.placed(&root_dir(), "a.txt", b"xyz", true);
	let world = world.started().await;
	world.drive.remote_rename(
		stable_of(&file),
		&crate::names::transit_name(stable_of(&file)),
	);
	world.settle().await;
	assert_eq!(world.identity("a.txt"), Some(identity));
	assert_eq!(world.state("a.txt").unwrap().on_disk(), 3);
	assert_eq!(
		world.local.entries().len(),
		1,
		"{:?}",
		world.local.entries()
	);
}

/// A file the local side ignores, left where a placeholder once was, is no save taking that
/// placeholder's file over: the file's remote moves are still followed.
#[tokio::test(start_paused = true)]
async fn an_ignored_file_at_a_placeholders_old_path_holds_back_none_of_its_moves() {
	let world = unstarted_keeping_records();
	let (file, identity) = world.placed(&root_dir(), "doc.swp", b"xyz", true);
	let world = world.started().await;
	rename_placeholder(&world, "doc.swp", "doc.txt").await;
	world.settle().await;
	world.local.user_create(&root_path("doc.swp"), b"swap");
	world.settle().await;
	world.drive.remote_rename(stable_of(&file), "other.txt");
	world.settle().await;
	assert_eq!(world.local.entry(&root_path("doc.txt")), None);
	assert!(
		matches!(world.identity("other.txt"), Some(PlaceholderIdentity::File { stable, .. })
		if Some(stable) == match identity {
			PlaceholderIdentity::File { stable, .. } => Some(stable),
			_ => None,
		}),
		"{:?}",
		world.local.entries()
	);
	assert!(world.local.entry(&root_path("doc.swp")).is_some());
}

/// The user writes `bytes` to the new file `temp`, which the engine uploads as a file of its own
/// and makes its placeholder before the save goes on: a user's process slow enough to rename it
/// after that. Its stable id.
async fn uploaded_temp(world: &World, temp: &str, bytes: &[u8]) -> Uuid {
	world.local.user_create(&root_path(temp), bytes);
	world.settle().await;
	match world.identity(temp) {
		Some(PlaceholderIdentity::File { stable, .. }) => stable,
		other => panic!("{temp} was not uploaded: {other:?}"),
	}
}

/// The stable ids of the items in the drive's trash.
fn trashed_ids(world: &World) -> Vec<Uuid> {
	world
		.drive
		.trashed()
		.iter()
		.map(|item| Uuid::parse_str(jobs::object_uuid(item)).unwrap())
		.collect()
}

/// The VM's `L5b-save-move-y` (`harness/results-1794857243`): a save's temporary file uploaded as
/// a file of its own before the user's process renamed it over the file it saves. The file keeps
/// its identity and gets the bytes as one more version; the temporary upload goes to the trash.
#[tokio::test(start_paused = true)]
async fn a_temporary_file_uploaded_before_its_rename_over_the_file_is_a_new_version() {
	let world = unstarted_keeping_records();
	let (file, _) = world.placed(&root_dir(), "save.txt", b"one", true);
	let world = world.started().await;
	let stable = stable_of(&file);
	let temp = uploaded_temp(&world, "save.txt.tmp", b"two").await;
	rename_placeholder(&world, "save.txt.tmp", "save.txt").await;
	world.settle().await;
	world.settle().await;
	assert_saved(&world, "save.txt", stable, b"two", &["save.txt"]);
	assert_eq!(world.drive.history(stable), [b"two".to_vec()]);
	assert_eq!(trashed_ids(&world), [temp], "only the temporary upload");
}

/// The same over a placeholder holding an edit not uploaded yet: the edit reaches the drive first,
/// as a version of its own, and the save after it, with no request vetoed.
#[tokio::test(start_paused = true)]
async fn a_temporary_file_uploaded_before_its_rename_over_an_unsent_edit_keeps_both_versions() {
	let world = unstarted_keeping_records();
	let (file, _) = world.placed(&root_dir(), "save.txt", b"one", true);
	let world = world.started().await;
	let stable = stable_of(&file);
	let temp = uploaded_temp(&world, "save.txt.tmp", b"two").await;
	world
		.local
		.user_write(&root_path("save.txt"), b"edit")
		.unwrap();
	rename_placeholder(&world, "save.txt.tmp", "save.txt").await;
	world.settle().await;
	world.settle().await;
	assert_saved(&world, "save.txt", stable, b"two", &["save.txt"]);
	assert_eq!(
		world.drive.history(stable),
		[b"edit".to_vec(), b"two".to_vec()]
	);
	assert_eq!(trashed_ids(&world), [temp]);
}

/// A save's temporary file whose upload is still on its way when the user's process renames it
/// over the file, which raises no request: the file gets the bytes as a version, and the upload,
/// which never got a name, goes to the trash.
#[tokio::test(start_paused = true)]
async fn a_temporary_file_renamed_over_the_file_while_it_uploads_is_a_new_version() {
	let world = unstarted_keeping_records();
	let (file, _) = world.placed(&root_dir(), "save.txt", b"one", true);
	let world = world.started().await;
	let stable = stable_of(&file);
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("save.txt.tmp"), b"two");
	world.run(Duration::from_secs(10)).await;
	assert!(
		world
			.drive
			.writes()
			.iter()
			.any(|op| matches!(op, DriveOp::Upload { .. })),
		"the upload is on its way"
	);
	world
		.local
		.user_replace(&root_path("save.txt.tmp"), &root_path("save.txt"));
	world.run(Duration::from_secs(2)).await;
	gate.add_permits(1_000);
	world.settle().await;
	world.settle().await;
	assert_saved(&world, "save.txt", stable, b"two", &["save.txt"]);
	assert_eq!(world.drive.history(stable), [b"two".to_vec()]);
	assert_eq!(
		trashed_ids(&world).len(),
		1,
		"the upload: {:?}",
		world.drive.trashed()
	);
	assert!(!trashed_ids(&world).contains(&stable));
}

/// A save's temporary file uploaded and still ordinary, as a writer held it when its upload ended,
/// renamed over the file: the same.
#[tokio::test(start_paused = true)]
async fn a_temporary_file_uploaded_and_not_converted_renamed_over_the_file_is_a_new_version() {
	let world = unstarted_keeping_records();
	let (file, _) = world.placed(&root_dir(), "save.txt", b"one", true);
	let world = world.started().await;
	let stable = stable_of(&file);
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("save.txt.tmp"), b"two");
	world.run(Duration::from_secs(10)).await;
	let writer = world.local.user_open_for_write(&root_path("save.txt.tmp"));
	gate.add_permits(1_000);
	world.run(Duration::from_secs(1)).await;
	assert!(
		world
			.local
			.entry(&root_path("save.txt.tmp"))
			.unwrap()
			.identity
			.is_none(),
		"still ordinary"
	);
	drop(writer);
	world
		.local
		.user_replace(&root_path("save.txt.tmp"), &root_path("save.txt"));
	world.settle().await;
	world.settle().await;
	assert_saved(&world, "save.txt", stable, b"two", &["save.txt"]);
	assert_eq!(world.drive.history(stable), [b"two".to_vec()]);
	assert_eq!(
		trashed_ids(&world).len(),
		1,
		"the upload: {:?}",
		world.drive.trashed()
	);
	assert!(!trashed_ids(&world).contains(&stable));
}

/// An Office-style save whose temporary file was uploaded before the save renamed the old file to
/// its backup name and the temporary file into its name: the file keeps its identity.
#[tokio::test(start_paused = true)]
async fn an_office_save_whose_temporary_file_was_uploaded_first_is_a_new_version() {
	let world = unstarted_keeping_records();
	let (file, _) = world.placed(&root_dir(), "office.txt", b"one", true);
	let world = world.started().await;
	let stable = stable_of(&file);
	let temp = uploaded_temp(&world, "~WRL0001.tmp", b"two").await;
	rename_placeholder(&world, "office.txt", "office.txt.bak").await;
	rename_placeholder(&world, "~WRL0001.tmp", "office.txt").await;
	delete_placeholder(&world, "office.txt.bak").await;
	world.settle().await;
	world.settle().await;
	assert_saved(&world, "office.txt", stable, b"two", &["office.txt"]);
	assert_eq!(world.drive.history(stable), [b"two".to_vec()]);
	assert_eq!(trashed_ids(&world), [temp]);
}

/// A new file renamed over an ordinary file the drive never had: no file's save, so the new file
/// keeps its identity under the name, and nothing else reaches the drive.
#[tokio::test(start_paused = true)]
async fn an_uploaded_file_renamed_over_a_new_ordinary_file_keeps_its_identity() {
	let world = unstarted_keeping_records().started().await;
	let temp = uploaded_temp(&world, "a.tmp", b"two").await;
	world.local.user_create(&root_path("a.txt"), b"plain");
	rename_placeholder(&world, "a.tmp", "a.txt").await;
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file("a.txt");
	assert_eq!(stable_of(&remote), temp);
	assert_eq!(world.drive.names(&root_dir()), ["a.txt"]);
	assert_eq!(world.identity("a.txt"), Some(identity_of(&remote)));
	assert!(world.local.entry(&root_path("a.txt")).unwrap().in_sync);
	assert_eq!(world.drive.trashed(), Vec::new());
	assert!(
		!world
			.reports()
			.iter()
			.any(|report| matches!(report, LocalReport::Vetoed { .. })),
		"{:?}",
		world.reports()
	);
}

/// A file the engine uploaded long ago, renamed over another file's clean placeholder: the user
/// moving one file over another, never a save. The file moved keeps its identity, and the file it
/// replaced goes to the trash, as before.
#[tokio::test(start_paused = true)]
async fn an_old_file_renamed_over_another_file_replaces_it() {
	let world = unstarted_keeping_records();
	let (file, _) = world.placed(&root_dir(), "a.txt", b"one", true);
	let world = world.started().await;
	let moved = uploaded_temp(&world, "b.txt", b"two").await;
	world.run(Duration::from_secs(120)).await;
	rename_placeholder(&world, "b.txt", "a.txt").await;
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file("a.txt");
	assert_eq!(stable_of(&remote), moved);
	assert_eq!(world.drive.content(moved), b"two");
	assert_eq!(world.drive.names(&root_dir()), ["a.txt"]);
	assert_eq!(world.identity("a.txt"), Some(identity_of(&remote)));
	assert_eq!(trashed_ids(&world), [stable_of(&file)]);
	assert_eq!(
		world.drive.content(stable_of(&file)),
		b"one",
		"its bytes stay"
	);
}

/// The saves the loop below makes, in turn.
#[derive(Debug, Clone, Copy)]
enum Save {
	/// Office's: the old file renamed to a backup name, the new one renamed into its name, the
	/// backup deleted.
	Office,
	/// `ReplaceFileW`'s: a backup name reserved as a new empty file, the old file renamed over it,
	/// the replacement renamed into its name, the backup deleted.
	ReplaceFile,
	/// A new file renamed over the old one, which raises no request.
	RenameOver,
	/// The new bytes written over the old ones in place.
	Overwrite,
	/// The old file deleted and a new one created at its name.
	DeleteAndCreate,
}

impl Save {
	const ALL: [Self; 5] = [
		Self::Office,
		Self::ReplaceFile,
		Self::RenameOver,
		Self::Overwrite,
		Self::DeleteAndCreate,
	];

	/// The file this kind of save saves.
	fn name(self) -> &'static str {
		match self {
			Self::Office => "office.txt",
			Self::ReplaceFile => "replace.txt",
			Self::RenameOver => "save.txt",
			Self::Overwrite => "plain.txt",
			Self::DeleteAndCreate => "deleted.txt",
		}
	}

	/// Saves `bytes` as this kind of save does; `n` tells its temporary names apart. A save through
	/// a temporary file renames it `gap` after writing it: long enough, and the engine uploads it
	/// as a file of its own first.
	async fn run(self, world: &World, n: u32, bytes: &[u8], gap: Duration) {
		let name = self.name();
		match self {
			Self::Office => office_save(world, name, "office.txt.bak", bytes, gap).await,
			Self::ReplaceFile => {
				let reserved = format!("replace.txt~RF{n:x}.TMP");
				world.local.user_create(&root_path("replace.new"), bytes);
				world.run(gap).await;
				world.local.user_create(&root_path(&reserved), b"");
				rename_placeholder(world, name, &reserved).await;
				rename_any(world, "replace.new", name).await;
				delete_placeholder(world, &reserved).await;
			}
			Self::RenameOver => {
				world.local.user_create(&root_path("save.txt.tmp"), bytes);
				world.run(gap).await;
				rename_any(world, "save.txt.tmp", name).await;
			}
			Self::Overwrite => world.local.user_write(&root_path(name), bytes).unwrap(),
			Self::DeleteAndCreate => {
				delete_placeholder(world, name).await;
				assert_eq!(
					world.local.entry(&root_path(name)),
					None,
					"deleted: {:?}",
					world.reports()
				);
				world.local.user_create(&root_path(name), bytes);
			}
		}
	}
}

/// A small deterministic generator, so a failing run can be replayed from its seed.
struct Dice(u64);

impl Dice {
	fn below(&mut self, n: u64) -> u64 {
		self.0 ^= self.0 << 13;
		self.0 ^= self.0 >> 7;
		self.0 ^= self.0 << 17;
		self.0 % n
	}
}

/// Every save kind over and over, as fast as a user's process makes them or with its temporary file
/// written up to six seconds before the save goes on, so that it is uploaded first, while the
/// drive's echoes
/// of the provider's own changes come as each write returns, or late, after the save's next steps
/// or another device's change, and again; while another device's edit in the directory has it
/// read and reconciled, a walk runs, and the save's upload is held. Each save is checked once
/// settled: one placeholder per item, nothing left at a backup or temporary name on
/// either side, the file keeping its identity with one more version holding the save's bytes and
/// never in the trash (a temporary file uploaded first goes there), and no request vetoed.
#[tokio::test(start_paused = true)]
async fn atomic_saves_at_any_echo_timing_keep_one_item_per_file() {
	const ROUNDS: u32 = 40;
	let world = unstarted_keeping_records();
	let mut files = Vec::new();
	for save in Save::ALL {
		let (file, _) = world.placed(&root_dir(), save.name(), b"0", true);
		files.push((save, stable_of(&file), Vec::<Vec<u8>>::new()));
	}
	let (other, _) = world.placed(&root_dir(), "other.txt", b"other", false);
	let world = world.started().await;
	let names: Vec<&str> = Save::ALL
		.iter()
		.map(|save| save.name())
		.chain(["other.txt"])
		.collect();
	let mut dice = Dice(0x5eed_0f0f_1234_5678);
	for round in 0..ROUNDS {
		for (save, stable, saved) in &mut files {
			let n = round * 8 + u32::try_from(saved.len()).unwrap();
			let bytes = format!("{save:?} {round}").into_bytes();
			// The echoes of the save's own writes held back, told late, maybe twice.
			let late = dice.below(2) == 0;
			world.drive.hold_echoes(late);
			let gap = Duration::from_millis(500 * dice.below(13));
			save.run(&world, n, &bytes, gap).await;
			// Armed once the requests are answered, as an answer waits for the drive too.
			let hold = dice.below(3) == 0;
			let gate = hold.then(|| world.drive.gate_calls(DriveCall::Refresh));
			saved.push(bytes.clone());
			for _ in 0..dice.below(4) {
				world
					.run(Duration::from_millis(500 * (dice.below(8) + 1)))
					.await;
				match dice.below(3) {
					0 => {
						world.drive.bump_head(stable_of(&other), b"other");
					}
					1 => world.engine.on_local_event(LocalEvent::Rescan),
					_ if late && world.drive.release_echoes() && dice.below(2) == 0 => {
						world.drive.echo_again();
					}
					_ => {}
				}
			}
			world.drive.hold_echoes(false);
			if late && world.drive.release_echoes() && dice.below(2) == 0 {
				world.run(Duration::from_millis(500)).await;
				world.drive.echo_again();
			}
			if let Some(gate) = gate {
				world.run(Duration::from_secs(dice.below(6))).await;
				world.drive.stop_gating(DriveCall::Refresh);
				gate.add_permits(1_000);
			}
			world.settle().await;
			let context = format!("round {round}, {save:?}");
			eprintln!("checking {context}");
			assert_saved(&world, save.name(), *stable, &bytes, &names);
			assert_eq!(
				world.drive.history(*stable),
				*saved,
				"{context}: one version per save"
			);
			assert!(
				!trashed_ids(&world).contains(stable),
				"{context}: only a temporary file goes to the trash"
			);
		}
	}
	assert!(
		!world.drive.trashed().is_empty(),
		"some saves' temporary files were uploaded before their rename"
	);
}
