use filen_mobile_native_cache::{
	CacheError,
	ffi::{FfiFile, FfiNonRootObject},
};
use filen_types::fs::Uuid;
use tempfile::TempDir;

use super::*;
use crate::{
	failure::Failure,
	identity::{IdentityBlob, PlaceholderIdentity},
	os::LocalError,
	populate::SkippedItem,
	test_support::{
		DriveCall, DriveOp, FakeDrive, FakeLocal, LocalOp, UserChange, ffi_file, root_path,
	},
};

const ROOT_UUID: Uuid = Uuid::from_u128(0x100);
const ROOT: PlaceholderIdentity = PlaceholderIdentity::Root { root: ROOT_UUID };

/// Records every local report.
#[derive(Default)]
struct Reports(Mutex<Vec<LocalReport>>);

impl CloudFilesCallback for Reports {
	fn on_skipped_items(&self, _directory: PlaceholderIdentity, _items: &[SkippedItem]) {}

	fn on_local_report(&self, report: &LocalReport) {
		lock(&self.0).push(report.clone());
	}
}

/// A drive, a sync root and an engine carrying the root's changes to the drive.
struct World {
	drive: Arc<FakeDrive>,
	local: FakeLocal,
	engine: LocalEngine<FakeDrive, FakeLocal>,
	reports: Arc<Reports>,
	/// Where the engine keeps its records, when it keeps any.
	// Reason for the Arc: a restarted world shares it with the one it replaces.
	records: Option<Arc<TempDir>>,
}

/// An engine over `drive` and `local`, reporting to `reports`, keeping its records in `records`.
fn engine(
	drive: &Arc<FakeDrive>,
	local: &FakeLocal,
	reports: &Arc<Reports>,
	config: LocalConfig,
	records: Option<&TempDir>,
) -> LocalEngine<FakeDrive, FakeLocal> {
	let (drive, local, callback) = (
		Arc::clone(drive),
		Arc::new(local.clone()),
		Arc::clone(reports) as Arc<dyn CloudFilesCallback>,
	);
	match records {
		Some(dir) => LocalEngine::new_keeping_records(
			drive,
			local,
			Handle::current(),
			config,
			callback,
			dir.path().to_path_buf(),
		),
		None => LocalEngine::new(drive, local, Handle::current(), config, callback),
	}
}

/// The cache id of the root directory.
fn root_dir() -> String {
	ROOT_UUID.to_string()
}

/// A world with nothing in it, its engine not started.
fn unstarted() -> World {
	unstarted_with(LocalConfig::default())
}

/// A world with nothing in it whose engine runs on `config`, not started.
fn unstarted_with(config: LocalConfig) -> World {
	let drive = Arc::new(FakeDrive::new(ROOT_UUID));
	drive.list(&root_dir(), Vec::new());
	let local = FakeLocal::new(ROOT);
	drive.link_local(&local);
	let reports = Arc::new(Reports::default());
	let engine = engine(&drive, &local, &reports, config, None);
	World {
		drive,
		local,
		engine,
		reports,
		records: None,
	}
}

/// A world with nothing in it whose engine keeps its records in a directory of its own, its root
/// populated, so the remote side places what the drive holds; not started.
fn unstarted_keeping_records() -> World {
	let world = unstarted();
	world.local.set_populated(&RootPath::root(), true);
	let records = Arc::new(TempDir::new().expect("a temporary directory"));
	let engine = engine(
		&world.drive,
		&world.local,
		&world.reports,
		LocalConfig::default(),
		Some(&records),
	);
	World {
		engine,
		records: Some(records),
		..world
	}
}

/// A started world with nothing in it.
async fn world() -> World {
	let world = unstarted();
	world.engine.start();
	world.run(Duration::from_secs(5)).await;
	world
}

impl World {
	/// Stops the engine, as the provider exiting does, and starts a new one on the same drive and
	/// root, as the next start does: nothing of the old engine's memory carries over.
	async fn restarted(self) -> Self {
		self.engine.shutdown().await;
		self.started_again()
	}

	/// Kills the engine, as the desktop app killing the helper does: nothing it would do on a
	/// stop is done.
	fn killed(self) -> Stopped {
		drop(self.engine);
		Stopped {
			drive: self.drive,
			local: self.local,
			reports: self.reports,
			records: self.records,
		}
	}

	/// Starts a new engine on the same drive, root and records, as the next start does.
	fn started_again(self) -> Self {
		let engine = engine(
			&self.drive,
			&self.local,
			&self.reports,
			LocalConfig::default(),
			self.records.as_deref(),
		);
		engine.start();
		Self { engine, ..self }
	}

	/// Hands the engine every watcher event since the last call.
	fn pump(&self) {
		for event in self.local.take_watch_events() {
			self.engine.on_local_event(event);
		}
	}

	/// Lets `span` of time pass on the paused clock, handing over the watcher's events every
	/// second, as the watcher would as they come.
	async fn run(&self, span: Duration) {
		let deadline = Instant::now() + span;
		while Instant::now() < deadline {
			self.pump();
			sleep(Duration::from_secs(1)).await;
		}
		self.pump();
	}

	/// Lets half a minute pass.
	async fn settle(&self) {
		self.run(Duration::from_secs(30)).await;
	}

	fn reports(&self) -> Vec<LocalReport> {
		lock(&self.reports.0).clone()
	}

	/// The remote child `name` of the directory `dir` (a cache id), which must be a file.
	fn remote_file(&self, dir: &str, name: &str) -> FfiFile {
		match self.drive.child(dir, name) {
			Some(FfiNonRootObject::File(file)) => file,
			other => panic!("{name} is not a remote file: {other:?}"),
		}
	}

	/// A file placeholder for a new remote file `name` holding `bytes` in the root, hydrated.
	fn placed_file(&self, name: &str, bytes: &[u8]) -> (Uuid, Uuid) {
		let file = self.drive.add_remote_file(&root_dir(), name, bytes);
		let (stable, version) = jobs::file_ids(&file).unwrap();
		self.local.place_file(
			&root_path(name),
			PlaceholderIdentity::File { stable, version },
			bytes,
		);
		(stable, version)
	}

	/// A file placeholder for the file `stable` (version `version`) holding `bytes` in the root,
	/// hydrated, which the drive lists with no declared hash, as a file an older client uploaded:
	/// the root's only remote item.
	fn placed_hashless_file(&self, name: &str, bytes: &[u8], stable: Uuid, version: Uuid) {
		self.drive.add_file(stable, version, bytes);
		let mut listed = ffi_file(stable, version, name, i64::try_from(bytes.len()).unwrap());
		listed.parent = root_dir();
		self.drive
			.list(&root_dir(), vec![FfiNonRootObject::File(listed)]);
		self.local.place_file(
			&root_path(name),
			PlaceholderIdentity::File { stable, version },
			bytes,
		);
	}

	/// How many times the drive was asked for the file `stable` afresh: once in each look at an
	/// edit of it.
	fn refreshes_of(&self, stable: Uuid) -> usize {
		let id = format!("stable/{stable}");
		self.drive
			.ops()
			.iter()
			.filter(|op| matches!(op, DriveOp::Refresh(of) if *of == id))
			.count()
	}

	fn parked(&self, path: &str) -> usize {
		self.reports()
			.iter()
			.filter(
				|report| matches!(report, LocalReport::Parked { path: at, .. } if *at == root_path(path)),
			)
			.count()
	}

	/// Asks the engine to delete the placeholder `path`, as the platform would.
	fn delete(&self, path: &str) {
		let (request, responder) = self.local.user_delete(&root_path(path)).asked();
		self.engine.on_notify_delete(request, responder);
	}

	/// Asks the engine to rename the placeholder `from` to `to`, as the platform would.
	fn rename(&self, from: &str, to: &str) {
		let (request, responder) = self
			.local
			.user_rename(&root_path(from), &root_path(to))
			.unwrap()
			.asked();
		self.engine.on_notify_rename(request, responder);
	}

	fn identity(&self, path: &str) -> Option<PlaceholderIdentity> {
		self.local.entry(&root_path(path))?.identity
	}

	fn in_sync(&self, path: &str) -> bool {
		self.local
			.entry(&root_path(path))
			.is_some_and(|entry| entry.in_sync)
	}

	fn delete_answers(&self) -> Vec<Result<(), Failure>> {
		self.local
			.ops()
			.into_iter()
			.filter_map(|op| match op {
				LocalOp::DeleteAnswered { answer, .. } => Some(answer),
				_ => None,
			})
			.collect()
	}

	fn rename_answers(&self) -> Vec<Result<(), Failure>> {
		self.local
			.ops()
			.into_iter()
			.filter_map(|op| match op {
				LocalOp::RenameAnswered { answer, .. } => Some(answer),
				_ => None,
			})
			.collect()
	}

	fn uploads(&self) -> usize {
		self.drive
			.writes()
			.iter()
			.filter(|op| matches!(op, DriveOp::Upload { .. } | DriveOp::UploadVersion(_)))
			.count()
	}

	fn trashes(&self) -> usize {
		self.drive
			.writes()
			.iter()
			.filter(|op| matches!(op, DriveOp::Trash(_)))
			.count()
	}
}

/// A world whose engine was killed: what the user does now, the provider never sees.
struct Stopped {
	drive: Arc<FakeDrive>,
	local: FakeLocal,
	reports: Arc<Reports>,
	records: Option<Arc<TempDir>>,
}

impl Stopped {
	/// Starts a new engine, as the next start does, the watcher's events of the stop dropped.
	fn started(self) -> World {
		self.local.take_watch_events();
		let engine = engine(
			&self.drive,
			&self.local,
			&self.reports,
			LocalConfig::default(),
			self.records.as_deref(),
		);
		engine.start();
		World {
			drive: self.drive,
			local: self.local,
			engine,
			reports: self.reports,
			records: self.records,
		}
	}
}

fn file_identity(file: &FfiFile) -> PlaceholderIdentity {
	let (stable, version) = jobs::file_ids(file).unwrap();
	PlaceholderIdentity::File { stable, version }
}

// ---- the queue ----

fn queue() -> Queue {
	Queue::new(4, WATCH_DEBOUNCE, WATCH_MAX_DEBOUNCE_BURST)
}

const SETTLE: Duration = Duration::from_secs(2);
/// What a finished local job still owes when it did not finish.
const LOCAL: Kinds = Kinds::of(Kind::Local);

/// The paths `take_due` hands out, without their kinds.
fn take_paths(queue: &mut Queue, now: Instant, limit: usize) -> Vec<RootPath> {
	queue
		.take_due(now, limit, usize::MAX)
		.into_iter()
		.map(|(path, _)| path)
		.collect()
}

#[tokio::test(start_paused = true)]
async fn a_burst_of_events_is_one_look_after_the_quiet_window() {
	let mut queue = queue();
	let path = root_path("a.txt");
	let start = Instant::now();
	for step in 0..3u32 {
		queue.push_kind(
			&path,
			start + Duration::from_millis(300) * step,
			Delay::Debounce,
			Kind::Local,
		);
	}
	let last = start + Duration::from_millis(600);
	assert!(
		queue
			.take_due(
				last + WATCH_DEBOUNCE - Duration::from_millis(1),
				8,
				usize::MAX
			)
			.is_empty()
	);
	assert_eq!(take_paths(&mut queue, last + WATCH_DEBOUNCE, 8), vec![path]);
}

#[tokio::test(start_paused = true)]
async fn hydration_jobs_never_take_more_slots_than_the_hydrations_free() {
	let mut queue = queue();
	let now = Instant::now();
	for name in ["a.bin", "b.bin", "c.bin"] {
		queue.push_kind(&root_path(name), now, Delay::Now, Kind::Hydration);
	}
	queue.push_kind(&root_path("edit.txt"), now, Delay::Now, Kind::Local);
	let taken = queue.take_due(now, 8, 1);
	assert_eq!(taken.len(), 2, "{taken:?}");
	assert!(taken.contains(&(root_path("edit.txt"), LOCAL)));
	assert_eq!(
		queue.next_due(false),
		None,
		"the hydrations left wait for a hydration to end"
	);
	assert_eq!(queue.next_due(true), Some(now));
	assert_eq!(queue.take_due(now, 8, 2).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_writer_that_never_stops_is_looked_at_after_the_burst_cap() {
	let mut queue = queue();
	let path = root_path("log.txt");
	let start = Instant::now();
	let cap = start + WATCH_DEBOUNCE * WATCH_MAX_DEBOUNCE_BURST;
	let mut now = start;
	while now < cap {
		queue.push_kind(&path, now, Delay::Debounce, Kind::Local);
		now += Duration::from_millis(500);
	}
	assert_eq!(queue.next_due(true), Some(cap));
}

#[tokio::test(start_paused = true)]
async fn a_closed_file_is_looked_at_soon() {
	let mut queue = queue();
	let path = root_path("a.txt");
	let now = Instant::now();
	queue.push_kind(&path, now, Delay::Debounce, Kind::Local);
	queue.push_kind(&path, now, Delay::Close, Kind::Local);
	assert_eq!(queue.next_due(true), Some(now + queue::CLOSE_DELAY));
}

/// A writer that keeps writing a file a look found busy: its events, during the look or after it,
/// quiet spells between them included, never bring the next look nearer than the wait for it, so
/// each look, which may copy and hash the whole file, backs off; its close brings it near.
#[tokio::test(start_paused = true)]
async fn a_busy_file_keeps_its_wait_through_its_writers_events() {
	let mut queue = queue();
	let path = root_path("log.txt");
	let mut now = Instant::now();
	queue.push_kind(&path, now, Delay::Now, Kind::Local);
	let mut waits = Vec::new();
	for _ in 0..5 {
		assert_eq!(take_paths(&mut queue, now, 8), vec![path.clone()]);
		queue.push_kind(&path, now, Delay::Debounce, Kind::Local);
		queue.finish(&path, &Outcome::Busy, LOCAL, now, SETTLE);
		let due = queue.next_due(true).unwrap();
		// The writer writes every half second meanwhile.
		let mut at = now + Duration::from_millis(500);
		while at < due {
			queue.push_kind(&path, at, Delay::Debounce, Kind::Local);
			assert!(
				take_paths(&mut queue, at, 8).is_empty(),
				"looked at while written"
			);
			at += Duration::from_millis(500);
		}
		assert_eq!(queue.next_due(true), Some(due));
		waits.push(due - now);
		now = due;
	}
	assert_eq!(waits, [2, 4, 8, 16, 30].map(Duration::from_secs).to_vec());
	assert_eq!(take_paths(&mut queue, now, 8), vec![path.clone()]);
	queue.finish(&path, &Outcome::Busy, LOCAL, now, SETTLE);
	let last = now + Duration::from_secs(1);
	queue.push_kind(&path, last, Delay::Debounce, Kind::Local);
	assert_eq!(
		queue.next_due(true),
		Some(now + Duration::from_secs(30)),
		"a quiet spell after a write is no close"
	);
	let closed = last + Duration::from_millis(100);
	queue.push_kind(&path, closed, Delay::Close, Kind::Local);
	assert_eq!(queue.next_due(true), Some(closed + queue::CLOSE_DELAY));
}

#[tokio::test(start_paused = true)]
async fn an_event_during_a_job_queues_another_look_after_it() {
	let mut queue = queue();
	let path = root_path("a.txt");
	let now = Instant::now();
	queue.push_kind(&path, now, Delay::Now, Kind::Local);
	assert_eq!(take_paths(&mut queue, now, 8), vec![path.clone()]);
	queue.push_kind(&path, now, Delay::Debounce, Kind::Local);
	assert!(take_paths(&mut queue, now, 8).is_empty(), "it runs");
	queue.finish(&path, &Outcome::Done, LOCAL, now, SETTLE);
	assert_eq!(take_paths(&mut queue, now + WATCH_DEBOUNCE, 8), vec![path]);
}

#[tokio::test(start_paused = true)]
async fn a_full_queue_overflows() {
	let mut queue = queue();
	let now = Instant::now();
	for n in 0..4 {
		assert_eq!(
			queue.push_kind(&root_path(&format!("{n}")), now, Delay::Now, Kind::Local),
			Admit::Queued
		);
	}
	assert_eq!(
		queue.push_kind(&root_path("4"), now, Delay::Now, Kind::Local),
		Admit::Overflow
	);
	// A path already queued still merges.
	assert_eq!(
		queue.push_kind(&root_path("0"), now, Delay::Now, Kind::Local),
		Admit::Queued
	);
	queue.clear();
	assert_eq!(
		queue.push_kind(&root_path("4"), now, Delay::Now, Kind::Local),
		Admit::Queued
	);
}

#[tokio::test(start_paused = true)]
async fn failures_back_off_then_park_and_an_event_retries() {
	let mut queue = queue();
	let path = root_path("a.txt");
	let mut now = Instant::now();
	queue.push_kind(&path, now, Delay::Now, Kind::Local);
	let mut waits = Vec::new();
	for _ in 0..queue::PARK_AFTER - 1 {
		assert_eq!(take_paths(&mut queue, now, 8), vec![path.clone()]);
		let reports = queue.finish(
			&path,
			&Outcome::Failed(Failure::Unreachable),
			LOCAL,
			now,
			SETTLE,
		);
		assert!(reports.is_empty());
		let due = queue.next_due(true).unwrap();
		waits.push(due - now);
		now = due;
	}
	assert_eq!(
		waits,
		[2, 4, 8, 16, 32, 64, 128].map(Duration::from_secs).to_vec()
	);
	take_paths(&mut queue, now, 8);
	let reports = queue.finish(
		&path,
		&Outcome::Failed(Failure::Unreachable),
		LOCAL,
		now,
		SETTLE,
	);
	assert_eq!(
		reports,
		vec![LocalReport::Parked {
			path: path.clone(),
			failure: Failure::Unreachable,
			attempts: queue::PARK_AFTER,
		}]
	);
	assert_eq!(queue.next_due(true), None, "parked");
	queue.unpark_unreachable(now);
	assert_eq!(take_paths(&mut queue, now, 8), vec![path.clone()]);
	queue.finish(&path, &Outcome::Park(Failure::Internal), LOCAL, now, SETTLE);
	assert_eq!(queue.next_due(true), None, "parked at once");
	queue.push_kind(&path, now, Delay::Now, Kind::Local);
	assert_eq!(take_paths(&mut queue, now, 8), vec![path]);
}

#[tokio::test(start_paused = true)]
async fn looks_that_come_to_nothing_back_off_and_park_whatever_events_come() {
	let mut queue = queue();
	let path = root_path("a.txt");
	let mut now = Instant::now();
	queue.push_kind(&path, now, Delay::Now, Kind::Local);
	let mut waits = Vec::new();
	for _ in 0..queue::AGAIN_FREE + queue::PARK_AFTER - 1 {
		assert_eq!(take_paths(&mut queue, now, 8), vec![path.clone()]);
		let reports = queue.finish(&path, &Outcome::Again(Duration::ZERO), LOCAL, now, SETTLE);
		assert!(reports.is_empty());
		// The job's own write raises an event at once: past the free looks, it never brings the path
		// back before its backoff.
		queue.push_kind(&path, now, Delay::Now, Kind::Local);
		let due = queue.next_due(true).unwrap();
		waits.push(due - now);
		now = due;
	}
	let mut expected = vec![Duration::ZERO; 4];
	expected.extend([2, 4, 8, 16, 32, 64, 128].map(Duration::from_secs));
	assert_eq!(waits, expected);
	take_paths(&mut queue, now, 8);
	let reports = queue.finish(&path, &Outcome::Again(Duration::ZERO), LOCAL, now, SETTLE);
	assert_eq!(
		reports,
		vec![LocalReport::Parked {
			path: path.clone(),
			failure: Failure::Transient,
			attempts: queue::PARK_AFTER,
		}]
	);
	assert_eq!(queue.next_due(true), None, "parked");
	queue.push_kind(&path, now, Delay::Now, Kind::Local);
	assert_eq!(
		queue.next_due(true),
		Some(now + Duration::from_secs(256)),
		"an event retries it once its backoff ran out"
	);
	now += Duration::from_secs(256);
	take_paths(&mut queue, now, 8);
	queue.finish(&path, &Outcome::Done, LOCAL, now, SETTLE);
	queue.push_kind(&path, now, Delay::Now, Kind::Local);
	assert_eq!(queue.next_due(true), Some(now), "at rest, it starts afresh");
}

#[tokio::test(start_paused = true)]
async fn looks_that_send_new_bytes_start_their_count_afresh_and_the_same_bytes_do_not() {
	let mut queue = queue();
	let path = root_path("a.txt");
	let mut now = Instant::now();
	queue.push_kind(&path, now, Delay::Now, Kind::Local);
	for n in 0..=255u8 {
		assert_eq!(take_paths(&mut queue, now, 8), vec![path.clone()]);
		queue.sent(&path, [n; 32]);
		let reports = queue.finish(&path, &Outcome::Again(Duration::ZERO), LOCAL, now, SETTLE);
		assert!(reports.is_empty());
		let due = queue.next_due(true).unwrap();
		assert_eq!(
			due,
			now + Duration::from_millis(50),
			"every look sent new bytes"
		);
		now = due;
	}
	let mut waits = Vec::new();
	for _ in 0..queue::AGAIN_FREE + 2 {
		assert_eq!(take_paths(&mut queue, now, 8), vec![path.clone()]);
		queue.sent(&path, [255; 32]);
		queue.finish(&path, &Outcome::Again(Duration::ZERO), LOCAL, now, SETTLE);
		let due = queue.next_due(true).unwrap();
		waits.push(due - now);
		now = due;
	}
	let mut expected = vec![Duration::from_millis(50); 3];
	expected.extend([2, 4, 8].map(Duration::from_secs));
	assert_eq!(waits, expected, "the same bytes again are no progress");
}

/// A file its writer saves again during each upload, closing it in between: every look sends a new
/// version, which is progress, so the file is never parked however many looks it takes, and the
/// last save is what the drive holds once the writer stops.
#[tokio::test(start_paused = true)]
async fn a_file_saved_again_during_each_upload_is_never_parked() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	let gate = world.drive.gate_uploads();
	let sends = |world: &World| {
		world
			.drive
			.ops()
			.iter()
			.filter(|op| matches!(op, DriveOp::UploadVersion(_)))
			.count()
	};
	world.local.user_write(&root_path("a.txt"), b"v0").unwrap();
	for n in 1..=12 {
		for _ in 0..600 {
			if sends(&world) == n {
				break;
			}
			world.run(Duration::from_secs(1)).await;
		}
		assert_eq!(sends(&world), n);
		world
			.local
			.user_write(&root_path("a.txt"), format!("v{n}").as_bytes())
			.unwrap();
		gate.add_permits(1);
	}
	gate.add_permits(100);
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"v12");
	assert!(world.in_sync("a.txt"));
	assert_eq!(world.parked("a.txt"), 0);
	assert_eq!(world.uploads(), 13);
}

/// Waits for something outside the job never count as failures or as looks that came to nothing,
/// and never park; once they have lasted ten minutes the path is reported, once, and looked at once
/// a minute; a look that ends otherwise starts afresh.
#[tokio::test(start_paused = true)]
async fn waits_for_something_outside_the_job_never_park_and_are_reported_once_long() {
	let mut queue = queue();
	let path = root_path("a.txt");
	let start = Instant::now();
	let mut now = start;
	queue.push_kind(&path, now, Delay::Now, Kind::Local);
	let mut reports = Vec::new();
	let mut waits = Vec::new();
	while now < start + Duration::from_secs(15 * 60) {
		assert_eq!(take_paths(&mut queue, now, 8), vec![path.clone()]);
		reports.extend(queue.finish(
			&path,
			&Outcome::Wait(Duration::from_secs(1)),
			LOCAL,
			now,
			SETTLE,
		));
		let due = queue.next_due(true).unwrap();
		waits.push((now - start, due - now));
		now = due;
	}
	assert_eq!(
		reports,
		vec![LocalReport::Parked {
			path: path.clone(),
			failure: Failure::Transient,
			attempts: 601,
		}]
	);
	assert!(
		waits
			.iter()
			.all(|(at, wait)| if *at < queue::WAIT_REPORT_AFTER {
				*wait == Duration::from_secs(1)
			} else {
				*wait == queue::WAIT_SLOW_DELAY
			}),
		"{waits:?}"
	);
	// Neither a look that came to nothing nor a failure was counted.
	take_paths(&mut queue, now, 8);
	queue.finish(&path, &Outcome::Again(Duration::ZERO), LOCAL, now, SETTLE);
	assert_eq!(queue.next_due(true), Some(now + Duration::from_millis(50)));
	now += Duration::from_millis(50);
	take_paths(&mut queue, now, 8);
	queue.finish(
		&path,
		&Outcome::Failed(Failure::Unreachable),
		LOCAL,
		now,
		SETTLE,
	);
	assert_eq!(queue.next_due(true), Some(now + Duration::from_secs(2)));
	// Afresh: another ten minutes before the next report.
	now += Duration::from_secs(2);
	take_paths(&mut queue, now, 8);
	let reports = queue.finish(
		&path,
		&Outcome::Wait(Duration::from_secs(1)),
		LOCAL,
		now,
		SETTLE,
	);
	assert!(reports.is_empty());
	assert_eq!(queue.next_due(true), Some(now + Duration::from_secs(1)));
}

/// Ends `count` looks at `path` with [`Outcome::Again`] from `now`, each when it falls due; the time
/// the last one ended.
fn come_to_nothing(queue: &mut Queue, path: &RootPath, mut now: Instant, count: u32) -> Instant {
	for _ in 0..count {
		now = queue.next_due(true).unwrap().max(now);
		assert_eq!(take_paths(queue, now, 8), vec![path.clone()]);
		queue.finish(path, &Outcome::Again(Duration::ZERO), LOCAL, now, SETTLE);
	}
	now
}

/// A path backing off from looks that came to nothing that moves with its directory onto a path
/// queued already, blocked there on its directory: the count and the backoff go with it, so the
/// directory's job ending does not bring it back before the backoff runs out, and its next such look
/// is counted; the drive coming back then starts the path, parked as unreachable, afresh.
#[tokio::test(start_paused = true)]
async fn a_backoff_survives_a_move_onto_a_blocked_path_and_ends_when_the_drive_returns() {
	let mut queue = queue();
	let (dir, other) = (root_path("d"), root_path("e"));
	let (blocked, moved) = (root_path("d\\f.txt"), root_path("e\\f.txt"));
	let now = Instant::now();
	queue.push_kind(&blocked, now, Delay::Now, Kind::Local);
	assert_eq!(take_paths(&mut queue, now, 8), vec![blocked.clone()]);
	queue.finish(&blocked, &Outcome::Blocked(dir.clone()), LOCAL, now, SETTLE);
	queue.push_kind(&moved, now, Delay::Now, Kind::Local);
	let now = come_to_nothing(&mut queue, &moved, now, queue::AGAIN_FREE + 1);
	let hold = now + Duration::from_secs(2);
	assert_eq!(queue.rebase(&other, &dir, now), Admit::Queued);
	queue.push_kind(&dir, now, Delay::Now, Kind::Local);
	assert_eq!(take_paths(&mut queue, now, 8), vec![dir.clone()]);
	queue.finish(&dir, &Outcome::Done, LOCAL, now, SETTLE);
	assert_eq!(
		queue.next_due(true),
		Some(hold),
		"released, still backing off"
	);
	assert_eq!(take_paths(&mut queue, hold, 8), vec![blocked.clone()]);
	queue.finish(
		&blocked,
		&Outcome::Again(Duration::ZERO),
		LOCAL,
		hold,
		SETTLE,
	);
	let next = hold + Duration::from_secs(4);
	assert_eq!(queue.next_due(true), Some(next), "the second counted look");
	assert_eq!(take_paths(&mut queue, next, 8), vec![blocked.clone()]);
	queue.finish(
		&blocked,
		&Outcome::Park(Failure::Unreachable),
		LOCAL,
		next,
		SETTLE,
	);
	assert_eq!(queue.next_due(true), None, "parked");
	queue.unpark_unreachable(next);
	assert_eq!(take_paths(&mut queue, next, 8), vec![blocked.clone()]);
	queue.finish(
		&blocked,
		&Outcome::Again(Duration::ZERO),
		LOCAL,
		next,
		SETTLE,
	);
	assert_eq!(
		queue.next_due(true),
		Some(next + Duration::from_millis(50)),
		"started afresh"
	);
}

#[tokio::test(start_paused = true)]
async fn a_blocked_path_waits_for_its_directory() {
	let mut queue = queue();
	let dir = root_path("d");
	let file = root_path("d\\f.txt");
	let now = Instant::now();
	queue.push_kind(&file, now, Delay::Now, Kind::Local);
	take_paths(&mut queue, now, 8);
	queue.finish(&file, &Outcome::Blocked(dir.clone()), LOCAL, now, SETTLE);
	queue.push_kind(&dir, now, Delay::Now, Kind::Local);
	assert_eq!(take_paths(&mut queue, now, 8), vec![dir.clone()]);
	queue.finish(&dir, &Outcome::Done, LOCAL, now, SETTLE);
	assert_eq!(take_paths(&mut queue, now, 8), vec![file.clone()]);
	queue.finish(&file, &Outcome::Blocked(dir.clone()), LOCAL, now, SETTLE);
	queue.push_kind(&dir, now, Delay::Now, Kind::Local);
	take_paths(&mut queue, now, 8);
	queue.finish(&dir, &Outcome::Skipped, LOCAL, now, SETTLE);
	assert!(
		!queue.holds(&file),
		"a skipped directory drops what waits on it"
	);
}

#[tokio::test(start_paused = true)]
async fn queued_paths_follow_a_directory_rename() {
	let mut queue = queue();
	let now = Instant::now();
	queue.push_kind(&root_path("old\\a.txt"), now, Delay::Now, Kind::Local);
	assert_eq!(
		queue.rebase(&root_path("OLD"), &root_path("new"), now),
		Admit::Queued
	);
	assert_eq!(
		take_paths(&mut queue, now, 8),
		vec![root_path("new\\a.txt")]
	);
}

#[tokio::test(start_paused = true)]
async fn a_rename_with_no_room_for_a_running_paths_new_path_overflows() {
	let mut queue = Queue::new(1, WATCH_DEBOUNCE, WATCH_MAX_DEBOUNCE_BURST);
	let now = Instant::now();
	queue.push_kind(&root_path("old\\a.txt"), now, Delay::Now, Kind::Local);
	take_paths(&mut queue, now, 8);
	assert_eq!(
		queue.rebase(&root_path("old"), &root_path("new"), now),
		Admit::Overflow
	);
}

#[tokio::test(start_paused = true)]
async fn a_path_whose_job_never_started_is_due_again() {
	let mut queue = queue();
	let path = root_path("a.txt");
	let now = Instant::now();
	queue.push_kind(&path, now, Delay::Now, Kind::Local);
	assert_eq!(take_paths(&mut queue, now, 8), vec![path.clone()]);
	queue.put_back(&path, now);
	assert_eq!(take_paths(&mut queue, now, 8), vec![path]);
}

#[tokio::test(start_paused = true)]
async fn a_file_held_open_is_looked_at_less_often_then_reported_busy() {
	let mut queue = queue();
	let path = root_path("a.txt");
	let mut now = Instant::now();
	queue.push_kind(&path, now, Delay::Now, Kind::Local);
	let mut waits = Vec::new();
	let mut reports = Vec::new();
	while now.saturating_duration_since(Instant::now()) < Duration::from_secs(11 * 60) {
		take_paths(&mut queue, now, 8);
		reports.extend(queue.finish(&path, &Outcome::Busy, LOCAL, now, SETTLE));
		let due = queue.next_due(true).unwrap();
		waits.push(due - now);
		now = due;
	}
	assert_eq!(&waits[..5], [2, 4, 8, 16, 30].map(Duration::from_secs));
	assert_eq!(waits.last(), Some(&Duration::from_secs(60)));
	assert_eq!(reports, vec![LocalReport::Busy { path }]);
}

// ---- new files ----

#[tokio::test(start_paused = true)]
async fn a_new_file_is_uploaded_and_becomes_its_placeholder() {
	let world = world().await;
	world.local.user_create(&root_path("a.txt"), b"hello");
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "a.txt");
	assert_eq!(
		world.drive.content(jobs::file_ids(&remote).unwrap().0),
		b"hello"
	);
	assert_eq!(world.identity("a.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("a.txt"));
	assert_eq!(world.uploads(), 1);
	assert!(world.reports().contains(&LocalReport::Uploaded {
		path: root_path("a.txt"),
		identity: file_identity(&remote),
	}));
	world.settle().await;
	assert_eq!(
		world.uploads(),
		1,
		"the convert's own events change nothing"
	);
}

#[tokio::test(start_paused = true)]
async fn a_file_written_while_it_uploads_ends_in_sync_with_one_more_version() {
	let world = world().await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"one");
	world.settle().await;
	assert_eq!(world.uploads(), 1, "the upload waits at the gate");
	world.local.user_write(&root_path("a.txt"), b"two").unwrap();
	gate.add_permits(2);
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "a.txt");
	assert_eq!(
		world.drive.content(jobs::file_ids(&remote).unwrap().0),
		b"two"
	);
	assert_eq!(world.identity("a.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("a.txt"));
	assert_eq!(
		world
			.drive
			.writes()
			.iter()
			.map(|op| match op {
				DriveOp::Upload { .. } => "upload",
				DriveOp::Rename { .. } => "name",
				DriveOp::UploadVersion(_) => "version",
				_ => "other",
			})
			.collect::<Vec<_>>(),
		["upload", "name", "version"],
		"one new file under a name of its own, its name, then one new version"
	);
}

#[tokio::test(start_paused = true)]
async fn a_file_deleted_while_it_uploads_has_its_remote_file_trashed() {
	let world = world().await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"one");
	world.settle().await;
	assert!(matches!(
		world.local.user_delete(&root_path("a.txt")),
		UserChange::Done
	));
	gate.add_permits(1);
	world.settle().await;
	assert_eq!(world.trashes(), 1);
	assert_eq!(world.drive.child(&root_dir(), "a.txt"), None);
	assert!(world.reports().contains(&LocalReport::Trashed {
		path: root_path("a.txt")
	}));
}

#[tokio::test(start_paused = true)]
async fn a_file_renamed_while_it_uploads_is_converted_where_it_went() {
	let world = world().await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"one");
	world.settle().await;
	assert!(matches!(
		world
			.local
			.user_rename(&root_path("a.txt"), &root_path("b.txt"))
			.unwrap(),
		UserChange::Done
	));
	world.run(Duration::from_secs(5)).await;
	gate.add_permits(1);
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "b.txt");
	assert_eq!(world.drive.child(&root_dir(), "a.txt"), None);
	assert_eq!(world.identity("b.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("b.txt"));
	assert_eq!(world.uploads(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_file_renamed_as_its_upload_ends_is_taken_in_where_it_went() {
	let world = world().await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"one");
	world.settle().await;
	// Renamed, still ordinary, once the provider opened it to give it its upload's identity and
	// before it did: no callback, and the watcher's event comes after.
	world
		.local
		.replace_during_settled_hash(&root_path("a.txt"), &root_path("b.txt"));
	gate.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "b.txt");
	assert_eq!(world.identity("b.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("b.txt"));
	assert_eq!(
		world.drive.content(jobs::file_ids(&remote).unwrap().0),
		b"one"
	);
	assert_eq!(world.local.entry(&root_path("a.txt")), None);
	assert_eq!(world.drive.child(&root_dir(), "a.txt"), None);
	assert_eq!(
		world.drive.trashed().len(),
		1,
		"the first upload goes to the trash: {:?}",
		world.drive.writes()
	);
}

#[tokio::test(start_paused = true)]
async fn a_file_whose_overtaken_convert_cannot_be_taken_back_is_uploaded_where_it_went() {
	let world = world().await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"one");
	world.settle().await;
	world
		.local
		.replace_during_settled_hash(&root_path("a.txt"), &root_path("b.txt"));
	world.local.fail_revert(
		&root_path("b.txt"),
		Some(LocalError::Os(crate::os::OsError::Rejected { code: -1 })),
	);
	gate.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "b.txt");
	assert_eq!(world.identity("b.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("b.txt"));
	assert_eq!(
		world.drive.content(jobs::file_ids(&remote).unwrap().0),
		b"one"
	);
	assert_eq!(world.local.entry(&root_path("a.txt")), None);
	assert_eq!(world.drive.names(&root_dir()), ["b.txt"]);
	assert_eq!(
		world.drive.trashed().len(),
		1,
		"the first upload goes to the trash: {:?}",
		world.drive.writes()
	);
}

#[tokio::test(start_paused = true)]
async fn a_file_whose_overtaken_convert_was_looked_past_where_it_went_is_taken_in_there() {
	let world = world().await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"one");
	world.settle().await;
	world
		.local
		.replace_during_settled_hash(&root_path("a.txt"), &root_path("b.txt"));
	// Every job the watcher's events bring to `b.txt` looks in the instant the file there is a
	// placeholder of the upload of `a.txt`, finds it in sync, and stops.
	world.local.swallow_events(&root_path("b.txt"));
	gate.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "b.txt");
	assert_eq!(world.identity("b.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("b.txt"));
	assert_eq!(
		world.drive.content(jobs::file_ids(&remote).unwrap().0),
		b"one"
	);
	assert_eq!(world.local.entry(&root_path("a.txt")), None);
	assert_eq!(world.drive.names(&root_dir()), ["b.txt"]);
	assert_eq!(world.drive.trashed().len(), 1, "{:?}", world.drive.writes());
}

#[tokio::test(start_paused = true)]
async fn a_file_whose_overtaken_convert_cannot_be_taken_back_is_looked_at_where_it_went() {
	let world = world().await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"one");
	world.settle().await;
	world
		.local
		.replace_during_settled_hash(&root_path("a.txt"), &root_path("b.txt"));
	world.local.fail_revert(
		&root_path("b.txt"),
		Some(LocalError::Os(crate::os::OsError::Rejected { code: -1 })),
	);
	world.local.swallow_events(&root_path("b.txt"));
	gate.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "b.txt");
	assert_eq!(world.identity("b.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("b.txt"));
	assert_eq!(world.drive.names(&root_dir()), ["b.txt"]);
	assert_eq!(world.drive.trashed().len(), 1, "{:?}", world.drive.writes());
}

#[tokio::test(start_paused = true)]
async fn an_overtaken_convert_whose_item_cannot_be_found_again_has_the_root_walked() {
	let world = world().await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"one");
	world.settle().await;
	world
		.local
		.replace_during_settled_hash(&root_path("a.txt"), &root_path("b.txt"));
	world.local.swallow_events(&root_path("b.txt"));
	world
		.local
		.fail_handle_paths(Some(LocalError::Os(crate::os::OsError::Rejected {
			code: -1,
		})));
	gate.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "b.txt");
	assert_eq!(world.identity("b.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("b.txt"));
	assert_eq!(world.drive.names(&root_dir()), ["b.txt"]);
}

#[tokio::test(start_paused = true)]
async fn a_new_file_equal_to_a_remote_one_of_its_name_is_adopted() {
	let world = world().await;
	let remote = world.drive.add_remote_file(&root_dir(), "a.txt", b"same");
	world.local.user_create(&root_path("a.txt"), b"same");
	world.settle().await;
	assert_eq!(world.uploads(), 0);
	assert_eq!(world.identity("a.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("a.txt"));
	assert!(world.reports().contains(&LocalReport::Adopted {
		path: root_path("a.txt"),
		identity: file_identity(&remote),
	}));
}

#[tokio::test(start_paused = true)]
async fn a_new_file_over_a_different_remote_one_keeps_both() {
	for (remote_name, local_name) in [("a.txt", "a.txt"), ("A.TXT", "a.txt")] {
		let world = world().await;
		let remote = world
			.drive
			.add_remote_file(&root_dir(), remote_name, b"remote");
		world.local.user_create(&root_path(local_name), b"local");
		world.settle().await;
		let aside = world.remote_file(&root_dir(), "a.old.txt");
		assert_eq!(
			world.drive.content(jobs::file_ids(&aside).unwrap().0),
			b"local"
		);
		assert_eq!(world.identity("a.old.txt"), Some(file_identity(&aside)));
		assert!(world.in_sync("a.old.txt"));
		assert_eq!(world.identity(remote_name), Some(file_identity(&remote)));
		assert_eq!(
			world.drive.content(jobs::file_ids(&remote).unwrap().0),
			b"remote",
			"the remote file is never overwritten"
		);
		assert!(world.reports().contains(&LocalReport::Conflict {
			path: root_path(local_name),
			kept_as: root_path("a.old.txt"),
			kind: ConflictKind::CreateVsExisting,
		}));
	}
}

#[tokio::test(start_paused = true)]
async fn a_new_directory_merges_into_a_remote_one_of_its_name() {
	let world = world().await;
	let remote = world.drive.add_remote_dir(&root_dir(), "d");
	world.local.user_mkdir(&root_path("d"));
	world.settle().await;
	let uuid = Uuid::parse_str(&remote.uuid).unwrap();
	assert!(world.local.ops().contains(&LocalOp::ConvertedDir {
		path: root_path("d"),
		identity: PlaceholderIdentity::Dir { uuid },
		populated: false,
	}));
	assert!(
		!world
			.drive
			.writes()
			.iter()
			.any(|op| matches!(op, DriveOp::CreateDir { .. }))
	);
}

#[tokio::test(start_paused = true)]
async fn a_new_directory_over_a_remote_file_keeps_both() {
	let world = world().await;
	let remote = world.drive.add_remote_file(&root_dir(), "d", b"file");
	world.local.user_mkdir(&root_path("d"));
	world.settle().await;
	assert!(matches!(
		world.drive.child(&root_dir(), "d.old"),
		Some(FfiNonRootObject::Dir(_))
	));
	assert_eq!(world.identity("d"), Some(file_identity(&remote)));
}

/// The uuid of the remote directory `name` in the directory `dir` (a cache id).
fn remote_dir_uuid(world: &World, dir: &str, name: &str) -> Uuid {
	match world.drive.child(dir, name) {
		Some(FfiNonRootObject::Dir(remote)) => Uuid::parse_str(&remote.uuid).unwrap(),
		other => panic!("{name} is not a remote directory: {other:?}"),
	}
}

/// How many items the drive trashed, each of which must be a directory.
fn trashed_dirs(world: &World) -> usize {
	let trashed = world.drive.trashed();
	assert!(
		trashed
			.iter()
			.all(|item| matches!(item, FfiNonRootObject::Dir(_))),
		"{trashed:?}"
	);
	trashed.len()
}

#[tokio::test(start_paused = true)]
async fn a_new_directory_renamed_as_its_take_in_ends_is_taken_in_where_it_went() {
	let world = world().await;
	world.local.user_mkdir(&root_path("d"));
	world.local.user_create(&root_path("d\\f.txt"), b"f");
	// Renamed, still ordinary, once the provider opened it to convert it and before it did: no
	// callback, and the watcher's event comes after.
	world
		.local
		.rename_during_dir_convert(&root_path("d"), &root_path("e"));
	world.settle().await;
	world.settle().await;
	let uuid = remote_dir_uuid(&world, &root_dir(), "e");
	assert_eq!(world.identity("e"), Some(PlaceholderIdentity::Dir { uuid }));
	assert_eq!(world.local.entry(&root_path("d")), None);
	assert_eq!(world.drive.names(&format!("stable/{uuid}")), ["f.txt"]);
	assert!(world.in_sync("e\\f.txt"));
	assert_eq!(world.uploads(), 1);
	assert_eq!(
		trashed_dirs(&world),
		1,
		"the directory created for the old name goes to the trash once nothing can be sending \
		 into it: {:?}",
		world.drive.writes()
	);
	assert_eq!(world.drive.names(&root_dir()), ["e"]);
}

#[tokio::test(start_paused = true)]
async fn a_directory_whose_overtaken_convert_was_looked_past_where_it_went_is_taken_in_there() {
	let world = world().await;
	world.local.user_mkdir(&root_path("d"));
	world.local.user_create(&root_path("d\\f.txt"), b"f");
	world
		.local
		.rename_during_dir_convert(&root_path("d"), &root_path("e"));
	// Every job the watcher's events bring to `e` or under it looks in the instant `e` is a
	// placeholder of the directory created for `d`, finds it in sync, and stops.
	world.local.swallow_events(&root_path("e"));
	world.settle().await;
	world.settle().await;
	let uuid = remote_dir_uuid(&world, &root_dir(), "e");
	assert_eq!(world.identity("e"), Some(PlaceholderIdentity::Dir { uuid }));
	assert_eq!(world.drive.names(&format!("stable/{uuid}")), ["f.txt"]);
	assert!(world.in_sync("e\\f.txt"));
	assert_eq!(world.uploads(), 1);
}

/// A world whose new directory `d`, holding `d\f.txt`, is renamed to `e` as its convert happens,
/// with every upload held: the directory created for `d` waits while the upload under `e` runs.
async fn overtaken_while_uploads_held() -> (World, Arc<Semaphore>) {
	let world = world().await;
	world.local.set_populated(&RootPath::root(), true);
	let uploads = world.drive.gate_uploads();
	world.local.user_mkdir(&root_path("d"));
	world.local.user_create(&root_path("d\\f.txt"), b"f");
	world
		.local
		.rename_during_dir_convert(&root_path("d"), &root_path("e"));
	world.settle().await;
	(world, uploads)
}

/// Lets every held upload go on.
fn release_uploads(uploads: &Semaphore) {
	uploads.add_permits(Semaphore::MAX_PERMITS / 2);
}

/// Has a walk's reconcile of the root place the directory created for `d`, nothing local standing
/// for it, as an empty directory placeholder: its uuid.
async fn placed_left_dir(world: &World) -> Uuid {
	world.engine.on_local_event(LocalEvent::Rescan);
	world.settle().await;
	let d = remote_dir_uuid(world, &root_dir(), "d");
	assert_eq!(
		world.identity("d"),
		Some(PlaceholderIdentity::Dir { uuid: d })
	);
	assert_eq!(trashed_dirs(world), 0, "{:?}", world.drive.writes());
	d
}

/// Past every look at a directory an overtaken take-in left.
const LEFTOVER_LOOKS: Duration = Duration::from_secs(20 * 60);

#[tokio::test(start_paused = true)]
async fn the_empty_folder_placed_for_a_directory_an_overtaken_take_in_left_goes_with_it() {
	let (world, uploads) = overtaken_while_uploads_held().await;
	placed_left_dir(&world).await;
	release_uploads(&uploads);
	world.run(LEFTOVER_LOOKS).await;
	assert_eq!(world.local.entry(&root_path("d")), None);
	assert_eq!(world.drive.names(&root_dir()), ["e"]);
	assert_eq!(trashed_dirs(&world), 1, "{:?}", world.drive.writes());
	let e = remote_dir_uuid(&world, &root_dir(), "e");
	assert_eq!(world.drive.names(&format!("stable/{e}")), ["f.txt"]);
	assert!(world.in_sync("e\\f.txt"));
}

#[tokio::test(start_paused = true)]
async fn a_directory_an_overtaken_take_in_left_stays_once_a_file_lands_in_it() {
	let (world, uploads) = overtaken_while_uploads_held().await;
	// Another device's upload lands in it while the upload under the new name still runs.
	let d = remote_dir_uuid(&world, &root_dir(), "d");
	world
		.drive
		.add_remote_file(&format!("stable/{d}"), "x.txt", b"x");
	release_uploads(&uploads);
	world.run(LEFTOVER_LOOKS).await;
	assert_eq!(trashed_dirs(&world), 0, "{:?}", world.drive.writes());
	assert_eq!(world.drive.names(&root_dir()), ["d", "e"]);
	assert_eq!(world.drive.names(&format!("stable/{d}")), ["x.txt"]);
}

#[tokio::test(start_paused = true)]
async fn a_directory_an_overtaken_take_in_left_stays_once_the_user_puts_a_file_in_its_folder() {
	let (world, uploads) = overtaken_while_uploads_held().await;
	let d = placed_left_dir(&world).await;
	world.local.user_create(&root_path("d\\g.txt"), b"g");
	release_uploads(&uploads);
	world.run(LEFTOVER_LOOKS).await;
	assert_eq!(trashed_dirs(&world), 0, "{:?}", world.drive.writes());
	assert_eq!(world.drive.names(&format!("stable/{d}")), ["g.txt"]);
	assert!(world.in_sync("d\\g.txt"));
}

#[tokio::test(start_paused = true)]
async fn a_directory_an_overtaken_take_in_left_stays_while_its_folder_holds_anything() {
	let (world, uploads) = overtaken_while_uploads_held().await;
	let d = placed_left_dir(&world).await;
	// Never uploaded: only the local folder holds it.
	world
		.local
		.user_create(&root_path("d\\desktop.ini"), b"[.ShellClassInfo]");
	release_uploads(&uploads);
	world.run(LEFTOVER_LOOKS).await;
	assert_eq!(trashed_dirs(&world), 0, "{:?}", world.drive.writes());
	assert!(world.drive.names(&format!("stable/{d}")).is_empty());
	assert_eq!(
		world.identity("d"),
		Some(PlaceholderIdentity::Dir { uuid: d })
	);
	assert!(world.local.entry(&root_path("d\\desktop.ini")).is_some());
}

#[tokio::test(start_paused = true)]
async fn a_directory_an_overtaken_take_in_left_stays_once_its_looks_run_out() {
	let (world, uploads) = overtaken_while_uploads_held().await;
	// The upload under the new name runs past every look.
	world.run(LEFTOVER_LOOKS).await;
	release_uploads(&uploads);
	world.run(LEFTOVER_LOOKS).await;
	assert_eq!(trashed_dirs(&world), 0, "{:?}", world.drive.writes());
	assert_eq!(world.drive.names(&root_dir()), ["d", "e"]);
	let e = remote_dir_uuid(&world, &root_dir(), "e");
	assert_eq!(world.drive.names(&format!("stable/{e}")), ["f.txt"]);
}

#[tokio::test(start_paused = true)]
async fn a_directory_whose_overtaken_convert_cannot_be_taken_back_is_taken_in_where_it_went() {
	let world = world().await;
	world.local.user_mkdir(&root_path("d"));
	world.local.user_create(&root_path("d\\f.txt"), b"f");
	world
		.local
		.rename_during_dir_convert(&root_path("d"), &root_path("e"));
	world.local.fail_revert(
		&root_path("e"),
		Some(LocalError::Os(crate::os::OsError::Rejected { code: -1 })),
	);
	world.settle().await;
	world.settle().await;
	let uuid = remote_dir_uuid(&world, &root_dir(), "e");
	assert_eq!(world.identity("e"), Some(PlaceholderIdentity::Dir { uuid }));
	assert_eq!(world.drive.names(&root_dir()), ["e"]);
	assert_eq!(world.drive.names(&format!("stable/{uuid}")), ["f.txt"]);
	assert!(world.in_sync("e\\f.txt"));
	assert_eq!(world.uploads(), 1);
	assert_eq!(trashed_dirs(&world), 1, "{:?}", world.drive.writes());
}

#[tokio::test(start_paused = true)]
async fn a_directory_made_at_the_name_of_one_renamed_as_its_take_in_ends_is_another_one() {
	let world = world().await;
	world.local.user_mkdir(&root_path("d"));
	world.local.user_create(&root_path("d\\f.txt"), b"f");
	// Renamed once the provider opened it to convert it, and a new directory made at its name.
	world
		.local
		.replace_during_dir_convert(&root_path("d"), &root_path("e"));
	world.settle().await;
	world.local.user_create(&root_path("d\\g.txt"), b"g");
	world.settle().await;
	world.settle().await;
	let (d, e) = (
		remote_dir_uuid(&world, &root_dir(), "d"),
		remote_dir_uuid(&world, &root_dir(), "e"),
	);
	assert_ne!(d, e);
	assert_eq!(
		world.identity("d"),
		Some(PlaceholderIdentity::Dir { uuid: d })
	);
	assert_eq!(
		world.identity("e"),
		Some(PlaceholderIdentity::Dir { uuid: e })
	);
	assert_eq!(world.drive.names(&root_dir()), ["d", "e"]);
	assert_eq!(world.drive.names(&format!("stable/{d}")), ["g.txt"]);
	assert_eq!(world.drive.names(&format!("stable/{e}")), ["f.txt"]);
	assert!(world.in_sync("d\\g.txt"));
	assert!(world.in_sync("e\\f.txt"));
	assert_eq!(world.uploads(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_directory_renamed_while_it_is_created_has_each_file_uploaded_once_under_it() {
	let world = world().await;
	let gate = world.drive.gate_calls(DriveCall::CreateDir);
	world.local.user_mkdir(&root_path("d"));
	world.local.user_create(&root_path("d\\a.txt"), b"a");
	world.local.user_create(&root_path("d\\b.txt"), b"b");
	world.settle().await;
	assert!(matches!(
		world
			.local
			.user_rename(&root_path("d"), &root_path("e"))
			.unwrap(),
		UserChange::Done
	));
	gate.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	let uuid = remote_dir_uuid(&world, &root_dir(), "e");
	assert_eq!(world.identity("e"), Some(PlaceholderIdentity::Dir { uuid }));
	assert_eq!(world.drive.names(&root_dir()), ["e"]);
	assert_eq!(world.local.entry(&root_path("d")), None);
	assert_eq!(
		world.drive.names(&format!("stable/{uuid}")),
		["a.txt", "b.txt"]
	);
	assert!(world.in_sync("e\\a.txt") && world.in_sync("e\\b.txt"));
	assert_eq!(world.uploads(), 2, "{:?}", world.drive.writes());
	assert_eq!(trashed_dirs(&world), 1);
}

#[tokio::test(start_paused = true)]
async fn a_directory_created_for_one_renamed_meanwhile_stays_once_it_holds_an_item() {
	let world = world().await;
	let creates = world.drive.gate_calls(DriveCall::CreateDir);
	world.local.user_mkdir(&root_path("d"));
	world.settle().await;
	assert!(matches!(
		world
			.local
			.user_rename(&root_path("d"), &root_path("e"))
			.unwrap(),
		UserChange::Done
	));
	let refreshes = world.drive.gate_calls(DriveCall::Refresh);
	creates.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	// Another device puts a file in the directory created for the old name before the job looks.
	let d = remote_dir_uuid(&world, &root_dir(), "d");
	world
		.drive
		.add_remote_file(&format!("stable/{d}"), "x.txt", b"x");
	refreshes.add_permits(Semaphore::MAX_PERMITS / 2);
	world.drive.stop_gating(DriveCall::Refresh);
	world.settle().await;
	world.settle().await;
	assert!(
		world.drive.trashed().is_empty(),
		"{:?}",
		world.drive.writes()
	);
	assert_eq!(world.drive.names(&format!("stable/{d}")), ["x.txt"]);
	let e = remote_dir_uuid(&world, &root_dir(), "e");
	assert_eq!(
		world.identity("e"),
		Some(PlaceholderIdentity::Dir { uuid: e })
	);
}

#[tokio::test(start_paused = true)]
async fn a_directory_renamed_as_its_take_in_ends_has_each_file_uploaded_once_under_it() {
	let world = world().await;
	world.local.user_mkdir(&root_path("d"));
	world.local.user_mkdir(&root_path("d\\s"));
	world.local.user_create(&root_path("d\\a.txt"), b"a");
	world.local.user_create(&root_path("d\\s\\b.txt"), b"b");
	world
		.local
		.rename_during_dir_convert(&root_path("d"), &root_path("e"));
	world.settle().await;
	world.settle().await;
	let uuid = remote_dir_uuid(&world, &root_dir(), "e");
	assert_eq!(world.drive.names(&root_dir()), ["e"]);
	assert_eq!(world.drive.names(&format!("stable/{uuid}")), ["a.txt", "s"]);
	let sub = remote_dir_uuid(&world, &format!("stable/{uuid}"), "s");
	assert_eq!(world.drive.names(&format!("stable/{sub}")), ["b.txt"]);
	assert!(world.in_sync("e\\a.txt") && world.in_sync("e\\s\\b.txt"));
	assert_eq!(world.uploads(), 2, "{:?}", world.drive.writes());
	assert_eq!(trashed_dirs(&world), 1);
}

#[tokio::test(start_paused = true)]
async fn one_job_at_a_time_still_takes_in_every_file() {
	let world = unstarted_with(LocalConfig {
		max_concurrent_jobs: NonZeroUsize::new(1).unwrap(),
		max_concurrent_uploads: NonZeroUsize::new(1).unwrap(),
		..LocalConfig::default()
	});
	world.engine.start();
	for n in 0..3 {
		world
			.local
			.user_create(&root_path(&format!("f{n}.txt")), b"x");
	}
	world.settle().await;
	assert_eq!(world.uploads(), 3);
	for n in 0..3 {
		assert!(world.in_sync(&format!("f{n}.txt")));
	}
}

// ---- trees ----

/// The writes, as which item each creates, in order: a new file by the name its upload is renamed
/// to, which the tests here give no file but its own.
fn created_names(drive: &FakeDrive) -> Vec<String> {
	drive
		.writes()
		.into_iter()
		.filter_map(|op| match op {
			DriveOp::Rename { name, .. } | DriveOp::CreateDir { name, .. } => Some(name),
			_ => None,
		})
		.collect()
}

#[tokio::test(start_paused = true)]
async fn a_tree_is_created_parents_first() {
	let world = world().await;
	world.local.user_mkdir(&root_path("a"));
	world.local.user_mkdir(&root_path("a\\b"));
	world.local.user_create(&root_path("a\\b\\f.txt"), b"f");
	world.local.user_create(&root_path("a\\g.txt"), b"g");
	world.settle().await;
	let names = created_names(&world.drive);
	let at = |name: &str| names.iter().position(|n| n == name).unwrap();
	assert!(at("a") < at("b") && at("b") < at("f.txt") && at("a") < at("g.txt"));
	assert_eq!(names.len(), 4);
	for path in ["a", "a\\b", "a\\b\\f.txt", "a\\g.txt"] {
		assert!(world.in_sync(path), "{path}");
	}
}

#[tokio::test(start_paused = true)]
async fn a_deep_event_takes_in_its_directories_first() {
	let world = world().await;
	world.local.user_mkdir(&root_path("a"));
	world.local.user_mkdir(&root_path("a\\b"));
	world.local.user_create(&root_path("a\\b\\f.txt"), b"f");
	world.local.take_watch_events();
	world
		.engine
		.on_local_event(LocalEvent::Created(root_path("a\\b\\f.txt")));
	world.settle().await;
	assert_eq!(created_names(&world.drive), ["a", "b", "f.txt"]);
}

#[tokio::test(start_paused = true)]
async fn a_tree_moved_in_with_one_event_is_taken_in_whole() {
	let world = world().await;
	world.local.user_mkdir(&root_path("t"));
	world.local.user_mkdir(&root_path("t\\u"));
	world.local.user_create(&root_path("t\\u\\f.txt"), b"f");
	world.local.user_create(&root_path("t\\g.txt"), b"g");
	world.local.take_watch_events();
	world
		.engine
		.on_local_event(LocalEvent::Created(root_path("t")));
	world.settle().await;
	assert_eq!(created_names(&world.drive).len(), 4);
	assert!(world.in_sync("t\\u\\f.txt"));
}

// ---- edits ----

#[tokio::test(start_paused = true)]
async fn an_edit_is_uploaded_as_a_new_version_without_a_download() {
	let world = world().await;
	let (stable, version) = world.placed_file("a.txt", b"one");
	world.local.user_write(&root_path("a.txt"), b"two").unwrap();
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "a.txt");
	let (remote_stable, new_version) = jobs::file_ids(&remote).unwrap();
	assert_eq!(remote_stable, stable);
	assert_ne!(new_version, version);
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(world.identity("a.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("a.txt"));
	assert!(world.drive.reads().is_empty());
	assert_eq!(
		world.drive.writes(),
		[DriveOp::UploadVersion(format!("stable/{stable}"))]
	);
}

#[tokio::test(start_paused = true)]
async fn an_append_that_leaves_the_file_in_sync_is_still_uploaded() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world
		.local
		.user_append_only(&root_path("a.txt"), b"+more")
		.unwrap();
	assert!(world.in_sync("a.txt"), "as measured");
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"one+more");
	assert!(world.in_sync("a.txt"));
}

#[tokio::test(start_paused = true)]
async fn an_edit_equal_to_a_newer_remote_version_takes_it_without_an_upload() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	let head = world.drive.bump_head(stable, b"two");
	world.local.user_write(&root_path("a.txt"), b"two").unwrap();
	world.settle().await;
	assert_eq!(world.uploads(), 0);
	assert_eq!(
		world.identity("a.txt"),
		Some(PlaceholderIdentity::File {
			stable,
			version: head
		})
	);
	assert!(world.in_sync("a.txt"));
}

#[tokio::test(start_paused = true)]
async fn an_edit_of_an_older_version_keeps_both() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	let head = world.drive.bump_head(stable, b"remote");
	world
		.local
		.user_write(&root_path("a.txt"), b"local")
		.unwrap();
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"remote", "never overwritten");
	let aside = world.remote_file(&root_dir(), "a.old.txt");
	assert_ne!(jobs::file_ids(&aside).unwrap().0, stable);
	assert_eq!(
		world.drive.content(jobs::file_ids(&aside).unwrap().0),
		b"local"
	);
	assert_eq!(world.identity("a.old.txt"), Some(file_identity(&aside)));
	assert_eq!(
		world.identity("a.txt"),
		Some(PlaceholderIdentity::File {
			stable,
			version: head
		})
	);
	let ops = world.local.ops();
	let detached = ops
		.iter()
		.position(|op| {
			matches!(
				op,
				LocalOp::Updated {
					identity: PlaceholderIdentity::Detached { .. },
					..
				}
			)
		})
		.unwrap();
	let renamed = ops
		.iter()
		.position(|op| matches!(op, LocalOp::Renamed { .. }))
		.unwrap();
	assert!(
		detached < renamed,
		"detached before it moves aside: {ops:?}"
	);
	assert!(world.reports().contains(&LocalReport::Conflict {
		path: root_path("a.txt"),
		kept_as: root_path("a.old.txt"),
		kind: ConflictKind::EditVsEdit,
	}));
}

#[tokio::test(start_paused = true)]
async fn an_edit_of_a_trashed_file_becomes_a_new_file() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.drive.trash(stable_id_of(stable)).await.unwrap();
	world.local.user_write(&root_path("a.txt"), b"two").unwrap();
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "a.txt");
	let (new_stable, _) = jobs::file_ids(&remote).unwrap();
	assert_ne!(new_stable, stable);
	assert_eq!(world.drive.content(new_stable), b"two");
	assert_eq!(world.identity("a.txt"), Some(file_identity(&remote)));
	assert_eq!(
		world.drive.trashed().len(),
		1,
		"the old file stays in the trash"
	);
}

fn stable_id_of(uuid: Uuid) -> filen_mobile_native_cache::ffi::FfiId {
	crate::identity::stable_id(uuid)
}

#[tokio::test(start_paused = true)]
async fn a_partly_downloaded_edit_is_hydrated_before_its_upload() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"0123456789");
	world
		.local
		.user_write(&root_path("a.txt"), b"abcdefghij")
		.unwrap();
	world.local.set_on_disk(&root_path("a.txt"), 4);
	world.settle().await;
	let ops = world.local.ops();
	assert!(ops.contains(&LocalOp::Hydrated(root_path("a.txt"))));
	assert!(!ops.iter().any(|op| matches!(op, LocalOp::HashedPartial(_))));
	assert_eq!(world.drive.content(stable), b"abcdefghij");

	let world = self::world().await;
	world.placed_file("b.txt", b"0123456789");
	world
		.local
		.user_write(&root_path("b.txt"), b"abcdefghij")
		.unwrap();
	world.local.set_on_disk(&root_path("b.txt"), 4);
	world
		.local
		.fail_hydrate(LocalError::Os(crate::os::OsError::Rejected { code: -1 }));
	world.settle().await;
	assert_eq!(world.uploads(), 0);
	assert!(world.reports().iter().any(|report| matches!(
		report,
		LocalReport::Parked { path, .. } if *path == root_path("b.txt")
	)));
}

#[tokio::test(start_paused = true)]
async fn a_metadata_change_of_a_dehydrated_file_marks_it_in_sync_without_a_download() {
	let world = world().await;
	let file = world.drive.add_remote_file(&root_dir(), "a.txt", b"one");
	world
		.local
		.place_file_with(&root_path("a.txt"), file_identity(&file), b"one", false);
	world.rename("a.txt", "b.txt");
	world.settle().await;
	assert!(world.in_sync("b.txt"));
	assert_eq!(world.uploads(), 0);
	assert!(world.drive.reads().is_empty());
}

/// A placeholder of `bytes` at `name` in the root with `on_disk` of its bytes on disk, which reads
/// back the drive's bytes once hydrated: the remote file's stable id.
fn placed_partly_on_disk(world: &World, name: &str, bytes: &[u8], on_disk: u64) -> Uuid {
	let file = world.drive.add_remote_file(&root_dir(), name, bytes);
	world.local.place_file_with(
		&root_path(name),
		file_identity(&file),
		&vec![0; bytes.len()],
		false,
	);
	world.local.set_on_disk(&root_path(name), on_disk);
	let drive = Arc::clone(&world.drive);
	world
		.local
		.hydrate_from(move |stable| drive.content(stable));
	jobs::file_ids(&file).unwrap().0
}

/// Whether anything brought the file at `name` onto disk: a hydration, a read of its data, or a
/// mark that fetched it.
fn fetched(world: &World, name: &str) -> bool {
	world.local.ops().iter().any(
		|op| matches!(op, LocalOp::Hydrated(at) | LocalOp::HashedPartial(at) if *at == root_path(name)),
	)
}

#[tokio::test(start_paused = true)]
async fn a_rename_of_a_dehydrated_file_renames_it_on_the_drive_and_downloads_nothing() {
	let world = world().await;
	let stable = placed_partly_on_disk(&world, "a.txt", b"one", 0);
	world.rename("a.txt", "b.txt");
	world.settle().await;
	assert_eq!(
		world.drive.writes(),
		[DriveOp::Rename {
			id: format!("stable/{stable}"),
			name: "b.txt".to_owned()
		}]
	);
	let renamed = world.local.entry(&root_path("b.txt")).unwrap();
	assert!(renamed.in_sync);
	assert_eq!(renamed.on_disk, 0, "it stays dehydrated");
	assert!(!fetched(&world, "b.txt"), "{:?}", world.local.ops());
	assert!(world.drive.reads().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_rename_of_a_partly_downloaded_file_downloads_none_of_the_rest() {
	let world = world().await;
	let stable = placed_partly_on_disk(&world, "a.txt", &[7; 10], 4);
	world.rename("a.txt", "b.txt");
	world.settle().await;
	assert_eq!(
		world.drive.writes(),
		[DriveOp::Rename {
			id: format!("stable/{stable}"),
			name: "b.txt".to_owned()
		}]
	);
	let renamed = world.local.entry(&root_path("b.txt")).unwrap();
	assert!(renamed.in_sync);
	assert_eq!(renamed.on_disk, 4, "it stays as partly on disk as it was");
	assert!(!fetched(&world, "b.txt"), "{:?}", world.local.ops());
	assert!(world.drive.reads().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_move_of_a_dehydrated_file_into_a_directory_moves_it_on_the_drive_and_downloads_nothing()
{
	// Placed before the engine starts, so its first walk knows where the drive's items go.
	let world = unstarted();
	world.local.set_populated(&RootPath::root(), true);
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let stable = placed_partly_on_disk(&world, "a.txt", b"one", 0);
	world.engine.start();
	world.run(Duration::from_secs(5)).await;
	world.rename("a.txt", "d\\a.txt");
	world.settle().await;
	let id = format!("stable/{stable}");
	assert_eq!(
		world.drive.writes(),
		[
			DriveOp::Rename {
				id: id.clone(),
				name: crate::names::transit_name(stable),
			},
			DriveOp::Move {
				id: id.clone(),
				parent: format!("stable/{uuid}"),
			},
			DriveOp::Rename {
				id,
				name: "a.txt".to_owned(),
			},
		]
	);
	let moved = world.remote_file(&format!("stable/{uuid}"), "a.txt");
	assert_eq!(moved.stable_uuid, stable.to_string());
	let entry = world.local.entry(&root_path("d\\a.txt")).unwrap();
	assert!(entry.in_sync);
	assert_eq!(entry.on_disk, 0, "it stays dehydrated");
	assert!(!fetched(&world, "d\\a.txt"), "{:?}", world.local.ops());
	assert!(world.drive.reads().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_file_held_open_is_uploaded_once_it_is_closed() {
	let world = world().await;
	world.local.user_create(&root_path("a.txt"), b"one");
	let writer = world.local.user_open_for_write(&root_path("a.txt"));
	world.run(Duration::from_secs(60)).await;
	assert_eq!(world.uploads(), 0);
	drop(writer);
	world.settle().await;
	assert_eq!(world.uploads(), 1);
	assert!(world.in_sync("a.txt"));
}

// ---- skipped names ----

#[tokio::test(start_paused = true)]
async fn names_the_server_refuses_and_default_ignores_are_left_alone() {
	let world = world().await;
	let long = "é".repeat(128);
	for name in [" lead.txt", long.as_str(), "desktop.ini", "~$a.docx"] {
		world.local.user_create(&root_path(name), b"x");
	}
	world.local.user_create(&root_path("link"), b"x");
	world.local.make_symlink(&root_path("link"));
	world.settle().await;
	assert_eq!(world.drive.writes(), []);
	for name in [
		" lead.txt",
		long.as_str(),
		"desktop.ini",
		"~$a.docx",
		"link",
	] {
		let entry = world.local.entry(&root_path(name)).unwrap();
		assert!(!entry.placeholder, "{name} stays an ordinary item");
	}
	let skipped: Vec<RootPath> = world
		.reports()
		.into_iter()
		.filter_map(|report| match report {
			LocalReport::Skipped { path, .. } => Some(path),
			_ => None,
		})
		.collect();
	assert_eq!(
		skipped.len(),
		3,
		"the default ignores are not reported: {skipped:?}"
	);
	assert!(skipped.contains(&root_path("link")));
}

#[tokio::test(start_paused = true)]
async fn a_name_the_server_refuses_on_upload_is_skipped_not_parked() {
	let world = world().await;
	world
		.drive
		.fail_next(DriveCall::Upload, CacheError::InvalidName("refused".into()));
	world.local.user_create(&root_path("a.txt"), b"x");
	world.settle().await;
	assert!(world.reports().iter().any(|report| matches!(
		report,
		LocalReport::Skipped { path, reason: LocalSkip::DriveRefusedName } if *path == root_path("a.txt")
	)));
	assert!(
		!world
			.reports()
			.iter()
			.any(|report| matches!(report, LocalReport::Parked { .. }))
	);
}

// ---- deletes ----

#[tokio::test(start_paused = true)]
async fn a_deleted_file_is_trashed_once_it_is_gone() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.delete("a.txt");
	world.settle().await;
	assert_eq!(world.delete_answers(), [Ok(())]);
	assert_eq!(world.local.entry(&root_path("a.txt")), None);
	assert_eq!(
		world.drive.writes(),
		[DriveOp::Trash(format!("stable/{stable}"))]
	);
	assert!(world.reports().contains(&LocalReport::Trashed {
		path: root_path("a.txt")
	}));
}

#[tokio::test(start_paused = true)]
async fn a_delete_while_offline_is_vetoed() {
	let world = world().await;
	world.placed_file("a.txt", b"one");
	world.drive.set_offline(true);
	world.delete("a.txt");
	world.settle().await;
	assert_eq!(world.delete_answers(), [Err(Failure::Unreachable)]);
	assert!(world.local.entry(&root_path("a.txt")).is_some());
	world.drive.set_offline(false);
	assert_eq!(world.drive.writes(), []);
	world.delete("a.txt");
	world.settle().await;
	assert_eq!(world.trashes(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_delete_the_drive_refuses_is_vetoed_with_its_failure() {
	let world = world().await;
	world.placed_file("a.txt", b"one");
	world.drive.fail_next(
		DriveCall::Refresh,
		CacheError::Unauthenticated("signed out".into()),
	);
	world.delete("a.txt");
	world.settle().await;
	assert_eq!(world.delete_answers(), [Err(Failure::NotAuthenticated)]);
}

#[tokio::test(start_paused = true)]
async fn a_delete_notified_twice_trashes_once() {
	let world = world().await;
	world.placed_file("a.txt", b"one");
	let (first, first_responder) = world.local.user_delete(&root_path("a.txt")).asked();
	let (second, second_responder) = world.local.user_delete(&root_path("a.txt")).asked();
	world.engine.on_notify_delete(first, first_responder);
	world.engine.on_notify_delete(second, second_responder);
	world.settle().await;
	assert_eq!(world.trashes(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_delete_of_a_file_edited_remotely_keeps_the_remote_file() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.drive.bump_head(stable, b"newer");
	world.delete("a.txt");
	world.settle().await;
	assert_eq!(world.delete_answers(), [Ok(())]);
	assert_eq!(world.trashes(), 0);
	assert!(
		world
			.reports()
			.contains(&LocalReport::RemoteOutlivedDelete {
				path: root_path("a.txt"),
			})
	);
}

#[tokio::test(start_paused = true)]
async fn a_deleted_directory_is_trashed_whole() {
	let world = world().await;
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let file = world
		.drive
		.add_remote_file(&format!("stable/{uuid}"), "f.txt", b"f");
	world
		.local
		.place_file(&root_path("d\\f.txt"), file_identity(&file), b"f");
	world.delete("d\\f.txt");
	world.delete("d");
	world.settle().await;
	assert_eq!(
		world.drive.writes(),
		[DriveOp::Trash(format!("stable/{uuid}"))]
	);
}

#[tokio::test(start_paused = true)]
async fn a_file_whose_confirmation_starts_as_its_directory_is_trashed_is_not_trashed_again() {
	let world = world().await;
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let file = world
		.drive
		.add_remote_file(&format!("stable/{uuid}"), "f.txt", b"f");
	world
		.local
		.place_file(&root_path("d\\f.txt"), file_identity(&file), b"f");
	world.delete("d\\f.txt");
	world.delete("d");
	world.run(Duration::from_millis(100)).await;
	// The scheduler starts both confirmations in one pass, and the directory's runs to its end
	// before the file's task is first polled: on a multi-threaded runtime, or when the blocking
	// pool answers the directory's look before its task yields.
	let start = |path: &str| {
		let mut state = lock(&world.engine.inner.state);
		let pending = state
			.deletes
			.get_mut(&root_path(path).case_key())
			.expect("the delete was allowed");
		pending.start()
	};
	let (dir_delete, file_delete) = (start("d"), start("d\\f.txt"));
	veto::confirm_delete(&world.engine.inner, dir_delete).await;
	veto::confirm_delete(&world.engine.inner, file_delete).await;
	world.settle().await;
	assert_eq!(
		world.drive.writes(),
		[DriveOp::Trash(format!("stable/{uuid}"))]
	);
	assert!(lock(&world.engine.inner.state).deletes.is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_root_cannot_be_deleted_and_a_detached_file_touches_nothing_remote() {
	let world = world().await;
	let (request, responder) = (
		DeleteRequest::new(ROOT.encode(), RootPath::root(), true, false),
		world.local.delete_responder(&RootPath::root()),
	);
	world.engine.on_notify_delete(request, responder);
	world.local.place_file(
		&root_path("x.txt"),
		PlaceholderIdentity::Detached {
			nonce: Uuid::from_u128(7),
		},
		b"x",
	);
	world.delete("x.txt");
	world.settle().await;
	assert_eq!(
		world.delete_answers(),
		[Err(Failure::InvalidRequest), Ok(())]
	);
	assert_eq!(world.drive.writes(), []);
	assert!(
		!world
			.drive
			.ops()
			.iter()
			.any(|op| matches!(op, DriveOp::Refresh(_)))
	);
}

#[tokio::test(start_paused = true)]
async fn a_delete_that_fails_after_it_was_allowed_trashes_nothing() {
	let world = world().await;
	world.placed_file("a.txt", b"one");
	world.local.fail_completion(&root_path("a.txt"));
	world.delete("a.txt");
	world.settle().await;
	assert_eq!(world.delete_answers(), [Ok(())]);
	assert!(world.local.entry(&root_path("a.txt")).is_some());
	assert_eq!(world.trashes(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_delete_taken_back_by_the_recycle_bin_trashes_on_the_move_out() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	let identity = world.identity("a.txt").unwrap();
	world.local.fail_completion(&root_path("a.txt"));
	world.delete("a.txt");
	world.run(Duration::from_millis(100)).await;
	let undelete = DeleteRequest::new(identity.encode(), root_path("a.txt"), false, true);
	world
		.engine
		.on_notify_delete(undelete, world.local.delete_responder(&root_path("a.txt")));
	world.settle().await;
	assert_eq!(world.trashes(), 0);
	let (request, responder) = world.local.user_move_out(&root_path("a.txt")).asked();
	world.engine.on_notify_rename(request, responder);
	world.settle().await;
	assert_eq!(
		world.drive.writes(),
		[DriveOp::Trash(format!("stable/{stable}"))]
	);
}

// ---- saves ----

/// Has the engine look at `path`, as the hydration of a file opened for editing makes it.
async fn open_in_editor(world: &World, path: &str) {
	world
		.engine
		.on_local_event(LocalEvent::Modified(root_path(path)));
	world.settle().await;
}

#[tokio::test(start_paused = true)]
async fn a_save_by_rename_over_the_file_is_a_new_version() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	open_in_editor(&world, "a.txt").await;
	world.local.user_create(&root_path("a.txt.tmp"), b"two");
	world
		.local
		.user_replace(&root_path("a.txt.tmp"), &root_path("a.txt"));
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "a.txt");
	assert_eq!(jobs::file_ids(&remote).unwrap().0, stable);
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(world.identity("a.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("a.txt"));
	assert_eq!(
		world.drive.writes(),
		[DriveOp::UploadVersion(format!("stable/{stable}"))]
	);
}

#[tokio::test(start_paused = true)]
async fn a_save_renamed_over_the_file_as_its_temporary_upload_ends_is_a_new_version() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	open_in_editor(&world, "a.txt").await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt.tmp"), b"two");
	world.settle().await;
	assert_eq!(
		world.uploads(),
		1,
		"the temporary file's upload waits at the gate"
	);
	// The editor renames its temporary file, still ordinary, over the file once the provider opened
	// it to give it the identity of its upload and before it did: no callback, and the watcher's
	// event comes after.
	world
		.local
		.replace_during_settled_hash(&root_path("a.txt.tmp"), &root_path("a.txt"));
	gate.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "a.txt");
	assert_eq!(jobs::file_ids(&remote).unwrap().0, stable);
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(world.identity("a.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("a.txt"));
	assert_eq!(world.local.entry(&root_path("a.txt.tmp")), None);
	assert_eq!(world.drive.child(&root_dir(), "a.txt.tmp"), None);
	assert_eq!(
		world.drive.trashed().len(),
		1,
		"the temporary upload goes to the trash: {:?}",
		world.drive.writes()
	);
}

#[tokio::test(start_paused = true)]
async fn a_save_whose_temporary_files_convert_was_looked_past_is_a_new_version() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	open_in_editor(&world, "a.txt").await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt.tmp"), b"two");
	world.settle().await;
	world
		.local
		.replace_during_settled_hash(&root_path("a.txt.tmp"), &root_path("a.txt"));
	// Every job the watcher's events bring to `a.txt` looks in the instant the saved file there is
	// a placeholder of the temporary upload, finds it in sync, and stops.
	world.local.swallow_events(&root_path("a.txt"));
	gate.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "a.txt");
	assert_eq!(jobs::file_ids(&remote).unwrap().0, stable);
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(world.identity("a.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("a.txt"));
	assert_eq!(world.drive.names(&root_dir()), ["a.txt"]);
	assert_eq!(world.drive.trashed().len(), 1, "{:?}", world.drive.writes());
}

#[tokio::test(start_paused = true)]
async fn a_save_renamed_over_the_file_while_its_temporary_upload_is_named_is_a_new_version() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	open_in_editor(&world, "a.txt").await;
	let renames = world.drive.gate_calls(DriveCall::Rename);
	world.local.user_create(&root_path("a.txt.tmp"), b"two");
	world.settle().await;
	// Uploaded, and waiting for its name: the editor renames its temporary file, still ordinary,
	// over the file.
	world
		.local
		.user_replace(&root_path("a.txt.tmp"), &root_path("a.txt"));
	world.settle().await;
	renames.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "a.txt");
	assert_eq!(jobs::file_ids(&remote).unwrap().0, stable);
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(world.identity("a.txt"), Some(file_identity(&remote)));
	assert_eq!(world.local.entry(&root_path("a.txt.tmp")), None);
	assert_eq!(world.drive.child(&root_dir(), "a.txt.tmp"), None);
	assert_eq!(
		world.drive.trashed().len(),
		1,
		"the temporary upload goes to the trash: {:?}",
		world.drive.writes()
	);
}

#[tokio::test(start_paused = true)]
async fn a_save_through_a_backup_rename_is_a_new_version() {
	let world = world().await;
	let (stable, _) = world.placed_file("doc.txt", b"one");
	open_in_editor(&world, "doc.txt").await;
	world.local.user_create(&root_path("tmp1"), b"two");
	world.rename("doc.txt", "doc.bak");
	world.run(Duration::from_millis(100)).await;
	world
		.local
		.user_rename(&root_path("tmp1"), &root_path("doc.txt"))
		.unwrap();
	world.delete("doc.bak");
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "doc.txt");
	assert_eq!(jobs::file_ids(&remote).unwrap().0, stable);
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(world.drive.child(&root_dir(), "doc.bak"), None);
	assert_eq!(world.trashes(), 0, "{:?}", world.drive.writes());
	assert_eq!(world.identity("doc.txt"), Some(file_identity(&remote)));
}

/// A world holding `doc.txt`, a placeholder of a remote file holding `one` with none of its bytes
/// on disk (it holds zeros until a hydration fills it from the drive), saved by an editor that
/// renames it aside to `doc.bak` and keeps it there: the remote file's stable id.
async fn save_over_a_dehydrated_file_kept_aside(world: &World) -> Uuid {
	let file = world.drive.add_remote_file(&root_dir(), "doc.txt", b"one");
	let (stable, version) = jobs::file_ids(&file).unwrap();
	world.local.place_file(
		&root_path("doc.txt"),
		PlaceholderIdentity::File { stable, version },
		&[0; 3],
	);
	world.local.set_on_disk(&root_path("doc.txt"), 0);
	let drive = Arc::clone(&world.drive);
	world
		.local
		.hydrate_from(move |stable| drive.content(stable));
	world.local.user_create(&root_path("tmp1"), b"two");
	world.rename("doc.txt", "doc.bak");
	world.run(Duration::from_millis(100)).await;
	world
		.local
		.user_rename(&root_path("tmp1"), &root_path("doc.txt"))
		.unwrap();
	stable
}

/// The save is the new version of the file `stable` at `doc.txt`, and `doc.bak`, the copy it left
/// aside, holds all of `one` on disk: hydrated before it was detached, while it still was the
/// version the drive reads, and uploaded as a file of its own once looked at.
async fn assert_backup_kept_as_its_own_file(world: &World, stable: Uuid) {
	assert_eq!(world.drive.content(stable), b"two");
	let saved = world.remote_file(&root_dir(), "doc.txt");
	assert_eq!(jobs::file_ids(&saved).unwrap().0, stable);
	open_in_editor(world, "doc.bak").await;
	let backup = world.local.entry(&root_path("doc.bak")).unwrap();
	assert_eq!(backup.bytes, b"one");
	assert_eq!(backup.on_disk, 3);
	let uploaded = world.remote_file(&root_dir(), "doc.bak");
	let (uploaded_stable, _) = jobs::file_ids(&uploaded).unwrap();
	assert_ne!(uploaded_stable, stable);
	assert_eq!(world.drive.content(uploaded_stable), b"one");
	assert_eq!(backup.identity, Some(file_identity(&uploaded)));
	assert!(
		!world
			.local
			.ops()
			.contains(&LocalOp::HashedPartial(root_path("doc.bak")))
	);
}

#[tokio::test(start_paused = true)]
async fn a_dehydrated_file_a_save_renamed_aside_is_downloaded_before_it_is_detached() {
	let world = world().await;
	let stable = save_over_a_dehydrated_file_kept_aside(&world).await;
	world.settle().await;
	assert_backup_kept_as_its_own_file(&world, stable).await;
}

/// Whether the sync root holds a detached file placeholder with bytes missing, bytes nothing could
/// ever fetch again, or `doc.bak` was read for an upload while some of its bytes were missing.
fn holds_unreadable_detached_file(world: &World) -> bool {
	let read_partly = |entry: &RootPath| {
		world
			.local
			.ops()
			.contains(&LocalOp::HashedPartial(entry.clone()))
	};
	read_partly(&root_path("doc.bak"))
		|| world.local.entries().iter().any(|entry| {
			!entry.is_dir
				&& matches!(entry.identity, Some(PlaceholderIdentity::Detached { .. }))
				&& entry.on_disk < u64::try_from(entry.bytes.len()).unwrap()
		})
}

/// Whether the drive holds a file whose content is `bytes`, anywhere but in its trash.
fn drive_holds(world: &World, bytes: &[u8]) -> bool {
	world.drive.names(&root_dir()).iter().any(|name| {
		matches!(world.drive.child(&root_dir(), name), Some(FfiNonRootObject::File(file))
			if world.drive.content(jobs::file_ids(&file).unwrap().0) == bytes)
	})
}

#[tokio::test(start_paused = true)]
async fn a_dehydrated_file_a_save_renamed_aside_after_another_device_changed_it_keeps_both() {
	let world = world().await;
	let stable = save_over_a_dehydrated_file_kept_aside(&world).await;
	world.drive.bump_head(stable, b"new");
	world.settle().await;
	world.settle().await;
	assert!(
		!world
			.local
			.ops()
			.contains(&LocalOp::Hydrated(root_path("doc.bak"))),
		"a placeholder of a version the drive no longer reads is never hydrated"
	);
	assert!(
		!holds_unreadable_detached_file(&world),
		"{:?}",
		world.local.entries()
	);
	assert_eq!(world.drive.content(stable), b"new");
	assert!(drive_holds(&world, b"two"), "{:?}", world.drive.writes());
}

#[tokio::test(start_paused = true)]
async fn a_dehydrated_file_a_save_renamed_aside_after_another_device_trashed_it_keeps_both() {
	let world = world().await;
	let stable = save_over_a_dehydrated_file_kept_aside(&world).await;
	world.drive.remote_trash(stable);
	world.settle().await;
	world.settle().await;
	assert!(
		!holds_unreadable_detached_file(&world),
		"{:?}",
		world.local.entries()
	);
	assert_eq!(world.drive.content(stable), b"one");
	assert!(drive_holds(&world, b"two"), "{:?}", world.drive.writes());
}

#[tokio::test(start_paused = true)]
async fn a_dehydrated_file_a_save_renamed_aside_stays_attached_until_its_download_succeeds() {
	let world = world().await;
	world
		.local
		.fail_hydrations(Some(LocalError::Os(crate::os::OsError::Rejected {
			code: -1,
		})));
	let stable = save_over_a_dehydrated_file_kept_aside(&world).await;
	world.run(Duration::from_secs(20)).await;
	assert!(matches!(
		world.identity("doc.bak"),
		Some(PlaceholderIdentity::File { stable: held, .. }) if held == stable
	));
	assert_eq!(world.local.entry(&root_path("doc.bak")).unwrap().on_disk, 0);
	assert_eq!(world.drive.content(stable), b"one");
	assert_eq!(world.uploads(), 0, "{:?}", world.drive.writes());
	world.local.fail_hydrations(None);
	world.settle().await;
	assert_backup_kept_as_its_own_file(&world, stable).await;
}

/// Whether the drive holds the file `stable` in its trash.
fn trashed_file(world: &World, stable: Uuid) -> bool {
	world.drive.trashed().iter().any(
		|item| matches!(item, FfiNonRootObject::File(file) if jobs::file_ids(file).unwrap().0 == stable),
	)
}

#[tokio::test(start_paused = true)]
async fn a_file_saved_by_delete_and_create_whose_new_file_is_deleted_while_it_uploads_is_trashed() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	open_in_editor(&world, "a.txt").await;
	world.delete("a.txt");
	world.run(Duration::from_millis(100)).await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"two");
	world.run(Duration::from_secs(5)).await;
	assert_eq!(
		world.uploads(),
		1,
		"the save's upload waits at the gate: {:?}",
		world.drive.writes()
	);
	assert!(matches!(
		world.local.user_delete(&root_path("a.txt")),
		UserChange::Done
	));
	gate.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	assert!(trashed_file(&world, stable), "{:?}", world.drive.writes());
	assert_eq!(world.local.entry(&root_path("a.txt")), None);
	assert_eq!(world.drive.names(&root_dir()), Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn a_file_saved_by_delete_and_create_whose_save_turns_out_a_new_file_is_trashed() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	open_in_editor(&world, "a.txt").await;
	world.delete("a.txt");
	world.run(Duration::from_millis(100)).await;
	// Another device renamed the file and put a file of its own at its name: the save is a new
	// file after all.
	world.drive.remote_rename(stable, "moved.txt");
	let other = world.drive.add_remote_file(&root_dir(), "a.txt", b"other");
	world.local.user_create(&root_path("a.txt"), b"two");
	world.settle().await;
	world.settle().await;
	assert!(trashed_file(&world, stable), "{:?}", world.drive.writes());
	assert_eq!(world.local.entry(&root_path("moved.txt")), None);
	assert_eq!(
		world.identity("a.txt"),
		Some(file_identity(&world.remote_file(&root_dir(), "a.txt")))
	);
	assert_eq!(
		jobs::file_ids(&world.remote_file(&root_dir(), "a.txt"))
			.unwrap()
			.0,
		jobs::file_ids(&other).unwrap().0
	);
	assert!(drive_holds(&world, b"two"), "{:?}", world.drive.writes());
}

#[tokio::test(start_paused = true)]
async fn a_save_by_delete_and_create_is_a_new_version() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	open_in_editor(&world, "a.txt").await;
	world.delete("a.txt");
	world.run(Duration::from_millis(100)).await;
	world.local.user_create(&root_path("a.txt"), b"two");
	world.settle().await;
	assert_eq!(world.trashes(), 0);
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(
		world.remote_file(&root_dir(), "a.txt").stable_uuid,
		stable.to_string()
	);
}

// ---- renames ----

#[tokio::test(start_paused = true)]
async fn a_rename_of_a_downloaded_file_whose_remote_copy_declares_no_hash_uploads_nothing() {
	let world = world().await;
	let (stable, version) = (Uuid::from_u128(0x2E1), Uuid::from_u128(0x20_02E1));
	world.placed_hashless_file("a.txt", b"one", stable, version);
	world.rename("a.txt", "b.txt");
	world.settle().await;
	assert_eq!(world.uploads(), 0, "{:?}", world.drive.writes());
	assert_eq!(
		world.identity("b.txt"),
		Some(PlaceholderIdentity::File { stable, version })
	);
	assert!(world.in_sync("b.txt"));
	assert_eq!(world.drive.content(stable), b"one");
}

#[tokio::test(start_paused = true)]
async fn an_edit_of_a_file_whose_remote_copy_declares_no_hash_is_uploaded() {
	let world = world().await;
	let (stable, version) = (Uuid::from_u128(0x2E2), Uuid::from_u128(0x20_02E2));
	world.placed_hashless_file("a.txt", b"one", stable, version);
	world.local.user_write(&root_path("a.txt"), b"two").unwrap();
	world.settle().await;
	assert_eq!(
		world.drive.writes(),
		[DriveOp::UploadVersion(format!("stable/{stable}"))]
	);
	assert_eq!(world.drive.content(stable), b"two");
	assert!(world.in_sync("a.txt"));
}

#[tokio::test(start_paused = true)]
async fn a_rename_is_applied_to_the_drive_and_marked_in_sync_again() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.rename("a.txt", "b.txt");
	world.settle().await;
	assert_eq!(world.rename_answers(), [Ok(())]);
	assert_eq!(
		world.remote_file(&root_dir(), "b.txt").stable_uuid,
		stable.to_string()
	);
	assert!(world.in_sync("b.txt"));
	assert_eq!(
		world.drive.writes(),
		[DriveOp::Rename {
			id: format!("stable/{stable}"),
			name: "b.txt".to_owned()
		}]
	);
	assert!(world.reports().contains(&LocalReport::Moved {
		from: root_path("a.txt"),
		to: root_path("b.txt"),
	}));
}

#[tokio::test(start_paused = true)]
async fn a_move_into_a_directory_moves_the_remote_item() {
	let world = world().await;
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.rename("a.txt", "d\\a.txt");
	world.settle().await;
	// Under a name only it holds while it moves, so it never lands over another file.
	assert_eq!(
		world.drive.writes(),
		[
			DriveOp::Rename {
				id: format!("stable/{stable}"),
				name: crate::names::transit_name(stable),
			},
			DriveOp::Move {
				id: format!("stable/{stable}"),
				parent: format!("stable/{uuid}"),
			},
			DriveOp::Rename {
				id: format!("stable/{stable}"),
				name: "a.txt".to_owned(),
			},
		]
	);
	assert!(
		world
			.drive
			.child(&format!("stable/{uuid}"), "a.txt")
			.is_some()
	);
}

/// Two directory placeholders, `x` and `y`, each standing for a remote directory: their cache ids.
fn two_dirs(world: &World) -> (String, String) {
	let ids = ["x", "y"].map(|name| {
		let dir = world.drive.add_remote_dir(&root_dir(), name);
		let uuid = Uuid::parse_str(&dir.uuid).unwrap();
		world
			.local
			.place_dir(&root_path(name), PlaceholderIdentity::Dir { uuid });
		format!("stable/{uuid}")
	});
	let [x, y] = ids;
	(x, y)
}

/// A file placeholder for a new remote file `name` holding `bytes` in the directory `dir` (a cache
/// id) at the local directory `at`.
fn placed_in(world: &World, dir: &str, at: &str, name: &str, bytes: &[u8]) -> Uuid {
	let file = world.drive.add_remote_file(dir, name, bytes);
	let (stable, version) = jobs::file_ids(&file).unwrap();
	world.local.place_file(
		&root_path(&format!("{at}\\{name}")),
		PlaceholderIdentity::File { stable, version },
		bytes,
	);
	stable
}

#[tokio::test(start_paused = true)]
async fn a_move_and_rename_never_lands_over_a_file_holding_the_old_name() {
	let world = world().await;
	let (x, y) = two_dirs(&world);
	let moved = placed_in(&world, &x, "x", "a.txt", b"moved");
	let other = placed_in(&world, &y, "y", "a.txt", b"other");
	// Explorer's "keep both" on a move: a free name in the destination.
	world.rename("x\\a.txt", "y\\a (2).txt");
	world.settle().await;
	assert_eq!(world.rename_answers(), [Ok(())]);
	assert_eq!(world.drive.content(other), b"other", "never superseded");
	assert_eq!(world.drive.history(other), Vec::<Vec<u8>>::new());
	assert_eq!(world.drive.content(moved), b"moved");
	assert_eq!(
		world.remote_file(&y, "a.txt").stable_uuid,
		other.to_string()
	);
	assert_eq!(
		world.remote_file(&y, "a (2).txt").stable_uuid,
		moved.to_string()
	);
	assert!(world.local.entry(&root_path("y\\a.txt")).is_some());
}

#[tokio::test(start_paused = true)]
async fn a_move_onto_a_name_taken_after_its_check_is_vetoed_and_supersedes_nothing() {
	let world = world().await;
	let (x, y) = two_dirs(&world);
	let moved = placed_in(&world, &x, "x", "a.txt", b"moved");
	let moves = world.drive.gate_calls(DriveCall::Move);
	world.rename("x\\a.txt", "y\\a.txt");
	world.run(Duration::from_secs(1)).await;
	// Another device creates a file of the name between the check and the move.
	let other = world.drive.add_remote_file(&y, "a.txt", b"other");
	let other = jobs::file_ids(&other).unwrap().0;
	moves.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	assert_eq!(world.rename_answers(), [Err(Failure::Transient)]);
	assert_eq!(world.drive.content(other), b"other", "never superseded");
	assert_eq!(world.drive.history(other), Vec::<Vec<u8>>::new());
	assert_eq!(
		world.remote_file(&x, "a.txt").stable_uuid,
		moved.to_string(),
		"back where it was"
	);
	assert!(world.local.entry(&root_path("x\\a.txt")).is_some());
}

/// A directory moved into a directory where another device gives a file its name between the
/// provider's check and its move: the server refuses it (measured), the move is vetoed, and the
/// directory stays where it was on both sides.
#[tokio::test(start_paused = true)]
async fn a_directory_move_onto_a_name_a_file_took_meanwhile_is_vetoed() {
	let world = world().await;
	let (x, y) = two_dirs(&world);
	let dir = world.drive.add_remote_dir(&x, "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("x\\d"), PlaceholderIdentity::Dir { uuid });
	let moves = world.drive.gate_calls(DriveCall::Move);
	world.rename("x\\d", "y\\d");
	world.run(Duration::from_secs(1)).await;
	let other = world.drive.add_remote_file(&y, "D", b"other");
	moves.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	assert_eq!(world.rename_answers(), [Err(Failure::Transient)]);
	assert!(matches!(
		world.drive.child(&x, "d"),
		Some(FfiNonRootObject::Dir(held)) if held.uuid == dir.uuid
	));
	assert!(matches!(
		world.drive.child(&y, "D"),
		Some(FfiNonRootObject::File(held)) if held.uuid == other.uuid
	));
	assert_eq!(
		world.identity("x\\d"),
		Some(PlaceholderIdentity::Dir { uuid })
	);
}

#[tokio::test(start_paused = true)]
async fn a_move_whose_old_parent_cannot_be_read_is_vetoed_with_the_drives_failure() {
	let world = world().await;
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	world.placed_file("a.txt", b"one");
	world
		.drive
		.fail_root(CacheError::Unauthenticated("signed out".into()));
	world.rename("a.txt", "d\\a.txt");
	world.settle().await;
	assert_eq!(world.rename_answers(), [Err(Failure::NotAuthenticated)]);
	assert_eq!(world.drive.writes(), []);
}

#[tokio::test(start_paused = true)]
async fn a_move_into_a_new_directory_creates_it_first() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.local.user_mkdir(&root_path("n"));
	world.local.take_watch_events();
	world.rename("a.txt", "n\\a.txt");
	world.settle().await;
	assert_eq!(world.rename_answers(), [Ok(())]);
	let writes = world.drive.writes();
	assert!(matches!(&writes[0], DriveOp::CreateDir { name, .. } if name == "n"));
	assert!(matches!(&writes[2], DriveOp::Move { id, .. } if *id == format!("stable/{stable}")));
	assert!(world.in_sync("n"));
}

#[tokio::test(start_paused = true)]
async fn a_move_out_of_the_root_trashes_the_item() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	let (request, responder) = world.local.user_move_out(&root_path("a.txt")).asked();
	world.engine.on_notify_rename(request, responder);
	world.settle().await;
	assert_eq!(world.rename_answers(), [Ok(())]);
	assert_eq!(
		world.drive.writes(),
		[DriveOp::Trash(format!("stable/{stable}"))]
	);
}

#[tokio::test(start_paused = true)]
async fn a_rename_the_drive_cannot_take_is_vetoed() {
	let world = world().await;
	world.placed_file("a.txt", b"one");
	world.drive.set_offline(true);
	world.rename("a.txt", "b.txt");
	world.settle().await;
	world.drive.set_offline(false);
	world.rename("a.txt", " b.txt");
	world.settle().await;
	world.drive.add_remote_file(&root_dir(), "C.txt", b"other");
	world.rename("a.txt", "c.txt");
	world.settle().await;
	assert_eq!(
		world.rename_answers(),
		[
			Err(Failure::Unreachable),
			Err(Failure::InvalidRequest),
			Err(Failure::Transient)
		]
	);
	assert!(world.local.entry(&root_path("a.txt")).is_some());
	assert_eq!(world.drive.writes(), []);
}

#[tokio::test(start_paused = true)]
async fn a_rename_the_drive_already_has_is_allowed() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world
		.drive
		.rename(stable_id_of(stable), "b.txt".to_owned())
		.await
		.unwrap();
	world.rename("a.txt", "b.txt");
	world.settle().await;
	assert_eq!(world.rename_answers(), [Ok(())]);
	assert_eq!(world.drive.writes().len(), 1, "only the set-up rename");
}

#[tokio::test(start_paused = true)]
async fn a_rename_that_fails_after_it_was_allowed_is_put_back() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.local.fail_completion(&root_path("a.txt"));
	world.rename("a.txt", "b.txt");
	world.settle().await;
	assert_eq!(
		world.remote_file(&root_dir(), "a.txt").stable_uuid,
		stable.to_string()
	);
	assert!(world.reports().contains(&LocalReport::Undone {
		path: root_path("a.txt"),
		op: UndoneOp::Rename,
	}));
}

/// An allowed rename that failed locally, whose put-back the drive keeps refusing: reported once it
/// failed as often as a change that parks, tried again meanwhile, and put back once the drive takes
/// it.
#[tokio::test(start_paused = true)]
async fn a_put_back_the_drive_keeps_refusing_is_reported_and_tried_until_it_passes() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.local.fail_completion(&root_path("a.txt"));
	world.rename("a.txt", "b.txt");
	world.run(Duration::from_secs(1)).await;
	assert_eq!(world.rename_answers(), [Ok(())]);
	world
		.drive
		.fail_always(DriveCall::Rename, || CacheError::remote("refused"));
	world.run(Duration::from_secs(10 * 60)).await;
	assert_eq!(world.parked("a.txt"), 1);
	assert_eq!(
		world.remote_file(&root_dir(), "b.txt").stable_uuid,
		stable.to_string(),
		"not put back yet"
	);
	world.drive.stop_failing(DriveCall::Rename);
	world.run(Duration::from_secs(10 * 60)).await;
	assert_eq!(
		world.remote_file(&root_dir(), "a.txt").stable_uuid,
		stable.to_string()
	);
	assert!(world.reports().contains(&LocalReport::Undone {
		path: root_path("a.txt"),
		op: UndoneOp::Rename,
	}));
	assert_eq!(world.parked("a.txt"), 1);
}

/// `ReplaceFileW` failing with "Unable to remove the file to be replaced" in the VM: the rename of
/// the old file to its temporary name was allowed and never happened, the placeholder left no
/// longer in sync, and its remote file declares no hash. The rename is put back on the drive, and
/// the file, whose bytes the drive cannot tell from its own and which holds no byte written since
/// it was in sync, is marked in sync again without being sent.
#[tokio::test(start_paused = true)]
async fn an_allowed_rename_that_never_happened_settles_once_it_is_put_back() {
	let world = world().await;
	let (stable, version) = (Uuid::from_u128(0x101F), Uuid::from_u128(0x10_101F));
	world.placed_hashless_file("replace.txt", b"one", stable, version);
	world.local.fail_completion(&root_path("replace.txt"));
	world.rename("replace.txt", "replace.txt~RFc0bd647.TMP");
	world.local.user_touch(&root_path("replace.txt"));
	world.run(Duration::from_secs(5 * 60)).await;
	assert!(world.reports().contains(&LocalReport::Undone {
		path: root_path("replace.txt"),
		op: UndoneOp::Rename,
	}));
	assert_eq!(
		world.remote_file(&root_dir(), "replace.txt").stable_uuid,
		stable.to_string()
	);
	assert_eq!(world.uploads(), 0);
	assert_eq!(world.drive.content(stable), b"one");
	assert!(world.in_sync("replace.txt"));
	assert_eq!(world.parked("replace.txt"), 0);
}

/// A drive that keeps answering an edit's upload with the version the file had: the job backs off
/// and parks rather than uploading again at once, tries again at most once per backoff, and sends
/// the edit once the drive takes it.
#[tokio::test(start_paused = true)]
async fn an_upload_the_drive_keeps_answering_with_the_old_version_backs_off_and_parks() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.drive.take_no_versions(true);
	world.local.user_write(&root_path("a.txt"), b"two").unwrap();
	world.run(Duration::from_secs(10 * 60)).await;
	let first = world.uploads();
	let parked = world.parked("a.txt");
	world.run(Duration::from_secs(30 * 60)).await;
	let later = world.uploads() - first;
	// Four looks that came to nothing, eight that back off, the last of them parking, and one more
	// its own event brings back once the backoff ran out, parking again; then one look per backoff
	// at most.
	assert_eq!((first, parked), (13, 2));
	assert_eq!(later, 6, "uploads in half an hour once parked");
	world.drive.take_no_versions(false);
	world
		.local
		.user_write(&root_path("a.txt"), b"three")
		.unwrap();
	world.run(Duration::from_secs(6 * 60)).await;
	assert_eq!(world.drive.content(stable), b"three");
	assert!(world.in_sync("a.txt"));
}

/// A file written again during each look's settle wait, its writer closing it in between: each look
/// waits for the writer as for one holding the file open, which never counts as a failure, so the
/// file is never parked, and once the writer stops its last write is sent, once.
#[tokio::test(start_paused = true)]
async fn a_file_written_during_each_settle_is_waited_for_and_never_parked() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	for n in 0..180 {
		world
			.local
			.user_write(&root_path("a.txt"), format!("v{n}").as_bytes())
			.unwrap();
		world.run(Duration::from_secs(1)).await;
	}
	// The last write may land in a look's settle wait, which then waits for the writer, up to half
	// a minute, before the look that sends it.
	world.run(Duration::from_secs(60)).await;
	assert_eq!(world.drive.content(stable), b"v179");
	assert!(world.in_sync("a.txt"));
	assert_eq!(world.parked("a.txt"), 0);
	assert_eq!(world.uploads(), 1);
}

/// A mark in sync the platform keeps refusing, `error` each time: the job backs off and parks
/// rather than looking again at once, sends nothing, and marks the file once the platform lets it.
/// `expect`: the looks in the first ten minutes, the parked reports in them, and the looks in the
/// half hour after.
async fn a_refused_mark_in_sync_backs_off(error: LocalError, expect: (usize, usize, usize)) {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.local.fail_file_updates(Some(error));
	world.local.user_touch(&root_path("a.txt"));
	world.run(Duration::from_secs(10 * 60)).await;
	let looks = world.refreshes_of(stable);
	let parked = world.parked("a.txt");
	world.run(Duration::from_secs(30 * 60)).await;
	let later = world.refreshes_of(stable) - looks;
	assert_eq!((looks, parked, later), expect);
	assert_eq!(world.uploads(), 0);
	world.local.fail_file_updates(None);
	world.local.user_touch(&root_path("a.txt"));
	world.run(Duration::from_secs(6 * 60)).await;
	assert!(world.in_sync("a.txt"));
	assert_eq!(world.uploads(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_mark_in_sync_refused_as_changed_backs_off_and_parks() {
	// As for an upload the drive does not take.
	a_refused_mark_in_sync_backs_off(LocalError::Modified, (13, 2, 6)).await;
}

#[tokio::test(start_paused = true)]
async fn a_mark_in_sync_refused_by_the_os_backs_off_and_parks() {
	a_refused_mark_in_sync_backs_off(
		// Failures from the first look: eight that back off, the last of them parking, then one look
		// per backoff.
		LocalError::Os(crate::os::OsError::Rejected { code: -1 }),
		(10, 3, 6),
	)
	.await;
}

/// A mark in sync the platform acknowledges and does not keep, its write raising a watcher event:
/// the job reads the mark back, backs off and parks rather than looking again on each event its
/// own write raises, sends nothing, and marks the file once the platform keeps the mark.
#[tokio::test(start_paused = true)]
async fn a_mark_in_sync_the_platform_does_not_keep_backs_off_and_parks() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.local.lose_in_sync_marks(true);
	world.local.user_touch(&root_path("a.txt"));
	world.run(Duration::from_secs(10 * 60)).await;
	let looks = world.refreshes_of(stable);
	let parked = world.parked("a.txt");
	world.run(Duration::from_secs(30 * 60)).await;
	let later = world.refreshes_of(stable) - looks;
	// Four looks that came to nothing, eight that back off, the last of them parking, and one
	// more its own event brings back once the backoff ran out, parking again.
	assert_eq!((looks, parked), (13, 2));
	assert_eq!(later, 6, "looks in half an hour once parked");
	assert_eq!(world.uploads(), 0);
	world.local.lose_in_sync_marks(false);
	world.local.user_touch(&root_path("a.txt"));
	world.run(Duration::from_secs(6 * 60)).await;
	assert!(world.in_sync("a.txt"));
	assert_eq!(world.uploads(), 0);
}

/// Runs `world` a second at a time, for at most a minute, until `done` holds.
async fn run_until(world: &World, mut done: impl FnMut(&World) -> bool) {
	for _ in 0..60 {
		if done(world) {
			return;
		}
		world.run(Duration::from_secs(1)).await;
	}
	assert!(done(world), "not reached in a minute");
}

/// A new file written to while its job waits for the drive's listing: the job holds no handle on
/// it meanwhile, so the write goes through, and what was written last is what the drive gets.
#[tokio::test(start_paused = true)]
async fn a_new_file_takes_writes_while_its_job_waits_for_the_drive() {
	let world = world().await;
	let lists = |world: &World| {
		world
			.drive
			.ops()
			.iter()
			.filter(|op| matches!(op, DriveOp::List(_)))
			.count()
	};
	let before = lists(&world);
	let gate = world.drive.gate_calls(DriveCall::List);
	world.local.user_create(&root_path("new.txt"), b"one");
	run_until(&world, |world| lists(world) > before).await;
	world.run(Duration::from_secs(1)).await;
	assert_eq!(
		world.local.user_write(&root_path("new.txt"), b"two"),
		Ok(())
	);
	gate.add_permits(1000);
	world.settle().await;
	let uploaded = world.remote_file(&root_dir(), "new.txt");
	let (stable, _) = jobs::file_ids(&uploaded).unwrap();
	assert_eq!(world.drive.content(stable), b"two");
	assert!(world.in_sync("new.txt"));
}

/// As above, for an edit waiting for the drive's head of its file.
#[tokio::test(start_paused = true)]
async fn an_edit_takes_writes_while_its_job_waits_for_the_drive() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	let gate = world.drive.gate_calls(DriveCall::Refresh);
	world.local.user_write(&root_path("a.txt"), b"two").unwrap();
	run_until(&world, |world| world.refreshes_of(stable) > 0).await;
	world.run(Duration::from_secs(1)).await;
	assert_eq!(
		world.local.user_write(&root_path("a.txt"), b"three"),
		Ok(())
	);
	gate.add_permits(1000);
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"three");
	assert!(world.in_sync("a.txt"));
}

/// An edit that turns out equal to the drive's head is marked in sync only while it still is:
/// written while the drive answered, it is sent instead.
#[tokio::test(start_paused = true)]
async fn an_edit_written_while_the_drive_answers_is_never_marked_as_the_head() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	let gate = world.drive.gate_calls(DriveCall::Refresh);
	world.local.user_touch(&root_path("a.txt"));
	run_until(&world, |world| world.refreshes_of(stable) > 0).await;
	world.local.user_write(&root_path("a.txt"), b"two").unwrap();
	gate.add_permits(1000);
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"two");
	assert!(world.in_sync("a.txt"));
}

/// A directory renamed onto a name another device gives a directory between the provider's look
/// and its rename: the server refuses it (measured), the rename is vetoed, and the directory keeps
/// its name on both sides.
#[tokio::test(start_paused = true)]
async fn a_directory_rename_onto_a_name_taken_meanwhile_is_vetoed() {
	let world = world().await;
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let gate = world.drive.gate_calls(DriveCall::Rename);
	world.rename("d", "e");
	world.run(Duration::from_secs(1)).await;
	let taken = world.drive.add_remote_dir(&root_dir(), "E");
	gate.add_permits(10);
	world.settle().await;
	assert_eq!(world.rename_answers(), [Err(Failure::Transient)]);
	assert!(matches!(
		world.drive.child(&root_dir(), "d"),
		Some(FfiNonRootObject::Dir(held)) if held.uuid == dir.uuid
	));
	assert!(matches!(
		world.drive.child(&root_dir(), "E"),
		Some(FfiNonRootObject::Dir(held)) if held.uuid == taken.uuid
	));
	assert_eq!(world.identity("d"), Some(PlaceholderIdentity::Dir { uuid }));
}

#[tokio::test(start_paused = true)]
async fn a_directory_rename_moves_what_is_queued_under_it() {
	let world = world().await;
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	world.local.user_create(&root_path("d\\new.txt"), b"x");
	world.pump();
	world.rename("d", "e");
	world.settle().await;
	assert!(
		world
			.drive
			.child(&format!("stable/{uuid}"), "new.txt")
			.is_some()
	);
	assert!(world.in_sync("e\\new.txt"));
	assert!(world.in_sync("e"));
}

// ---- the provider's own writes ----

#[tokio::test(start_paused = true)]
async fn events_for_placeholders_in_sync_write_nothing() {
	let world = world().await;
	for n in 0..5 {
		world.placed_file(&format!("f{n}.bin"), b"data");
		world
			.engine
			.on_local_event(LocalEvent::Modified(root_path(&format!("f{n}.bin"))));
	}
	world
		.engine
		.on_local_event(LocalEvent::Modified(RootPath::root()));
	world.settle().await;
	assert_eq!(world.drive.writes(), []);
}

// ---- start-up and shutdown ----

#[tokio::test(start_paused = true)]
async fn the_start_up_walk_takes_in_what_changed() {
	let world = unstarted();
	world.local.user_create(&root_path("new.txt"), b"new");
	let edited = world
		.drive
		.add_remote_file(&root_dir(), "edited.txt", b"one");
	world
		.local
		.place_file(&root_path("edited.txt"), file_identity(&edited), b"one");
	world
		.local
		.user_write(&root_path("edited.txt"), b"two")
		.unwrap();
	world.local.place_file(
		&root_path("detached.txt"),
		PlaceholderIdentity::Detached {
			nonce: Uuid::from_u128(9),
		},
		b"kept",
	);
	world.placed_file("clean.txt", b"clean");
	world.local.take_watch_events();
	world.engine.start();
	world.settle().await;
	let writes = world.drive.writes();
	assert_eq!(
		writes.len(),
		5,
		"two new files, each with its name, and a version: {writes:?}"
	);
	assert!(world.in_sync("detached.txt"));
}

#[tokio::test(start_paused = true)]
async fn a_lost_watcher_event_is_found_by_a_walk() {
	let world = world().await;
	world.local.user_create(&root_path("a.txt"), b"x");
	world.local.lose_watch_events();
	world.settle().await;
	assert_eq!(world.uploads(), 1);
}

#[tokio::test(start_paused = true)]
async fn shutdown_ends_every_task_and_vetoes_what_it_left_unanswered() {
	let world = world().await;
	world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"x");
	world.settle().await;
	world.engine.shutdown().await;
	assert!(world.local.entry(&root_path("a.txt")).is_some());
	let (request, responder) = (
		DeleteRequest::new(
			IdentityBlob::try_from([1u8].as_slice()).unwrap(),
			root_path("a.txt"),
			false,
			false,
		),
		world.local.delete_responder(&root_path("a.txt")),
	);
	// Spawned no more: the responder drops unanswered, which vetoes.
	world.engine.on_notify_delete(request, responder);
	assert_eq!(
		world.local.ops().last(),
		Some(&LocalOp::DeleteAbandoned(root_path("a.txt")))
	);
}

// ---- review: what must never happen ----

#[tokio::test(start_paused = true)]
async fn an_upload_stopped_on_its_way_leaves_the_newer_remote_version() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	let gate = world.drive.gate_uploads();
	world
		.local
		.user_write(&root_path("a.txt"), b"local")
		.unwrap();
	world.settle().await;
	assert_eq!(
		world.drive.pending_uploads(),
		[stable],
		"the edit is marked"
	);
	// The provider stops mid-upload; another device uploads a newer version meanwhile.
	world.engine.shutdown().await;
	gate.add_permits(Semaphore::MAX_PERMITS / 2);
	world.drive.bump_head(stable, b"remote");
	let world = world.restarted().await;
	world.settle().await;
	world.settle().await;
	assert_eq!(
		world.drive.content(stable),
		b"remote",
		"never uploaded over"
	);
	let aside = world.remote_file(&root_dir(), "a.old.txt");
	assert_eq!(
		world.drive.content(jobs::file_ids(&aside).unwrap().0),
		b"local"
	);
	assert!(
		world.drive.pending_uploads().is_empty(),
		"the cache's copy was given up, the placeholder holding the edit"
	);
	assert_eq!(world.drive.history(stable), [b"remote".to_vec()]);
}

/// An edit of `f.txt` whose every upload fails, then a newer version from another device while
/// the provider is stopped: the world restarted, with the file still holding the edit.
async fn failed_edit_then_newer_remote(world: World) -> (World, Uuid) {
	let (stable, _) = world.placed_file("f.txt", b"one");
	world.drive.fail_always(DriveCall::UploadVersion, || {
		CacheError::remote("unreachable")
	});
	world
		.local
		.user_write(&root_path("f.txt"), b"edit")
		.unwrap();
	world.settle().await;
	world.engine.shutdown().await;
	world.drive.stop_failing(DriveCall::UploadVersion);
	world.drive.bump_head(stable, b"remote");
	(world, stable)
}

/// Once its writer lets go, the edit made from the old version is kept aside as a new file and the
/// newer version takes the name.
async fn assert_edit_kept_aside(world: &World, stable: Uuid) {
	world.settle().await;
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"remote");
	let aside = world.remote_file(&root_dir(), "f.old.txt");
	assert_eq!(
		world.drive.content(jobs::file_ids(&aside).unwrap().0),
		b"edit"
	);
	assert_eq!(
		world.local.entry(&root_path("f.old.txt")).unwrap().bytes,
		b"edit"
	);
	assert!(world.drive.pending_uploads().is_empty());
	assert_eq!(
		world.drive.history(stable),
		[b"remote".to_vec()],
		"the file got no version but the other device's"
	);
}

#[tokio::test(start_paused = true)]
async fn a_failed_edit_upload_leaves_nothing_marked_in_the_cache() {
	let world = world().await;
	let (stable, _) = world.placed_file("f.txt", b"one");
	world.drive.fail_always(DriveCall::UploadVersion, || {
		CacheError::remote("unreachable")
	});
	world
		.local
		.user_write(&root_path("f.txt"), b"edit")
		.unwrap();
	world.settle().await;
	assert!(world.drive.pending_uploads().is_empty());
	assert!(
		!world.in_sync("f.txt"),
		"the local file still holds the edit"
	);
	world.drive.stop_failing(DriveCall::UploadVersion);
	world.run(WATCH_SAFETY_NET * 2).await;
	assert_eq!(world.drive.content(stable), b"edit");
	assert!(world.in_sync("f.txt"));
}

#[tokio::test(start_paused = true)]
async fn a_copy_taken_mid_write_is_never_uploaded() {
	let world = world().await;
	let (stable, _) = world.placed_file("f.txt", b"one");
	world.local.user_write(&root_path("f.txt"), b"two").unwrap();
	world
		.local
		.tear_next_copy(&root_path("f.txt"), b"tw", b"three");
	world.settle().await;
	world.settle().await;
	assert_eq!(world.drive.history(stable), [b"three".to_vec()]);
	assert!(world.in_sync("f.txt"));
	assert!(world.drive.pending_uploads().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_unsent_edit_whose_file_was_deleted_is_kept_as_a_new_file() {
	let world = unstarted();
	let (stable, version) = world.placed_file("f.txt", b"one");
	world.drive.mark_pending(stable, b"unsent");
	// Deleted while the provider was stopped, as the platform allows.
	world.local.vanish(&root_path("f.txt"));
	world.local.take_watch_events();
	world.engine.start();
	world.settle().await;
	world.settle().await;
	assert_eq!(
		world.drive.content(stable),
		b"one",
		"the file is left as it was"
	);
	let kept = world.remote_file(&root_dir(), "f.old.txt");
	assert_eq!(
		world.drive.content(jobs::file_ids(&kept).unwrap().0),
		b"unsent"
	);
	assert!(world.drive.pending_uploads().is_empty());
	assert!(world.in_sync("f.old.txt"));
	assert!(
		world
			.reports()
			.contains(&LocalReport::Remote(RemoteApplied::KeptLocal {
				path: root_path("f.old.txt"),
				reason: KeptReason::UnsentEdit,
			}))
	);
	assert_eq!(world.drive.history(stable), Vec::<Vec<u8>>::new());
	assert_eq!(
		world.remote_file(&root_dir(), "f.txt").uuid,
		version.to_string()
	);
}

#[tokio::test(start_paused = true)]
async fn an_unsent_edit_whose_copy_a_kill_cut_short_is_kept_once() {
	let world = unstarted();
	let (stable, _) = world.placed_file("g.txt", b"one");
	world.drive.mark_pending(stable, b"unsent");
	world.local.vanish(&root_path("g.txt"));
	world.local.take_watch_events();
	let copies = world.drive.gate_calls(DriveCall::CopyPending);
	world.engine.start();
	world.run(Duration::from_secs(5)).await;
	// The copy is on disk and the provider is killed before it returns.
	let world = world.killed().started();
	copies.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	let kept = world.remote_file(&root_dir(), "g.old.txt");
	assert_eq!(
		world.drive.content(jobs::file_ids(&kept).unwrap().0),
		b"unsent"
	);
	assert_eq!(
		world.drive.child(&root_dir(), "g.old.1.txt"),
		None,
		"kept once: {:?}",
		world.drive.writes()
	);
	assert_eq!(
		world
			.drive
			.writes()
			.iter()
			.filter(|op| matches!(op, DriveOp::Upload { .. }))
			.count(),
		1,
		"the copy the kill left is never uploaded"
	);
	assert!(world.drive.pending_uploads().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_edit_made_while_an_unsent_one_is_taken_over_waits_for_it() {
	let world = unstarted();
	let (stable, _) = world.placed_file("f.txt", b"one");
	world.drive.mark_pending(stable, b"unsent");
	world.local.take_watch_events();
	// The take-over is held where it reads the listing of the directory to keep the edit in.
	let lists = world.drive.gate_calls(DriveCall::CachedChildren);
	world.engine.start();
	world.run(Duration::from_secs(5)).await;
	world
		.local
		.user_write(&root_path("f.txt"), b"newer")
		.unwrap();
	world.run(Duration::from_secs(30)).await;
	assert_eq!(
		world.drive.history(stable),
		Vec::<Vec<u8>>::new(),
		"the edit waits for the take-over"
	);
	lists.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	let kept = world.remote_file(&root_dir(), "f.old.txt");
	assert_eq!(
		world.drive.content(jobs::file_ids(&kept).unwrap().0),
		b"unsent"
	);
	assert_eq!(world.drive.history(stable), [b"newer".to_vec()]);
	assert!(world.drive.pending_uploads().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_unsent_edit_is_kept_while_the_drive_is_offline() {
	let world = unstarted();
	let (stable, _) = world.placed_file("f.txt", b"one");
	world.drive.mark_pending(stable, b"unsent");
	world.local.take_watch_events();
	world.drive.set_offline(true);
	world.engine.start();
	world.run(Duration::from_secs(5)).await;
	assert_eq!(
		world.local.entry(&root_path("f.old.txt")).unwrap().bytes,
		b"unsent",
		"kept from the cache alone"
	);
	assert!(world.drive.pending_uploads().is_empty());
	world
		.local
		.user_write(&root_path("f.txt"), b"newer")
		.unwrap();
	world.drive.set_offline(false);
	world.run(WATCH_SAFETY_NET * 2).await;
	assert_eq!(world.drive.history(stable), [b"newer".to_vec()]);
	let kept = world.remote_file(&root_dir(), "f.old.txt");
	assert_eq!(
		world.drive.content(jobs::file_ids(&kept).unwrap().0),
		b"unsent"
	);
}

#[tokio::test(start_paused = true)]
async fn an_edit_of_a_file_whose_unsent_edit_is_not_kept_yet_waits_for_it() {
	let world = unstarted();
	let (stable, _) = world.placed_file("f.txt", b"one");
	world.drive.mark_pending(stable, b"unsent");
	world.local.take_watch_events();
	world.drive.fail_always(DriveCall::CachedChildren, || {
		CacheError::io("the cache is unreadable")
	});
	world.engine.start();
	world.run(Duration::from_secs(5)).await;
	world
		.local
		.user_write(&root_path("f.txt"), b"newer")
		.unwrap();
	world.run(Duration::from_secs(10 * 60)).await;
	assert_eq!(world.drive.history(stable), Vec::<Vec<u8>>::new());
	assert_eq!(
		world.drive.pending_uploads(),
		[stable],
		"the unsent edit stays"
	);
	assert!(world.reports().iter().any(|report| matches!(
		report,
		LocalReport::Parked { path, .. } if *path == root_path("f.txt")
	)));
	world.drive.stop_failing(DriveCall::CachedChildren);
	world.run(WATCH_SAFETY_NET * 2).await;
	let kept = world.remote_file(&root_dir(), "f.old.txt");
	assert_eq!(
		world.drive.content(jobs::file_ids(&kept).unwrap().0),
		b"unsent"
	);
	assert_eq!(world.drive.history(stable), [b"newer".to_vec()]);
	assert!(world.drive.pending_uploads().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_edit_waiting_for_the_take_over_holds_no_job_slot() {
	let world = unstarted_with(LocalConfig {
		max_concurrent_jobs: NonZeroUsize::new(1).unwrap(),
		..LocalConfig::default()
	});
	let (unsent, _) = world.placed_file("g.txt", b"one");
	world.drive.mark_pending(unsent, b"unsent");
	world.local.vanish(&root_path("g.txt"));
	world.placed_file("f.txt", b"one");
	world.local.take_watch_events();
	// The take-over is held where it reads the listing of the directory to keep g's edit in.
	let lists = world.drive.gate_calls(DriveCall::CachedChildren);
	world.engine.start();
	world.run(Duration::from_secs(5)).await;
	world
		.local
		.user_write(&root_path("f.txt"), b"edit")
		.unwrap();
	world.local.user_create(&root_path("n.txt"), b"new");
	world.run(Duration::from_secs(60)).await;
	assert!(
		world.drive.child(&root_dir(), "n.txt").is_some(),
		"a new file is taken in while an edit waits"
	);
	lists.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	let stable = jobs::file_ids(&world.remote_file(&root_dir(), "f.txt"))
		.unwrap()
		.0;
	assert_eq!(world.drive.content(stable), b"edit");
	let kept = world.remote_file(&root_dir(), "g.old.txt");
	assert_eq!(
		world.drive.content(jobs::file_ids(&kept).unwrap().0),
		b"unsent"
	);
}

#[tokio::test(start_paused = true)]
async fn a_save_made_while_stopped_over_an_unsent_edit_keeps_both_edits() {
	let (world, stable) = kept_world_with("a.txt", b"one").await;
	let stopped = world.killed();
	// The run before left an edit marked mid-upload; while stopped, an editor saved a newer one
	// over the placeholder, and another device uploaded a version of its own.
	stopped.drive.mark_pending(stable, b"unsent");
	save_over_unseen(&stopped.local, "a.txt", b"saved");
	stopped.drive.bump_head(stable, b"theirs");
	// The take-over is held where it reads the listing of the directory to keep the edit in.
	let lists = stopped.drive.gate_calls(DriveCall::CachedChildren);
	let world = stopped.started();
	world.run(Duration::from_secs(2 * 60)).await;
	lists.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	assert_eq!(
		world.drive.content(stable),
		b"theirs",
		"the newer version stays"
	);
	let mut kept: Vec<Vec<u8>> = ["a.old.txt", "a.old.1.txt"]
		.into_iter()
		.filter_map(|name| world.drive.child(&root_dir(), name))
		.map(|object| match object {
			FfiNonRootObject::File(file) => world.drive.content(jobs::file_ids(&file).unwrap().0),
			other => panic!("not a file: {other:?}"),
		})
		.collect();
	kept.sort();
	assert_eq!(
		kept,
		[b"saved".to_vec(), b"unsent".to_vec()],
		"both edits are kept: {:?}",
		world.drive.writes()
	);
	assert!(world.drive.pending_uploads().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_unsent_edit_the_drive_holds_already_is_given_up() {
	let world = unstarted();
	let (stable, _) = world.placed_file("f.txt", b"one");
	world.drive.mark_pending(stable, b"one");
	world.local.take_watch_events();
	world.engine.start();
	world.settle().await;
	assert!(world.drive.pending_uploads().is_empty());
	assert_eq!(world.drive.writes(), []);
	assert_eq!(world.drive.child(&root_dir(), "f.old.txt"), None);
}

#[tokio::test(start_paused = true)]
async fn an_unsent_edit_of_a_file_holding_changes_is_left_to_the_file() {
	let world = unstarted();
	let (stable, _) = world.placed_file("f.txt", b"one");
	world.drive.mark_pending(stable, b"older edit");
	world
		.local
		.user_write(&root_path("f.txt"), b"newer edit")
		.unwrap();
	world.local.take_watch_events();
	world.engine.start();
	world.settle().await;
	world.settle().await;
	assert_eq!(world.drive.history(stable), [b"newer edit".to_vec()]);
	assert!(world.drive.pending_uploads().is_empty());
	assert_eq!(world.drive.child(&root_dir(), "f.old.txt"), None);
}

#[tokio::test(start_paused = true)]
async fn a_failed_edit_held_open_over_a_restart_never_replaces_a_newer_version() {
	let (world, stable) = failed_edit_then_newer_remote(world().await).await;
	let writer = world.local.user_open_for_write(&root_path("f.txt"));
	let world = world.restarted().await;
	world.run(Duration::from_secs(5 * 60)).await;
	assert_eq!(
		world.drive.content(stable),
		b"remote",
		"nothing reached the drive over the newer version"
	);
	assert_eq!(
		world.local.entry(&root_path("f.txt")).unwrap().bytes,
		b"edit"
	);
	drop(writer);
	assert_edit_kept_aside(&world, stable).await;
}

#[tokio::test(start_paused = true)]
async fn a_failed_edit_parked_over_a_restart_never_replaces_a_newer_version() {
	let (world, stable) = failed_edit_then_newer_remote(world().await).await;
	world.drive.fail_always(DriveCall::Refresh, || {
		CacheError::FailedToDecrypt("garbled".into())
	});
	let world = world.restarted().await;
	world.run(Duration::from_secs(5 * 60)).await;
	assert!(
		world.reports().iter().any(
			|report| matches!(report, LocalReport::Parked { path, .. } if *path == root_path("f.txt"))
		),
		"{:?}",
		world.reports()
	);
	assert_eq!(
		world.drive.content(stable),
		b"remote",
		"nothing reached the drive over the newer version"
	);
	assert_eq!(
		world.local.entry(&root_path("f.txt")).unwrap().bytes,
		b"edit"
	);
	world.drive.stop_failing(DriveCall::Refresh);
	world.local.user_touch(&root_path("f.txt"));
	assert_edit_kept_aside(&world, stable).await;
}

#[tokio::test(start_paused = true)]
async fn a_file_created_at_the_name_while_a_new_file_uploads_is_never_versioned_over() {
	let world = world().await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"mine");
	world.settle().await;
	assert_eq!(world.uploads(), 1, "the upload waits at the gate");
	let theirs = world.drive.add_remote_file(&root_dir(), "a.txt", b"theirs");
	let (stable, version) = jobs::file_ids(&theirs).unwrap();
	gate.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"theirs");
	assert_eq!(world.drive.history(stable), Vec::<Vec<u8>>::new());
	assert_eq!(
		world.remote_file(&root_dir(), "a.txt").uuid,
		version.to_string()
	);
	let kept = world.remote_file(&root_dir(), "a.old.txt");
	assert_eq!(
		world.drive.content(jobs::file_ids(&kept).unwrap().0),
		b"mine"
	);
	assert_eq!(
		world.identity("a.txt"),
		Some(PlaceholderIdentity::File { stable, version })
	);
	assert_eq!(
		world.local.entry(&root_path("a.old.txt")).unwrap().bytes,
		b"mine"
	);
	assert!(world.in_sync("a.old.txt"));
	assert_eq!(
		world.uploads(),
		1,
		"the upload was named aside, not sent again"
	);
	assert!(world.reports().contains(&LocalReport::Conflict {
		path: root_path("a.txt"),
		kept_as: root_path("a.old.txt"),
		kind: ConflictKind::CreateVsExisting,
	}));
}

/// The local file id of the file at `path`.
fn file_id_at(world: &World, path: &str) -> LocalFileId {
	match world.local.inspect(&root_path(path)) {
		Ok(LocalEntry::Plain { id, .. } | LocalEntry::Placeholder { id, .. }) => id,
		other => panic!("{path} is not there: {other:?}"),
	}
}

#[tokio::test(start_paused = true)]
async fn an_upload_that_never_got_its_name_is_named_rather_than_sent_again() {
	let world = unstarted();
	world.local.user_create(&root_path("a.txt"), b"mine");
	let earlier = world.drive.add_remote_file(
		&root_dir(),
		&crate::names::upload_name(file_id_at(&world, "a.txt").get()),
		b"mine",
	);
	world.local.take_watch_events();
	world.engine.start();
	world.settle().await;
	assert_eq!(world.uploads(), 0);
	let named = world.remote_file(&root_dir(), "a.txt");
	assert_eq!(named.stable_uuid, earlier.stable_uuid);
	assert_eq!(world.identity("a.txt"), Some(file_identity(&named)));
	assert!(world.in_sync("a.txt"));
}

#[tokio::test(start_paused = true)]
async fn new_files_holding_the_same_bytes_each_get_an_upload_of_their_own() {
	let world = world().await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"");
	world.local.user_create(&root_path("b.txt"), b"");
	world.settle().await;
	assert_eq!(world.uploads(), 2, "both wait at the gate");
	gate.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	let (a, b) = (
		world.remote_file(&root_dir(), "a.txt"),
		world.remote_file(&root_dir(), "b.txt"),
	);
	assert_ne!(a.stable_uuid, b.stable_uuid);
	assert_eq!(world.identity("a.txt"), Some(file_identity(&a)));
	assert_eq!(world.identity("b.txt"), Some(file_identity(&b)));
	assert!(world.in_sync("a.txt") && world.in_sync("b.txt"));
}

#[tokio::test(start_paused = true)]
async fn a_new_file_on_its_way_to_its_name_gets_no_placeholder() {
	let world = world().await;
	world
		.drive
		.add_remote_file(&root_dir(), ".filen-upload-0123456789abcdef", b"theirs");
	world.settle().await;
	assert_eq!(
		world
			.local
			.entry(&root_path(".filen-upload-0123456789abcdef")),
		None
	);
	assert!(
		!world
			.reports()
			.iter()
			.any(|report| matches!(report, LocalReport::Skipped { .. })),
		"{:?}",
		world.reports()
	);
}

#[tokio::test(start_paused = true)]
async fn a_file_renamed_long_ago_does_not_claim_a_new_file_at_its_old_name() {
	let world = world().await;
	let (stable, _) = world.placed_file("report.txt", b"old");
	open_in_editor(&world, "report.txt").await;
	world.rename("report.txt", "report-v1.txt");
	world.settle().await;
	world.run(Duration::from_secs(60 * 60)).await;
	world
		.local
		.user_create(&root_path("report.txt"), b"unrelated");
	world.settle().await;
	let kept = world.remote_file(&root_dir(), "report-v1.txt");
	assert_eq!(kept.stable_uuid, stable.to_string());
	assert_eq!(world.drive.content(stable), b"old");
	let new = world.remote_file(&root_dir(), "report.txt");
	assert_ne!(new.stable_uuid, stable.to_string());
	assert_eq!(
		world.drive.content(jobs::file_ids(&new).unwrap().0),
		b"unrelated"
	);
	assert_eq!(
		world.identity("report-v1.txt"),
		Some(PlaceholderIdentity::File {
			stable,
			version: jobs::file_ids(&kept).unwrap().1
		})
	);
}

#[tokio::test(start_paused = true)]
async fn a_rename_vetoed_after_the_drive_moved_the_item_is_put_back() {
	let world = world().await;
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let (stable, _) = world.placed_file("a.txt", b"one");
	let renames = world.drive.gate_calls(DriveCall::Rename);
	world.rename("a.txt", "d\\b.txt");
	world.run(Duration::from_secs(60)).await;
	assert_eq!(world.rename_answers(), [Err(Failure::Unreachable)]);
	renames.add_permits(Semaphore::MAX_PERMITS / 2);
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "a.txt");
	assert_eq!(remote.stable_uuid, stable.to_string(), "back where it was");
	assert!(world.local.entry(&root_path("a.txt")).is_some());
}

#[tokio::test(start_paused = true)]
async fn a_directory_that_cannot_be_created_stops_neither_the_take_over_nor_the_safety_net() {
	let world = unstarted();
	let (stable, _) = world.placed_file("kept.txt", b"kept");
	world.drive.mark_pending(stable, b"unsent");
	world.local.user_mkdir(&root_path("d"));
	world.local.user_create(&root_path("d\\f.txt"), b"x");
	world.drive.fail_always(DriveCall::CreateDir, || {
		CacheError::SyncAnchorExpired("bug".into())
	});
	world.engine.start();
	world.settle().await;
	let kept = world.remote_file(&root_dir(), "kept.old.txt");
	assert_eq!(
		world.drive.content(jobs::file_ids(&kept).unwrap().0),
		b"unsent"
	);
	world.local.user_create(&root_path("lost.txt"), b"y");
	world.local.take_watch_events();
	world.run(WATCH_SAFETY_NET * 2).await;
	assert!(world.drive.child(&root_dir(), "lost.txt").is_some());
}

#[tokio::test(start_paused = true)]
async fn a_delete_and_create_over_a_file_never_looked_at_is_a_new_version() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.delete("a.txt");
	world.run(Duration::from_millis(100)).await;
	world.local.user_create(&root_path("a.txt"), b"two");
	world.settle().await;
	assert_eq!(world.trashes(), 0);
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(world.drive.child(&root_dir(), "a.old.txt"), None);
	assert!(
		!world
			.reports()
			.iter()
			.any(|report| matches!(report, LocalReport::Conflict { .. }))
	);
}

#[tokio::test(start_paused = true)]
async fn a_save_through_a_backup_rename_of_a_file_never_looked_at_is_a_new_version() {
	let world = world().await;
	let (stable, _) = world.placed_file("doc.txt", b"one");
	world.local.user_create(&root_path("tmp1"), b"two");
	world.rename("doc.txt", "doc.bak");
	world.run(Duration::from_millis(100)).await;
	world
		.local
		.user_rename(&root_path("tmp1"), &root_path("doc.txt"))
		.unwrap();
	world.delete("doc.bak");
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "doc.txt");
	assert_eq!(remote.stable_uuid, stable.to_string());
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(world.trashes(), 0, "{:?}", world.drive.writes());
}

#[tokio::test(start_paused = true)]
async fn a_save_over_a_file_placed_before_the_start_is_a_new_version() {
	let world = unstarted();
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.engine.start();
	world.settle().await;
	world.local.user_create(&root_path("a.txt.tmp"), b"two");
	world
		.local
		.user_replace(&root_path("a.txt.tmp"), &root_path("a.txt"));
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(world.drive.child(&root_dir(), "a.old.txt"), None);
	assert_eq!(
		world.drive.writes(),
		[DriveOp::UploadVersion(format!("stable/{stable}"))]
	);
}

#[tokio::test(start_paused = true)]
async fn an_edit_whose_hydration_stalls_backs_off_and_parks() {
	let world = world().await;
	world.placed_file("b.txt", b"0123456789");
	world
		.local
		.user_write(&root_path("b.txt"), b"abcdefghij")
		.unwrap();
	world.local.set_on_disk(&root_path("b.txt"), 4);
	world.local.stall_hydration();
	world.run(Duration::from_secs(20 * 60)).await;
	let hydrations = world
		.local
		.ops()
		.iter()
		.filter(|op| matches!(op, LocalOp::Hydrated(_)))
		.count();
	// A few tries, then a few more after each safety-net walk.
	assert!(hydrations <= 20, "{hydrations} hydrations");
	assert_eq!(world.uploads(), 0);
	assert!(world.reports().iter().any(|report| matches!(
		report,
		LocalReport::Parked { path, .. } if *path == root_path("b.txt")
	)));
}

#[tokio::test(start_paused = true)]
async fn a_file_renamed_then_deleted_while_it_uploads_has_its_remote_file_trashed() {
	let world = world().await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"x");
	world.settle().await;
	world
		.local
		.user_rename(&root_path("a.txt"), &root_path("b.txt"))
		.unwrap();
	// The job at the new name runs while the upload does, and follows it there.
	world.run(Duration::from_secs(5)).await;
	assert!(matches!(
		world.local.user_delete(&root_path("b.txt")),
		UserChange::Done
	));
	world.run(Duration::from_secs(1)).await;
	gate.add_permits(1);
	world.settle().await;
	assert_eq!(world.drive.child(&root_dir(), "a.txt"), None);
	assert_eq!(world.drive.trashed().len(), 1, "{:?}", world.drive.writes());
}

#[tokio::test(start_paused = true)]
async fn a_rename_the_provider_stopped_in_the_middle_of_is_put_back() {
	let world = world().await;
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let (stable, _) = world.placed_file("a.txt", b"one");
	let renames = world.drive.gate_calls(DriveCall::Rename);
	world.rename("a.txt", "d\\b.txt");
	world.run(Duration::from_secs(5)).await;
	renames.add_permits(Semaphore::MAX_PERMITS / 2);
	world.engine.shutdown().await;
	assert_eq!(
		world.local.ops().last(),
		Some(&LocalOp::RenameAbandoned(root_path("a.txt")))
	);
	let remote = world.remote_file(&root_dir(), "a.txt");
	assert_eq!(remote.stable_uuid, stable.to_string(), "back where it was");
}

// ---- review: what the suite did not cover ----

#[tokio::test(start_paused = true)]
async fn a_file_created_and_one_edited_while_offline_are_uploaded_once_online() {
	let world = world().await;
	let (stable, _) = world.placed_file("edited.txt", b"one");
	world.drive.set_offline(true);
	world.local.user_create(&root_path("new.txt"), b"new");
	world
		.local
		.user_write(&root_path("edited.txt"), b"two")
		.unwrap();
	world.run(Duration::from_secs(10 * 60)).await;
	assert_eq!(world.uploads(), 0);
	world.drive.set_offline(false);
	world.run(WATCH_SAFETY_NET * 2).await;
	assert_eq!(world.drive.content(stable), b"two");
	assert!(world.drive.child(&root_dir(), "new.txt").is_some());
	assert!(world.in_sync("new.txt"));
	assert!(world.in_sync("edited.txt"));
}

#[tokio::test(start_paused = true)]
async fn a_case_only_rename_renames_the_remote_item() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.rename("a.txt", "A.txt");
	world.settle().await;
	assert_eq!(world.rename_answers(), [Ok(())]);
	assert_eq!(
		world.drive.writes(),
		[DriveOp::Rename {
			id: format!("stable/{stable}"),
			name: "A.txt".to_owned()
		}]
	);
	assert!(world.in_sync("A.txt"));
}

#[tokio::test(start_paused = true)]
async fn a_new_file_whose_upload_a_restart_cut_short_is_uploaded_once() {
	let world = world().await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"x");
	world.settle().await;
	world.engine.shutdown().await;
	gate.add_permits(Semaphore::MAX_PERMITS / 2);
	let world = world.restarted().await;
	world.settle().await;
	let uploads: Vec<DriveOp> = world
		.drive
		.ops()
		.into_iter()
		.filter(|op| matches!(op, DriveOp::Upload { .. }))
		.collect();
	assert_eq!(uploads.len(), 2, "the cut one and the one after the start");
	assert!(world.in_sync("a.txt"));
	assert_eq!(
		world.drive.child(&root_dir(), "a.txt").into_iter().count(),
		1
	);
}

#[tokio::test(start_paused = true)]
async fn a_delete_the_provider_stops_within_the_grace_of_leaves_the_remote_item() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.delete("a.txt");
	world.run(Duration::from_secs(1)).await;
	assert_eq!(world.delete_answers(), [Ok(())]);
	let world = world.restarted().await;
	world.settle().await;
	assert_eq!(
		world.trashes(),
		0,
		"documented: the delete is lost, not the file"
	);
	assert_eq!(
		world.remote_file(&root_dir(), "a.txt").stable_uuid,
		stable.to_string()
	);
}

#[tokio::test(start_paused = true)]
async fn a_directory_whose_remote_got_a_child_never_seen_is_trashed_whole() {
	let world = world().await;
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	world
		.drive
		.add_remote_file(&format!("stable/{uuid}"), "unseen.txt", b"x");
	world.delete("d");
	world.settle().await;
	assert_eq!(world.delete_answers(), [Ok(())]);
	assert_eq!(
		world.drive.writes(),
		[DriveOp::Trash(format!("stable/{uuid}"))]
	);
}

#[tokio::test(start_paused = true)]
async fn a_name_the_core_cannot_name_found_by_the_walk_is_reported_once() {
	let world = unstarted();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	world.local.add_unrepresentable(&root_path("d"));
	world.engine.start();
	world.run(WATCH_SAFETY_NET * 2).await;
	let reports: Vec<LocalReport> = world
		.reports()
		.into_iter()
		.filter(|report| {
			matches!(
				report,
				LocalReport::Skipped {
					reason: LocalSkip::Unrepresentable,
					..
				}
			)
		})
		.collect();
	assert_eq!(
		reports,
		[LocalReport::Skipped {
			path: root_path("d"),
			reason: LocalSkip::Unrepresentable
		}]
	);
}

// ---- what a start finds that the provider did not see ----

/// A world keeping records, with the file `name` holding `bytes` placed in it and seen by its
/// engine, whose lineage was saved since.
async fn kept_world_with(name: &str, bytes: &[u8]) -> (World, Uuid) {
	let world = unstarted_keeping_records();
	let (stable, _) = world.placed_file(name, bytes);
	world.engine.start();
	world.settle().await;
	(world, stable)
}

/// Saves `bytes` over the file `name` as an editor does, through a new file renamed over it, with
/// no provider to see it.
fn save_over_unseen(local: &FakeLocal, name: &str, bytes: &[u8]) {
	let temp = format!("{name}.tmp");
	local.user_create(&root_path(&temp), bytes);
	local.user_replace(&root_path(&temp), &root_path(name));
}

/// Deletes the placeholder `path` with no provider to ask, as the platform allows while none is
/// connected.
fn delete_unseen(local: &FakeLocal, path: &str) {
	let (_, responder) = local.user_delete(&root_path(path)).asked();
	responder.allow();
}

fn conflicts(world: &World) -> Vec<ConflictKind> {
	world
		.reports()
		.into_iter()
		.filter_map(|report| match report {
			LocalReport::Conflict { kind, .. } => Some(kind),
			_ => None,
		})
		.collect()
}

#[tokio::test(start_paused = true)]
async fn a_save_made_while_the_provider_was_stopped_is_a_new_version() {
	let (world, stable) = kept_world_with("a.txt", b"one").await;
	let stopped = world.killed();
	save_over_unseen(&stopped.local, "a.txt", b"two");
	let world = stopped.started();
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(world.drive.child(&root_dir(), "a.old.txt"), None);
	assert_eq!(conflicts(&world), []);
	assert_eq!(
		world.drive.writes(),
		[DriveOp::UploadVersion(format!("stable/{stable}"))]
	);
	let remote = world.remote_file(&root_dir(), "a.txt");
	assert_eq!(world.identity("a.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("a.txt"));
}

#[tokio::test(start_paused = true)]
async fn a_save_by_delete_and_create_while_the_provider_was_stopped_is_a_new_version() {
	let (world, stable) = kept_world_with("a.txt", b"one").await;
	let stopped = world.killed();
	delete_unseen(&stopped.local, "a.txt");
	stopped.local.user_create(&root_path("a.txt"), b"two");
	let world = stopped.started();
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(world.trashes(), 0);
	assert_eq!(conflicts(&world), []);
}

#[tokio::test(start_paused = true)]
async fn a_save_made_while_stopped_over_a_version_the_drive_moved_past_keeps_both() {
	let (world, stable) = kept_world_with("a.txt", b"one").await;
	let stopped = world.killed();
	stopped.drive.bump_head(stable, b"theirs");
	save_over_unseen(&stopped.local, "a.txt", b"mine");
	let world = stopped.started();
	world.settle().await;
	assert_eq!(
		world.drive.content(stable),
		b"theirs",
		"the newer version stays"
	);
	let kept = world.remote_file(&root_dir(), "a.old.txt");
	assert_eq!(
		world.drive.content(jobs::file_ids(&kept).unwrap().0),
		b"mine"
	);
	assert_eq!(conflicts(&world), [ConflictKind::EditVsEdit]);
	assert_eq!(
		world.identity("a.txt"),
		Some(file_identity(&world.remote_file(&root_dir(), "a.txt")))
	);
}

#[tokio::test(start_paused = true)]
async fn a_save_the_provider_was_killed_in_the_middle_of_is_a_new_version() {
	let (world, stable) = kept_world_with("doc.txt", b"one").await;
	world.local.user_create(&root_path("tmp1"), b"two");
	world.rename("doc.txt", "doc.bak");
	world.run(Duration::from_millis(100)).await;
	world
		.local
		.user_rename(&root_path("tmp1"), &root_path("doc.txt"))
		.unwrap();
	world.delete("doc.bak");
	world.run(Duration::from_millis(500)).await;
	assert_eq!(world.uploads(), 0, "killed before the save's upload");
	let world = world.killed().started();
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "doc.txt");
	assert_eq!(remote.stable_uuid, stable.to_string());
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(world.drive.child(&root_dir(), "doc.bak"), None);
	assert_eq!(world.local.entry(&root_path("doc.bak")), None);
	assert_eq!(world.trashes(), 0, "{:?}", world.drive.writes());
	assert_eq!(conflicts(&world), []);
}

#[tokio::test(start_paused = true)]
async fn a_save_made_while_stopped_is_not_claimed_when_a_directory_cannot_be_listed() {
	let (world, stable) = kept_world_with("a.txt", b"one").await;
	let stopped = world.killed();
	// While stopped, the placeholder moved into a directory the next start cannot list, and a new
	// file took its name.
	stopped.local.user_mkdir(&root_path("d"));
	let (_, responder) = stopped
		.local
		.user_rename(&root_path("a.txt"), &root_path("d\\a.txt"))
		.unwrap()
		.asked();
	responder.allow();
	stopped.local.user_create(&root_path("a.txt"), b"new");
	stopped
		.local
		.fail_list_dir(&root_path("d"), Some(LocalError::Busy));
	let world = stopped.started();
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"one", "never a version of it");
}

#[tokio::test(start_paused = true)]
async fn a_save_made_while_stopped_is_taken_back_though_a_walk_ran_before_the_records_were_read() {
	let (world, stable) = kept_world_with("a.txt", b"one").await;
	let stopped = world.killed();
	save_over_unseen(&stopped.local, "a.txt", b"two");
	stopped.local.take_watch_events();
	let engine = engine(
		&stopped.drive,
		&stopped.local,
		&stopped.reports,
		LocalConfig::default(),
		stopped.records.as_deref(),
	);
	// A walk asked for before the start read its records.
	engine.on_local_event(LocalEvent::Rescan);
	engine.start();
	let world = World {
		drive: stopped.drive,
		local: stopped.local,
		engine,
		reports: stopped.reports,
		records: stopped.records,
	};
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"two");
	assert_eq!(conflicts(&world), []);
}

#[tokio::test(start_paused = true)]
async fn a_saved_file_deleted_before_its_upload_gives_the_path_back_to_the_drive() {
	let (world, stable) = kept_world_with("a.txt", b"one").await;
	let stopped = world.killed();
	save_over_unseen(&stopped.local, "a.txt", b"two");
	stopped.drive.set_offline(true);
	let world = stopped.started();
	world.run(Duration::from_secs(60)).await;
	world.local.vanish(&root_path("a.txt"));
	world
		.engine
		.on_local_event(LocalEvent::Removed(root_path("a.txt")));
	world.run(Duration::from_secs(60)).await;
	world.drive.set_offline(false);
	world.run(WATCH_SAFETY_NET * 2).await;
	assert_eq!(world.drive.content(stable), b"one");
	assert_eq!(
		world.identity("a.txt"),
		Some(file_identity(&world.remote_file(&root_dir(), "a.txt"))),
		"the remote file is placed again"
	);
}

#[tokio::test(start_paused = true)]
async fn a_new_file_at_a_path_whose_placeholder_was_renamed_while_stopped_is_a_new_file() {
	let (world, stable) = kept_world_with("a.txt", b"one").await;
	let stopped = world.killed();
	let (_, responder) = stopped
		.local
		.user_rename(&root_path("a.txt"), &root_path("b.txt"))
		.unwrap()
		.asked();
	responder.allow();
	stopped.local.user_create(&root_path("a.txt"), b"new");
	let world = stopped.started();
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"one", "never a version of it");
}

#[tokio::test(start_paused = true)]
async fn a_save_made_while_stopped_after_another_device_moved_the_file_keeps_both() {
	let (world, stable) = kept_world_with("a.txt", b"one").await;
	let stopped = world.killed();
	stopped.drive.remote_rename(stable, "moved.txt");
	save_over_unseen(&stopped.local, "a.txt", b"two");
	let world = stopped.started();
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"one", "never a version of it");
	let new = world.remote_file(&root_dir(), "a.txt");
	assert_ne!(new.stable_uuid, stable.to_string());
	assert_eq!(world.drive.content(jobs::file_ids(&new).unwrap().0), b"two");
	assert!(world.local.entry(&root_path("moved.txt")).is_some());
}

#[tokio::test(start_paused = true)]
async fn a_file_deleted_while_the_provider_was_stopped_comes_back_from_the_drive() {
	let (world, stable) = kept_world_with("a.txt", b"one").await;
	let stopped = world.killed();
	delete_unseen(&stopped.local, "a.txt");
	let world = stopped.started();
	world.settle().await;
	assert_eq!(
		world.trashes(),
		0,
		"a delete the provider never saw is never trashed"
	);
	assert_eq!(world.drive.content(stable), b"one");
	assert_eq!(
		world.identity("a.txt"),
		Some(file_identity(&world.remote_file(&root_dir(), "a.txt")))
	);
}

#[tokio::test(start_paused = true)]
async fn a_rename_vetoed_when_the_provider_was_killed_is_put_back_on_the_next_start() {
	let world = unstarted_keeping_records();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.engine.start();
	world.settle().await;
	let renames = world.drive.gate_calls(DriveCall::Rename);
	// The first rename gives it its transit name.
	renames.add_permits(1);
	world.rename("a.txt", "d\\b.txt");
	world.run(Duration::from_secs(5)).await;
	// The drive moved it, and the rename waits for the drive: the app kills the helper.
	let stopped = world.killed();
	renames.add_permits(Semaphore::MAX_PERMITS / 2);
	assert!(
		stopped
			.drive
			.child(
				&format!("stable/{}", dir.uuid),
				&crate::names::transit_name(stable)
			)
			.is_some()
	);
	let world = stopped.started();
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "a.txt");
	assert_eq!(remote.stable_uuid, stable.to_string(), "back where it was");
	assert!(world.local.entry(&root_path("a.txt")).is_some());
	assert_eq!(world.local.entry(&root_path("d\\a.txt")), None);
	assert_eq!(world.local.entry(&root_path("d\\b.txt")), None);
}

#[tokio::test(start_paused = true)]
async fn a_rename_whose_old_name_was_taken_while_stopped_stays_and_is_settled() {
	let world = unstarted_keeping_records();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.engine.start();
	world.settle().await;
	let renames = world.drive.gate_calls(DriveCall::Rename);
	world.rename("a.txt", "d\\b.txt");
	world.run(Duration::from_secs(5)).await;
	// Its record saved and the drive held up, the app kills the helper.
	let stopped = world.killed();
	renames.add_permits(Semaphore::MAX_PERMITS / 2);
	// The drive did the rename the user was refused, and another device takes the old name.
	let dir_id = format!("stable/{uuid}");
	stopped.drive.remote_move(stable, &dir_id);
	stopped.drive.remote_rename(stable, "b.txt");
	let other = stopped
		.drive
		.add_remote_file(&root_dir(), "a.txt", b"other");
	let other = jobs::file_ids(&other).unwrap().0;
	let world = stopped.started();
	world.settle().await;
	world.settle().await;
	assert_eq!(world.drive.content(other), b"other", "never superseded");
	assert_eq!(world.drive.history(other), Vec::<Vec<u8>>::new());
	assert_eq!(
		world.remote_file(&dir_id, "b.txt").stable_uuid,
		stable.to_string(),
		"kept where the rename put it, under a name of its own"
	);
	assert!(world.local.entry(&root_path("d\\b.txt")).is_some());
	assert_eq!(
		world.identity("a.txt"),
		Some(file_identity(&world.remote_file(&root_dir(), "a.txt")))
	);
	// Settled: nothing is tried again.
	let writes = world.drive.writes().len();
	world.run(Duration::from_secs(30 * 60)).await;
	assert_eq!(world.drive.writes().len(), writes);
}

#[tokio::test(start_paused = true)]
async fn renames_in_a_row_the_provider_was_killed_after_all_stay() {
	let world = unstarted_keeping_records();
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.engine.start();
	world.settle().await;
	world.rename("a.txt", "b.txt");
	world.run(Duration::from_millis(300)).await;
	world.rename("b.txt", "c.txt");
	world.run(Duration::from_millis(300)).await;
	assert_eq!(world.rename_answers(), [Ok(()), Ok(())]);
	let world = world.killed().started();
	world.settle().await;
	world.settle().await;
	assert_eq!(
		world.remote_file(&root_dir(), "c.txt").stable_uuid,
		stable.to_string()
	);
	assert!(world.local.entry(&root_path("c.txt")).is_some());
	assert_eq!(world.local.entry(&root_path("a.txt")), None);
}

#[tokio::test(start_paused = true)]
async fn renames_are_vetoed_until_the_root_is_read_for_the_records() {
	let world = unstarted_keeping_records();
	world.placed_file("a.txt", b"one");
	world
		.local
		.fail_inspect(&RootPath::root(), Some(LocalError::Busy));
	world.engine.start();
	world.run(Duration::from_secs(5)).await;
	world.rename("a.txt", "b.txt");
	world.run(Duration::from_secs(5)).await;
	assert_eq!(
		world.rename_answers(),
		[Err(Failure::Transient)],
		"no record of it could be saved"
	);
	assert_eq!(world.drive.writes(), []);
	world.local.fail_inspect(&RootPath::root(), None);
	world.run(Duration::from_secs(2 * 60)).await;
	world.rename("a.txt", "b.txt");
	world.run(Duration::from_secs(5)).await;
	assert_eq!(world.rename_answers(), [Err(Failure::Transient), Ok(())]);
	assert!(world.drive.child(&root_dir(), "b.txt").is_some());
}

#[tokio::test(start_paused = true)]
async fn a_stop_puts_back_within_its_budget_and_the_next_start_does_the_rest() {
	let world = unstarted_keeping_records();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.engine.start();
	world.settle().await;
	let renames = world.drive.gate_calls(DriveCall::Rename);
	// The first rename gives it its transit name; the drive holds up every one after it.
	renames.add_permits(1);
	world.rename("a.txt", "d\\b.txt");
	world.run(Duration::from_secs(5)).await;
	let stopping = Instant::now();
	world.engine.shutdown().await;
	let took = stopping.elapsed();
	assert!(
		(STOP_PUT_BACK_BUDGET..STOP_PUT_BACK_BUDGET + Duration::from_secs(1)).contains(&took),
		"{took:?}"
	);
	renames.add_permits(Semaphore::MAX_PERMITS / 2);
	let world = world.started_again();
	world.settle().await;
	world.settle().await;
	assert_eq!(
		world.remote_file(&root_dir(), "a.txt").stable_uuid,
		stable.to_string(),
		"put back by the next start"
	);
	assert!(world.local.entry(&root_path("a.txt")).is_some());
}

#[tokio::test(start_paused = true)]
async fn a_rename_that_happened_before_the_provider_was_killed_stays() {
	let world = unstarted_keeping_records();
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.engine.start();
	world.settle().await;
	world.rename("a.txt", "b.txt");
	world.run(Duration::from_secs(1)).await;
	assert_eq!(world.rename_answers(), [Ok(())]);
	let world = world.killed().started();
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "b.txt");
	assert_eq!(remote.stable_uuid, stable.to_string());
	assert!(world.local.entry(&root_path("b.txt")).is_some());
	assert_eq!(world.local.entry(&root_path("a.txt")), None);
}

/// A save's rename of its temporary file, uploaded as a file of its own first, over the file it
/// saves, which the provider was killed before it finished: the next start finishes it from its
/// record. The file keeps its identity with one more version, and the temporary upload goes to
/// the trash.
#[tokio::test(start_paused = true)]
async fn a_save_over_an_uploaded_temporary_file_cut_short_by_a_kill_is_finished_on_the_next_start()
{
	let world = unstarted_keeping_records();
	let (stable, _) = world.placed_file("save.txt", b"one");
	world.engine.start();
	world.settle().await;
	world.local.user_create(&root_path("save.txt.tmp"), b"two");
	world.settle().await;
	let Some(PlaceholderIdentity::File { stable: temp, .. }) = world.identity("save.txt.tmp")
	else {
		panic!("the temporary file was not uploaded");
	};
	world
		.local
		.fail_revert(&root_path("save.txt"), Some(LocalError::Busy));
	let (request, responder) = world
		.local
		.user_rename_replacing(&root_path("save.txt.tmp"), &root_path("save.txt"))
		.unwrap()
		.asked();
	world.engine.on_notify_rename(request, responder);
	world.run(Duration::from_secs(15)).await;
	assert_eq!(world.rename_answers(), [Ok(())]);
	assert_eq!(world.trashes(), 0);
	assert_eq!(world.uploads(), 1, "the temporary file's own upload only");
	world.local.fail_revert(&root_path("save.txt"), None);
	let world = world.killed().started();
	world.settle().await;
	world.settle().await;
	let remote = world.remote_file(&root_dir(), "save.txt");
	assert_eq!(remote.stable_uuid, stable.to_string());
	assert_eq!(world.drive.history(stable), [b"two".to_vec()]);
	assert_eq!(world.identity("save.txt"), Some(file_identity(&remote)));
	assert!(world.in_sync("save.txt"));
	assert_eq!(world.drive.names(&root_dir()), ["save.txt"]);
	let trashed: Vec<String> = world
		.drive
		.trashed()
		.iter()
		.map(|item| jobs::object_uuid(item).to_owned())
		.collect();
	assert_eq!(trashed, [temp.to_string()]);
}

/// A rename a previous run left unsettled is restored at the next start, and the first walk leaves
/// its target alone. Once it settles, its target is looked at, not only by the safety-net walk.
#[tokio::test(start_paused = true)]
async fn a_rename_a_previous_run_left_unsettled_is_looked_at_once_settled() {
	let world = unstarted_keeping_records();
	world.placed_file("a.txt", b"one");
	world.placed_file("b.txt", b"old");
	world.engine.start();
	world.settle().await;
	// The file the rename replaces is trashed once it happened: while that fails, its record stays
	// unsettled.
	world
		.drive
		.fail_always(DriveCall::Trash, || CacheError::remote("unreachable"));
	let (request, responder) = world
		.local
		.user_rename_replacing(&root_path("a.txt"), &root_path("b.txt"))
		.unwrap()
		.asked();
	world.engine.on_notify_rename(request, responder);
	world.run(Duration::from_secs(1)).await;
	assert_eq!(world.rename_answers(), [Ok(())]);
	assert!(!world.in_sync("b.txt"), "a rename clears the in-sync state");
	let world = world.killed().started();
	world.settle().await;
	assert!(
		!world.in_sync("b.txt"),
		"not read before the rename settled"
	);
	assert!(
		lock(&world.engine.inner.state)
			.rename_records
			.iter()
			.any(|record| !record.confirmed),
		"restored, not settled"
	);
	world.drive.stop_failing(DriveCall::Trash);
	// Within the put-back's backoff, well before the safety-net walk.
	world.run(Duration::from_secs(90)).await;
	assert!(
		!lock(&world.engine.inner.state)
			.rename_records
			.iter()
			.any(|record| !record.confirmed),
		"settled"
	);
	assert!(
		world.in_sync("b.txt"),
		"looked at once the restored rename settled"
	);
}

/// A rename vetoed after the drive was changed for it is put back on the drive; while that keeps
/// failing, its target is no name the platform renames onto any more, and a file the user puts
/// there is read and uploaded as any other.
#[tokio::test(start_paused = true)]
async fn a_file_at_the_target_of_a_vetoed_rename_is_uploaded_while_the_put_back_fails() {
	let world = unstarted_keeping_records();
	world.placed_file("a.txt", b"one");
	world.engine.start();
	world.settle().await;
	let renames = world.drive.gate_calls(DriveCall::Rename);
	world.rename("a.txt", "b.txt");
	// Past the answer's budget: vetoed while the drive is still being changed.
	world.run(Duration::from_secs(50)).await;
	assert_eq!(world.rename_answers().len(), 1);
	assert!(world.rename_answers()[0].is_err());
	world
		.drive
		.fail_always(DriveCall::Rename, || CacheError::remote("unreachable"));
	renames.add_permits(Semaphore::MAX_PERMITS / 2);
	world.run(Duration::from_secs(5)).await;
	assert!(
		lock(&world.engine.inner.state)
			.rename_records
			.iter()
			.any(|record| !record.confirmed),
		"the put-back fails"
	);
	world.local.user_create(&root_path("b.txt"), b"mine");
	world.run(Duration::from_secs(120)).await;
	let uploaded = world.drive.names(&root_dir()).into_iter().any(|name| {
		matches!(world.drive.child(&root_dir(), &name), Some(FfiNonRootObject::File(file))
			if world.drive.content(Uuid::parse_str(&file.stable_uuid).unwrap()) == b"mine")
	});
	assert!(uploaded, "{:?}", world.drive.names(&root_dir()));
}

/// A walk while a directory's rename is not settled yet still indexes what the directory holds:
/// only a file rename's target may be a name reserved for it, which nothing may read.
#[tokio::test(start_paused = true)]
async fn a_walk_indexes_the_items_in_a_directory_renamed_and_not_settled_yet() {
	let world = unstarted_keeping_records();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let inside = world
		.drive
		.add_remote_file(&format!("stable/{uuid}"), "x.txt", b"x");
	let (stable, version) = jobs::file_ids(&inside).unwrap();
	world.local.place_file(
		&root_path(r"d\x.txt"),
		PlaceholderIdentity::File { stable, version },
		b"x",
	);
	world.engine.start();
	world.settle().await;
	world.rename("d", "e");
	world.run(Duration::from_secs(1)).await;
	assert_eq!(world.rename_answers(), [Ok(())]);
	Inner::request_walk(&world.engine.inner);
	world.run(Duration::from_secs(1)).await;
	let state = lock(&world.engine.inner.state);
	assert!(
		state.rename_records.iter().any(|record| !record.confirmed),
		"the walk ran before the rename settled"
	);
	assert_eq!(state.index.path_of(stable), Some(&root_path(r"e\x.txt")));
	assert!(
		state.index.is_populated(uuid),
		"its populated state is kept"
	);
}

#[tokio::test(start_paused = true)]
async fn a_rename_put_back_while_offline_leaves_the_placeholder_until_it_is() {
	let world = unstarted_keeping_records();
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	let (stable, _) = world.placed_file("a.txt", b"one");
	world.engine.start();
	world.settle().await;
	let renames = world.drive.gate_calls(DriveCall::Rename);
	world.rename("a.txt", "d\\b.txt");
	world.run(Duration::from_secs(5)).await;
	let stopped = world.killed();
	renames.add_permits(Semaphore::MAX_PERMITS / 2);
	stopped.drive.set_offline(true);
	let world = stopped.started();
	world.run(Duration::from_secs(120)).await;
	assert!(
		world.local.entry(&root_path("a.txt")).is_some(),
		"not moved to follow the drive"
	);
	assert_eq!(world.local.entry(&root_path("d\\a.txt")), None);
	world.drive.set_offline(false);
	world.run(Duration::from_secs(10 * 60)).await;
	let remote = world.remote_file(&root_dir(), "a.txt");
	assert_eq!(remote.stable_uuid, stable.to_string(), "back where it was");
	assert!(world.local.entry(&root_path("a.txt")).is_some());
}

#[tokio::test(start_paused = true)]
async fn a_new_file_at_a_path_whose_placeholder_was_moved_into_a_new_folder_while_stopped_is_a_new_file()
 {
	let (world, stable) = kept_world_with("a.txt", b"one").await;
	let stopped = world.killed();
	stopped.local.user_mkdir(&root_path("d"));
	let (_, responder) = stopped
		.local
		.user_rename(&root_path("a.txt"), &root_path("d\\a.txt"))
		.unwrap()
		.asked();
	responder.allow();
	stopped.local.user_create(&root_path("a.txt"), b"new");
	let world = stopped.started();
	world.settle().await;
	assert_eq!(world.drive.content(stable), b"one", "never a version of it");
}

// ---- what the drive does not have yet ----

impl World {
	fn unsynced(&self) -> Option<u64> {
		self.engine.unsynced_count()
	}
}

#[tokio::test(start_paused = true)]
async fn nothing_is_counted_before_the_first_walk_has_seen_the_root() {
	let world = unstarted();
	world
		.local
		.user_create(&root_path("made offline.txt"), b"x");
	world.local.take_watch_events();
	world.drive.set_offline(true);
	world.engine.start();
	assert_eq!(world.unsynced(), None, "not known before the walk");
	world.run(Duration::from_secs(5)).await;
	assert_eq!(world.unsynced(), Some(1), "the walk found it");
}

#[tokio::test(start_paused = true)]
async fn a_new_file_counts_from_its_first_event_until_it_is_on_the_drive() {
	let world = world().await;
	assert_eq!(world.unsynced(), Some(0));
	world.local.user_create(&root_path("a.txt"), b"one");
	world.pump();
	assert_eq!(world.unsynced(), Some(1), "in the debounce");
	world.settle().await;
	assert_eq!(world.uploads(), 1);
	assert_eq!(world.unsynced(), Some(0));
}

#[tokio::test(start_paused = true)]
async fn an_edit_still_being_written_counts_while_it_settles() {
	let world = world().await;
	world.placed_file("a.txt", b"one");
	for n in 0..10_u8 {
		world.local.user_write(&root_path("a.txt"), &[n]).unwrap();
		world.run(Duration::from_secs(1)).await;
		assert_eq!(world.unsynced(), Some(1), "write {n}");
	}
	world.settle().await;
	assert_eq!(world.unsynced(), Some(0));
	assert_eq!(world.uploads(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_edit_held_open_by_a_writer_counts_until_it_is_uploaded() {
	let world = world().await;
	world.placed_file("a.txt", b"one");
	world.local.user_write(&root_path("a.txt"), b"two").unwrap();
	let writer = world.local.user_open_for_write(&root_path("a.txt"));
	world.run(Duration::from_secs(5 * 60)).await;
	assert_eq!(world.uploads(), 0);
	assert_eq!(world.unsynced(), Some(1));
	drop(writer);
	world.settle().await;
	assert_eq!(world.uploads(), 1);
	assert_eq!(world.unsynced(), Some(0));
}

#[tokio::test(start_paused = true)]
async fn an_edit_its_writer_holds_without_any_event_counts_once_a_walk_has_looked() {
	let world = world().await;
	world.placed_file("a.txt", b"one");
	let writer = world.local.user_open_for_write(&root_path("a.txt"));
	world.local.user_write(&root_path("a.txt"), b"two").unwrap();
	// Measured on Windows: a write through a handle still open raises no watcher event.
	world.local.take_watch_events();
	world.run(Duration::from_secs(5)).await;
	assert_eq!(world.unsynced(), Some(0), "nothing told the engine yet");
	let counted = world
		.engine
		.unsynced_count_after_walk(Duration::from_secs(60))
		.await;
	assert_eq!(
		counted,
		Some(1),
		"the walk found the placeholder not in sync"
	);
	assert_eq!(world.uploads(), 0);
	drop(writer);
	world.settle().await;
	assert_eq!(world.uploads(), 1);
	assert_eq!(
		world
			.engine
			.unsynced_count_after_walk(Duration::from_secs(60))
			.await,
		Some(0)
	);
}

#[tokio::test(start_paused = true)]
async fn nothing_is_counted_after_a_walk_that_could_not_list_a_directory() {
	let world = world().await;
	let dir = world.drive.add_remote_dir(&root_dir(), "d");
	let uuid = Uuid::parse_str(&dir.uuid).unwrap();
	world
		.local
		.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	placed_in(&world, &format!("stable/{uuid}"), "d", "a.txt", b"one");
	let writer = world.local.user_open_for_write(&root_path("d\\a.txt"));
	world
		.local
		.user_write(&root_path("d\\a.txt"), b"two")
		.unwrap();
	world.local.take_watch_events();
	world
		.local
		.fail_list_dir(&root_path("d"), Some(LocalError::Busy));
	assert_eq!(
		world
			.engine
			.unsynced_count_after_walk(Duration::from_secs(60))
			.await,
		None,
		"the edit in the directory it could not list may be there"
	);
	world.local.fail_list_dir(&root_path("d"), None);
	assert_eq!(
		world
			.engine
			.unsynced_count_after_walk(Duration::from_secs(60))
			.await,
		Some(1)
	);
	drop(writer);
}

#[tokio::test(start_paused = true)]
async fn an_edit_on_its_way_up_counts_once() {
	let world = world().await;
	let (stable, _) = world.placed_file("a.txt", b"one");
	let gate = world.drive.gate_uploads();
	world.local.user_write(&root_path("a.txt"), b"two").unwrap();
	world.settle().await;
	assert_eq!(
		world.drive.history(stable),
		Vec::<Vec<u8>>::new(),
		"held at the gate"
	);
	assert_eq!(world.unsynced(), Some(1));
	gate.add_permits(1);
	world.settle().await;
	assert_eq!(world.drive.history(stable), [b"two".to_vec()]);
	assert_eq!(world.unsynced(), Some(0));
}

#[tokio::test(start_paused = true)]
async fn a_new_file_the_drive_cannot_take_counts_while_it_backs_off_and_once_parked() {
	let world = world().await;
	world.drive.set_offline(true);
	world.local.user_create(&root_path("a.txt"), b"one");
	world.run(Duration::from_secs(20)).await;
	assert_eq!(world.unsynced(), Some(1), "backing off");
	world.run(Duration::from_secs(30 * 60)).await;
	assert!(world.reports().iter().any(|report| matches!(
		report,
		LocalReport::Parked { path, .. } if *path == root_path("a.txt")
	)));
	assert_eq!(world.unsynced(), Some(1), "parked");
	world.drive.set_offline(false);
	world
		.engine
		.on_local_event(LocalEvent::Modified(root_path("a.txt")));
	world.settle().await;
	assert_eq!(world.uploads(), 1);
	assert_eq!(world.unsynced(), Some(0));
}

#[tokio::test(start_paused = true)]
async fn a_new_directory_counts_with_each_item_named_in_it_until_they_are_on_the_drive() {
	let world = world().await;
	world.drive.set_offline(true);
	world.local.user_mkdir(&root_path("d"));
	world.local.user_create(&root_path("d\\a.txt"), b"one");
	world.local.user_create(&root_path("d\\b.txt"), b"two");
	world.settle().await;
	assert_eq!(
		world.unsynced(),
		Some(3),
		"the directory and the two files its events named"
	);
	world.drive.set_offline(false);
	world
		.engine
		.on_local_event(LocalEvent::Modified(root_path("d")));
	world.run(Duration::from_secs(10 * 60)).await;
	assert_eq!(world.uploads(), 2);
	assert_eq!(world.unsynced(), Some(0));
}

#[tokio::test(start_paused = true)]
async fn a_directory_filled_while_the_provider_was_stopped_counts_once_until_it_is_on_the_drive() {
	let world = unstarted();
	world.local.user_mkdir(&root_path("d"));
	world.local.user_create(&root_path("d\\a.txt"), b"one");
	world.local.user_create(&root_path("d\\b.txt"), b"two");
	world.local.take_watch_events();
	world.drive.set_offline(true);
	world.engine.start();
	world.settle().await;
	assert_eq!(
		world.unsynced(),
		Some(1),
		"the directory, whatever it holds"
	);
	world.drive.set_offline(false);
	world
		.engine
		.on_local_event(LocalEvent::Modified(root_path("d")));
	world.run(Duration::from_secs(10 * 60)).await;
	assert_eq!(world.uploads(), 2);
	assert_eq!(world.unsynced(), Some(0));
}

#[tokio::test(start_paused = true)]
async fn an_unsent_edit_not_kept_yet_counts_once_with_its_files_own_edit() {
	let world = unstarted();
	let (stable, _) = world.placed_file("f.txt", b"one");
	world.drive.mark_pending(stable, b"unsent");
	world.local.take_watch_events();
	world.drive.fail_always(DriveCall::CachedChildren, || {
		CacheError::io("the cache is unreadable")
	});
	world.engine.start();
	world.run(Duration::from_secs(5)).await;
	assert_eq!(world.unsynced(), Some(1), "the unsent edit");
	world
		.local
		.user_write(&root_path("f.txt"), b"newer")
		.unwrap();
	world.run(Duration::from_secs(10 * 60)).await;
	assert_eq!(world.drive.pending_uploads(), [stable]);
	assert_eq!(world.unsynced(), Some(1), "one file, counted once");
	world.drive.stop_failing(DriveCall::CachedChildren);
	world.run(WATCH_SAFETY_NET * 2).await;
	assert!(world.drive.pending_uploads().is_empty());
	assert_eq!(world.unsynced(), Some(0));
}

impl World {
	/// The count as the helper's `getState` takes it: after a walk, within a minute.
	async fn unsynced_after_walk(&self) -> Option<u64> {
		self.engine
			.unsynced_count_after_walk(Duration::from_secs(60))
			.await
	}

	/// Lets the paused clock run, handing over no watcher event, until the provider converted the
	/// ordinary file `path` into its placeholder; within ten minutes.
	async fn until_converted(&self, path: &str) {
		let deadline = Instant::now() + Duration::from_secs(10 * 60);
		while !self
			.local
			.ops()
			.iter()
			.any(|op| matches!(op, LocalOp::Converted { path: at, .. } if *at == root_path(path)))
		{
			assert!(Instant::now() < deadline, "{path} was never converted");
			sleep(Duration::from_millis(100)).await;
		}
	}
}

#[tokio::test(start_paused = true)]
async fn a_new_file_carried_once_the_drive_answers_again_counts_nothing_after_its_own_convert() {
	let world = world().await;
	world.drive.set_offline(true);
	world.local.user_create(&root_path("offnew.txt"), b"new");
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.unsynced_after_walk().await, Some(1), "offline");
	world.drive.set_offline(false);
	world.until_converted("offnew.txt").await;
	assert!(world.in_sync("offnew.txt"));
	// The convert's own event: the path is named again, and holds nothing for the drive.
	world.pump();
	assert_eq!(world.unsynced_after_walk().await, Some(0), "at once");
	for wait in [
		WATCH_DEBOUNCE,
		LocalConfig::default().settle,
		WATCH_SAFETY_NET,
	] {
		world.run(wait).await;
		assert_eq!(world.unsynced_after_walk().await, Some(0), "after {wait:?}");
		assert_eq!(world.unsynced(), Some(0), "after {wait:?}");
	}
	assert_eq!(world.uploads(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_item_the_default_ignore_patterns_match_never_counts() {
	let world = unstarted_with(LocalConfig {
		max_concurrent_jobs: NonZeroUsize::MIN,
		..LocalConfig::default()
	});
	world.engine.start();
	world.run(Duration::from_secs(5)).await;
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"one");
	world.run(Duration::from_secs(5)).await;
	// Only a walk names it, and its job waits for the one job slot the upload holds.
	world.local.user_create(&root_path("desktop.ini"), b"x");
	world.local.take_watch_events();
	let opener = {
		let gate = Arc::clone(&gate);
		tokio::spawn(async move {
			sleep(Duration::from_secs(10)).await;
			gate.add_permits(1);
		})
	};
	assert_eq!(world.unsynced_after_walk().await, Some(0));
	opener.await.unwrap();
	assert_eq!(world.uploads(), 1);
	assert_eq!(
		world
			.local
			.entry(&root_path("desktop.ini"))
			.unwrap()
			.identity,
		None
	);
}

#[tokio::test(start_paused = true)]
async fn a_change_no_job_could_look_at_yet_is_unknown_never_left_out() {
	let world = unstarted_with(LocalConfig {
		max_concurrent_jobs: NonZeroUsize::MIN,
		..LocalConfig::default()
	});
	world.engine.start();
	world.run(Duration::from_secs(5)).await;
	world.placed_file("b.txt", b"one");
	let gate = world.drive.gate_uploads();
	world.local.user_create(&root_path("a.txt"), b"one");
	world.run(Duration::from_secs(5)).await;
	let writer = world.local.user_open_for_write(&root_path("b.txt"));
	world.local.user_write(&root_path("b.txt"), b"two").unwrap();
	world.local.take_watch_events();
	assert_eq!(
		world
			.engine
			.unsynced_count_after_walk(Duration::from_secs(4))
			.await,
		None,
		"the walk found b.txt not in sync, and the upload holds the only job slot"
	);
	assert_eq!(world.unsynced(), Some(2), "named, and so counted");
	gate.add_permits(2);
	assert_eq!(
		world.unsynced_after_walk().await,
		Some(1),
		"held by its writer"
	);
	drop(writer);
	world.settle().await;
	assert_eq!(world.unsynced_after_walk().await, Some(0));
	assert_eq!(world.uploads(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_new_file_waiting_for_its_first_look_counts_after_a_walk() {
	let world = world().await;
	world.drive.set_offline(true);
	world.local.user_create(&root_path("a.txt"), b"one");
	world.pump();
	assert_eq!(world.unsynced(), Some(1), "in the debounce");
	assert_eq!(
		world.unsynced_after_walk().await,
		Some(1),
		"looked at, not carried"
	);
	world.drive.set_offline(false);
	world.settle().await;
	assert_eq!(world.unsynced_after_walk().await, Some(0));
}

#[tokio::test(start_paused = true)]
async fn a_count_asked_for_during_the_start_waits_for_the_take_over() {
	let world = unstarted();
	world
		.local
		.user_create(&root_path("made offline.txt"), b"x");
	world.local.take_watch_events();
	world.drive.set_offline(true);
	world.engine.start();
	assert_eq!(world.unsynced(), None);
	assert_eq!(world.unsynced_after_walk().await, Some(1));
}
