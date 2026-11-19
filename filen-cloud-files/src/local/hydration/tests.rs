use std::{
	num::NonZeroUsize,
	sync::{Arc, Mutex},
	time::Duration,
};

use filen_mobile_native_cache::{CacheError, ffi::FfiFile};
use filen_sdk_rs::consts::CHUNK_SIZE_U64;
use filen_sdk_rs::sync_engine::WATCH_SAFETY_NET;
use filen_types::fs::Uuid;
use tokio::{
	runtime::Handle,
	time::{Instant, sleep},
};

use super::super::{
	FileVersion, LocalConfig, LocalEngine, LocalReport, LocalSkip, jobs, lock, queue::PARK_AFTER,
	report::RemoteApplied,
};
use crate::{
	dispatch::CloudFilesCallback,
	failure::Failure,
	identity::PlaceholderIdentity,
	os::{LocalError, LocalFs, OsError, PinAttr},
	populate::{SkipReason, SkippedItem, spec_for},
	root_path::RootPath,
	test_support::{DriveCall, FakeDrive, FakeLocal, LocalOp, ProviderAct, pattern, root_path},
};

const ROOT_UUID: Uuid = Uuid::from_u128(0x100);
const ROOT: PlaceholderIdentity = PlaceholderIdentity::Root { root: ROOT_UUID };
/// A file of two cache chunks.
const SIZE: usize = 2 << 20;
/// The quiet window of the default configuration.
const QUIET: Duration = Duration::from_secs(5);

/// What the engine reported, and the remote children it reported skipped.
#[derive(Default)]
struct Reports(Mutex<Vec<LocalReport>>, Mutex<Vec<SkippedItem>>);

impl CloudFilesCallback for Reports {
	fn on_skipped_items(&self, _directory: PlaceholderIdentity, items: &[SkippedItem]) {
		lock(&self.1).extend_from_slice(items);
	}

	fn on_local_report(&self, report: &LocalReport) {
		lock(&self.0).push(report.clone());
	}
}

struct World {
	drive: Arc<FakeDrive>,
	local: FakeLocal,
	engine: LocalEngine<FakeDrive, FakeLocal>,
	reports: Arc<Reports>,
}

fn root_dir() -> String {
	ROOT_UUID.to_string()
}

fn engine(
	drive: &Arc<FakeDrive>,
	local: &FakeLocal,
	reports: &Arc<Reports>,
	config: LocalConfig,
) -> LocalEngine<FakeDrive, FakeLocal> {
	LocalEngine::new(
		Arc::clone(drive),
		Arc::new(local.clone()),
		Handle::current(),
		config,
		Arc::clone(reports) as Arc<dyn CloudFilesCallback>,
	)
}

/// A drive and a populated root holding nothing yet, the engine not started.
fn unstarted() -> World {
	unstarted_with(LocalConfig::default())
}

/// [`unstarted`], its engine running with `config`.
fn unstarted_with(config: LocalConfig) -> World {
	let drive = Arc::new(FakeDrive::new(ROOT_UUID));
	drive.list(&root_dir(), Vec::new());
	let local = FakeLocal::new(ROOT);
	local.set_populated(&RootPath::root(), true);
	drive.link_local(&local);
	let source = Arc::clone(&drive);
	local.hydrate_from(move |stable| source.content(stable));
	let reports = Arc::new(Reports::default());
	let engine = engine(&drive, &local, &reports, config);
	World {
		drive,
		local,
		engine,
		reports,
	}
}

impl World {
	async fn started(self) -> Self {
		self.engine.start();
		self.run(Duration::from_secs(1)).await;
		self
	}

	async fn restarted(self) -> Self {
		self.engine.shutdown().await;
		drop(self.engine);
		let engine = engine(
			&self.drive,
			&self.local,
			&self.reports,
			LocalConfig::default(),
		);
		engine.start();
		let world = Self { engine, ..self };
		world.run(Duration::from_secs(1)).await;
		world
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

	/// A remote file `name` in the root holding `remote`, with a clean placeholder holding `local`
	/// of which `on_disk` bytes are on disk.
	fn placed(
		&self,
		name: &str,
		remote: &[u8],
		local: &[u8],
		on_disk: usize,
	) -> PlaceholderIdentity {
		let file = self.drive.add_remote_file(&root_dir(), name, remote);
		let identity = identity_of(&file);
		self.local
			.place_file_with(&root_path(name), identity, local, false);
		self.local
			.set_on_disk(&root_path(name), u64::try_from(on_disk).unwrap());
		identity
	}

	/// A read of `identity` by another process, in flight until the guard drops.
	fn read(&self, identity: PlaceholderIdentity) -> impl Drop + use<> {
		let PlaceholderIdentity::File { stable, version } = identity else {
			panic!("a file placeholder");
		};
		self.engine
			.hints()
			.reading(FileVersion { stable, version }, false)
	}

	fn on_disk(&self, name: &str) -> u64 {
		self.local.entry(&root_path(name)).unwrap().on_disk
	}

	fn count(&self, op: &LocalOp) -> usize {
		self.local.ops().iter().filter(|done| *done == op).count()
	}

	fn hydrations(&self, name: &str) -> usize {
		self.count(&LocalOp::Hydrated(root_path(name)))
	}

	fn hashes(&self, name: &str) -> usize {
		self.count(&LocalOp::HashChecked(root_path(name)))
	}

	fn dehydrations(&self, name: &str) -> usize {
		self.count(&LocalOp::Dehydrated(root_path(name)))
	}

	fn parked(&self) -> Vec<LocalReport> {
		lock(&self.reports.0)
			.iter()
			.filter(|report| matches!(report, LocalReport::Parked { .. }))
			.cloned()
			.collect()
	}

	fn damaged(&self) -> Vec<LocalReport> {
		lock(&self.reports.0)
			.iter()
			.filter(|report| matches!(report, LocalReport::Damaged { .. }))
			.cloned()
			.collect()
	}
}

fn identity_of(file: &FfiFile) -> PlaceholderIdentity {
	let (stable, version) = jobs::file_ids(file).unwrap();
	PlaceholderIdentity::File { stable, version }
}

/// `bytes` with one byte flipped in its second chunk.
fn garbled(bytes: &[u8]) -> Vec<u8> {
	let mut garbled = bytes.to_vec();
	let at = usize::try_from(CHUNK_SIZE_U64).unwrap() + 17;
	garbled[at] ^= 0xFF;
	garbled
}

/// Two chunks, the last missing: what a whole read leaves when its reader closes too early.
fn missing_last_chunk() -> usize {
	SIZE - usize::try_from(CHUNK_SIZE_U64).unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_file_missing_its_last_chunk_is_completed_and_verified_after_the_quiet_window() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("big.bin", &bytes, &bytes, missing_last_chunk());
	let read = world.read(file);
	let world = world.started().await;
	world.run(Duration::from_secs(10)).await;
	assert_eq!(
		world.hydrations("big.bin"),
		0,
		"not while a read is in flight"
	);
	drop(read);
	world.run(QUIET - Duration::from_secs(2)).await;
	assert_eq!(
		world.hydrations("big.bin"),
		0,
		"not within the quiet window"
	);
	world.run(Duration::from_secs(5)).await;
	assert_eq!(world.hydrations("big.bin"), 1);
	assert_eq!(world.on_disk("big.bin"), u64::try_from(SIZE).unwrap());
	assert_eq!(world.hashes("big.bin"), 1);
	assert_eq!(world.damaged(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_new_read_restarts_the_quiet_window() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("big.bin", &bytes, &bytes, missing_last_chunk());
	let read = world.read(file);
	let world = world.started().await;
	drop(read);
	world.run(Duration::from_secs(3)).await;
	drop(world.read(file));
	world.run(Duration::from_secs(3)).await;
	assert_eq!(
		world.hydrations("big.bin"),
		0,
		"the second read restarted the window"
	);
	world.run(Duration::from_secs(4)).await;
	assert_eq!(world.hydrations("big.bin"), 1);
}

#[tokio::test(start_paused = true)]
async fn a_mostly_unread_file_is_not_completed() {
	let world = unstarted();
	let bytes = pattern(SIZE * 4);
	world.placed("movie.bin", &bytes, &bytes, 8192);
	let world = world.started().await;
	world.run(Duration::from_secs(30)).await;
	assert_eq!(world.hydrations("movie.bin"), 0);
	assert_eq!(world.on_disk("movie.bin"), 8192);
}

#[tokio::test(start_paused = true)]
async fn a_matching_file_is_recorded_and_not_hashed_again() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	drop(world.read(file));
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.hashes("a.bin"), 1);
	drop(world.read(file));
	world.run(Duration::from_secs(10)).await;
	assert_eq!(
		world.hashes("a.bin"),
		1,
		"a verified version is not hashed again"
	);
}

#[tokio::test(start_paused = true)]
async fn a_mismatching_file_is_dehydrated_and_reported_damaged() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &garbled(&bytes), SIZE);
	let world = world.started().await;
	drop(world.read(file));
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.on_disk("a.bin"), 0);
	assert_eq!(
		world.damaged(),
		vec![LocalReport::Damaged {
			path: root_path("a.bin"),
			identity: file,
		}]
	);
	let PlaceholderIdentity::File { stable, version } = file else {
		unreachable!()
	};
	assert!(
		world
			.engine
			.hints()
			.is_damaged(FileVersion { stable, version })
	);
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_damaged_version_is_not_completed_again() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &garbled(&bytes), SIZE);
	let world = world.started().await;
	drop(world.read(file));
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.dehydrations("a.bin"), 1);
	// A reader pulls most of it again (the dispatcher refuses that; here the bytes just appear).
	world.local.set_on_disk(
		&root_path("a.bin"),
		u64::try_from(missing_last_chunk()).unwrap(),
	);
	drop(world.read(file));
	world.run(Duration::from_secs(10)).await;
	assert_eq!(
		world.hydrations("a.bin"),
		0,
		"a damaged version is never completed"
	);
	assert_eq!(world.dehydrations("a.bin"), 2, "its bytes go again");
	assert_eq!(world.damaged().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn an_edit_during_the_hash_is_not_corruption() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &bytes, SIZE);
	world
		.local
		.edit_during_hash(&root_path("a.bin"), b"the user's edit");
	let world = world.started().await;
	drop(world.read(file));
	world.run(Duration::from_secs(30)).await;
	assert_eq!(world.damaged(), Vec::new());
	assert_eq!(world.dehydrations("a.bin"), 0);
	assert_eq!(
		world.local.entry(&root_path("a.bin")).unwrap().bytes,
		b"the user's edit"
	);
	assert!(
		lock(&world.reports.0)
			.iter()
			.any(|report| matches!(report, LocalReport::Uploaded { .. })),
		"the edit is uploaded"
	);
}

#[tokio::test(start_paused = true)]
async fn a_file_edited_before_the_job_is_left_to_the_local_side() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	drop(world.read(file));
	world
		.local
		.user_write(&root_path("a.bin"), b"edited")
		.unwrap();
	world.run(Duration::from_secs(30)).await;
	assert_eq!(world.hashes("a.bin"), 0);
	assert_eq!(world.damaged(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_busy_damaged_dehydrate_is_retried_by_the_queue_and_parked_after_its_bound() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &garbled(&bytes), SIZE);
	world.local.busy_dehydrations(3);
	let world = world.started().await;
	drop(world.read(file));
	world.run(Duration::from_secs(40)).await;
	assert_eq!(world.on_disk("a.bin"), 0, "freed once the holder let go");
	assert_eq!(world.damaged().len(), 1);
	assert_eq!(world.parked(), Vec::new());

	let world = unstarted();
	let file = world.placed("b.bin", &bytes, &garbled(&bytes), SIZE);
	world.local.busy_dehydrations(u32::MAX);
	let world = world.started().await;
	drop(world.read(file));
	world.run(Duration::from_secs(15 * 60)).await;
	assert_eq!(world.damaged(), Vec::new());
	assert_eq!(
		world.parked(),
		vec![LocalReport::Parked {
			path: root_path("b.bin"),
			failure: Failure::ContentDamaged,
			attempts: PARK_AFTER,
		}]
	);
	assert_eq!(world.on_disk("b.bin"), u64::try_from(SIZE).unwrap());
	let PlaceholderIdentity::File { stable, version } = file else {
		unreachable!()
	};
	assert!(
		world
			.engine
			.hints()
			.is_damaged(FileVersion { stable, version }),
		"its reads stay refused"
	);
	// Parked, the user may free it: the platform asks, and the bytes go once its holder let go.
	world.local.busy_dehydrations(0);
	assert_eq!(
		world.user_dehydrate("b.bin").await,
		[answered("b.bin", true)]
	);
	assert_eq!(world.on_disk("b.bin"), 0);
}

#[tokio::test(start_paused = true)]
async fn a_completion_left_partial_backs_off_and_stops() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("big.bin", &bytes, &bytes, missing_last_chunk());
	world.local.stall_hydration();
	let world = world.started().await;
	world.run(Duration::from_secs(120)).await;
	assert_eq!(world.hydrations("big.bin"), 2, "two tries, then it waits");
	drop(world.read(file));
	world.run(Duration::from_secs(30)).await;
	assert_eq!(
		world.hydrations("big.bin"),
		4,
		"a new read gives it two more tries"
	);
}

#[tokio::test(start_paused = true)]
async fn a_newer_head_hands_the_path_to_the_remote_job() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	let PlaceholderIdentity::File { stable, .. } = file else {
		unreachable!()
	};
	world.drive.quiet_signals(true);
	let version = world.drive.bump_head(stable, b"newer");
	drop(world.read(file));
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.hashes("a.bin"), 0);
	assert_eq!(
		world.local.entry(&root_path("a.bin")).unwrap().identity,
		Some(PlaceholderIdentity::File { stable, version })
	);
	assert!(
		lock(&world.reports.0)
			.iter()
			.any(|report| matches!(report, LocalReport::Remote(RemoteApplied::Updated { .. })))
	);
}

#[tokio::test(start_paused = true)]
async fn a_file_without_a_declared_hash_is_recorded_once() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &bytes, SIZE);
	let PlaceholderIdentity::File { stable, .. } = file else {
		unreachable!()
	};
	world.drive.declare(stable, None);
	let world = world.started().await;
	drop(world.read(file));
	world.run(Duration::from_secs(10)).await;
	drop(world.read(file));
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.hashes("a.bin"), 0);
	assert_eq!(world.damaged(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn the_walk_queues_partial_clean_files_after_a_restart() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("big.bin", &bytes, &bytes, missing_last_chunk());
	let read = world.read(file);
	let world = world.started().await;
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.hydrations("big.bin"), 0, "a read is in flight");
	let world = world.restarted().await;
	// The new engine knows nothing of the old one's reads.
	drop(read);
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.hydrations("big.bin"), 1);
	assert_eq!(world.on_disk("big.bin"), u64::try_from(SIZE).unwrap());
}

#[tokio::test(start_paused = true)]
async fn the_walk_does_not_rehash_full_files_after_a_restart() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await.restarted().await;
	world.run(Duration::from_secs(30)).await;
	assert_eq!(world.hashes("a.bin"), 0);
}

#[tokio::test(start_paused = true)]
async fn the_jobs_own_events_cause_no_drive_write() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let partial = world.placed("big.bin", &bytes, &bytes, missing_last_chunk());
	let damaged = world.placed("bad.bin", &bytes, &garbled(&bytes), SIZE);
	let world = world.started().await;
	drop(world.read(partial));
	drop(world.read(damaged));
	world.run(Duration::from_secs(60)).await;
	assert_eq!(world.hydrations("big.bin"), 1);
	assert_eq!(world.dehydrations("bad.bin"), 1);
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn an_own_process_read_counts_in_flight_but_raises_no_quiet_hint() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("big.bin", &bytes, &bytes, missing_last_chunk());
	let world = world.started().await;
	let PlaceholderIdentity::File { stable, version } = file else {
		unreachable!()
	};
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.hydrations("big.bin"), 1, "the walk completed it");
	// Put it back partly on disk with no event, as a race nothing saw would.
	world.local.set_on_disk(
		&root_path("big.bin"),
		u64::try_from(missing_last_chunk()).unwrap(),
	);
	let own = world
		.engine
		.hints()
		.reading(FileVersion { stable, version }, true);
	assert_eq!(world.engine.hints().reads_in_flight(stable), 1);
	drop(own);
	world.run(Duration::from_secs(QUIET.as_secs() * 2)).await;
	assert_eq!(world.hydrations("big.bin"), 1, "only the walk's completion");
	assert_eq!(
		world
			.engine
			.hints()
			.last_read(FileVersion { stable, version }),
		None
	);
}

#[tokio::test(start_paused = true)]
async fn a_version_the_dispatcher_found_damaged_is_dehydrated_at_once() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &bytes, missing_last_chunk());
	let read = world.read(file);
	let world = world.started().await;
	let PlaceholderIdentity::File { stable, version } = file else {
		unreachable!()
	};
	// A whole-file read failed its hash: the dispatcher records it while the read is in flight.
	world
		.engine
		.hints()
		.damaged(FileVersion { stable, version });
	world.run(Duration::from_secs(2)).await;
	assert_eq!(world.on_disk("a.bin"), 0, "not held back by the read");
	assert_eq!(world.hydrations("a.bin"), 0);
	assert_eq!(world.damaged().len(), 1);
	drop(read);
}

// ---- pins ----

fn dir_id(uuid: &str) -> String {
	format!("stable/{uuid}")
}

impl World {
	/// A remote directory `name` in the root with a populated placeholder, and its cache id.
	fn placed_dir(&self, name: &str) -> String {
		let dir = self.drive.add_remote_dir(&root_dir(), name);
		let uuid = Uuid::parse_str(&dir.uuid).unwrap();
		self.local
			.place_dir(&root_path(name), PlaceholderIdentity::Dir { uuid });
		dir_id(&dir.uuid)
	}

	/// A remote file `name` in the directory `dir` (a cache id) with a dehydrated placeholder at
	/// `path`.
	fn placed_in(&self, dir: &str, path: &str, bytes: &[u8]) -> PlaceholderIdentity {
		let name = root_path(path).name().unwrap().as_str().to_owned();
		let file = self.drive.add_remote_file(dir, &name, bytes);
		let identity = identity_of(&file);
		self.local
			.place_file_with(&root_path(path), identity, bytes, false);
		identity
	}
}

#[tokio::test(start_paused = true)]
async fn pinning_a_dehydrated_file_hydrates_and_verifies_it() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	world.placed("a.bin", &bytes, &bytes, 0);
	let world = world.started().await;
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.hydrations("a.bin"), 0, "nothing hydrates it unpinned");
	world.local.user_pin(&root_path("a.bin"), PinAttr::Pinned);
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.on_disk("a.bin"), u64::try_from(SIZE).unwrap());
	assert_eq!(world.hashes("a.bin"), 1);
	assert_eq!(world.damaged(), Vec::new());
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn pinning_a_directory_hydrates_its_tree() {
	let world = unstarted();
	let dir = world.placed_dir("d");
	world.placed_in(&dir, r"d\a.bin", &pattern(5000));
	world.placed_in(&dir, r"d\b.bin", &pattern(7000));
	let sub = world.drive.add_remote_dir(&dir, "s");
	world.local.place_dir(
		&root_path(r"d\s"),
		PlaceholderIdentity::Dir {
			uuid: Uuid::parse_str(&sub.uuid).unwrap(),
		},
	);
	world.placed_in(&dir_id(&sub.uuid), r"d\s\c.bin", &pattern(9000));
	world.placed("outside.bin", &pattern(100), &pattern(100), 0);
	let world = world.started().await;
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.run(Duration::from_secs(20)).await;
	assert_eq!(world.on_disk(r"d\a.bin"), 5000);
	assert_eq!(world.on_disk(r"d\b.bin"), 7000);
	assert_eq!(world.on_disk(r"d\s\c.bin"), 9000);
	assert_eq!(world.on_disk("outside.bin"), 0);
	assert_eq!(world.damaged(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn an_unpinned_child_of_a_pinned_directory_is_left() {
	let world = unstarted();
	let dir = world.placed_dir("d");
	world.placed_in(&dir, r"d\a.bin", &pattern(5000));
	world.placed_in(&dir, r"d\b.bin", &pattern(7000));
	world
		.local
		.user_pin(&root_path(r"d\b.bin"), PinAttr::Unpinned);
	let world = world.started().await;
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.run(Duration::from_secs(20)).await;
	assert_eq!(world.on_disk(r"d\a.bin"), 5000);
	assert_eq!(world.on_disk(r"d\b.bin"), 0);
}

#[tokio::test(start_paused = true)]
async fn a_new_remote_file_under_a_pinned_directory_arrives_hydrated() {
	let world = unstarted();
	let dir = world.placed_dir("d");
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	let world = world.started().await;
	world.drive.add_remote_file(&dir, "new.bin", &pattern(6000));
	world.run(Duration::from_secs(20)).await;
	assert_eq!(world.on_disk(r"d\new.bin"), 6000);
	assert_eq!(
		world.local.entry(&root_path(r"d\new.bin")).unwrap().bytes,
		pattern(6000)
	);
	assert_eq!(world.damaged(), Vec::new());
}

impl World {
	/// A remote directory `path` (its last component its name) in the directory `parent` (a cache
	/// id) whose placeholder the platform never populated, and its cache id.
	fn unopened_dir(&self, parent: &str, path: &str) -> String {
		let name = root_path(path).name().unwrap().as_str().to_owned();
		let dir = self.drive.add_remote_dir(parent, &name);
		let uuid = Uuid::parse_str(&dir.uuid).unwrap();
		self.local
			.place_dir_unpopulated(&root_path(path), PlaceholderIdentity::Dir { uuid });
		dir_id(&dir.uuid)
	}

	/// A remote directory `name` in the directory `parent` (a cache id) with no placeholder, and
	/// its cache id.
	fn remote_dir(&self, parent: &str, name: &str) -> String {
		dir_id(&self.drive.add_remote_dir(parent, name).uuid)
	}

	/// A tree nothing ever listed: `d`, whose placeholder was never populated, holding `a.bin` and
	/// `s`, which holds `b.bin` and `t`, which holds `c.bin`. Returns each file's path and size.
	fn unopened_tree(&self) -> [(&'static str, u64); 3] {
		let d = self.unopened_dir(&root_dir(), "d");
		self.drive.add_remote_file(&d, "a.bin", &pattern(5000));
		let s = self.remote_dir(&d, "s");
		self.drive.add_remote_file(&s, "b.bin", &pattern(6000));
		let t = self.remote_dir(&s, "t");
		self.drive.add_remote_file(&t, "c.bin", &pattern(7000));
		[
			(r"d\a.bin", 5000),
			(r"d\s\b.bin", 6000),
			(r"d\s\t\c.bin", 7000),
		]
	}

	/// What the directory `dir` holds, sorted.
	fn listed(&self, dir: &str) -> Vec<RootPath> {
		let mut listed: Vec<RootPath> = LocalFs::list_dir(&self.local, &root_path(dir))
			.unwrap()
			.into_iter()
			.map(Result::unwrap)
			.collect();
		listed.sort_by_key(RootPath::to_string);
		listed
	}

	/// How often the provider marked the directory `path` populated.
	fn populations(&self, path: &str) -> usize {
		self.count(&LocalOp::DirPopulated(root_path(path)))
	}

	/// How many placeholders the provider created under `dir`, at any depth.
	fn created_under(&self, dir: &str) -> usize {
		let dir = root_path(dir);
		self.local
			.ops()
			.iter()
			.filter(|op| {
				matches!(op, LocalOp::PlaceholderCreated { path, .. } if path.starts_with(&dir) && *path != dir)
			})
			.count()
	}
}

#[tokio::test(start_paused = true)]
async fn pinning_a_never_opened_tree_three_levels_deep_hydrates_all_of_it() {
	let world = unstarted();
	let files = world.unopened_tree();
	let world = world.started().await;
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.run(Duration::from_secs(30)).await;
	for (path, size) in files {
		let entry = world.local.entry(&root_path(path)).expect(path);
		assert_eq!(entry.on_disk, size, "{path} hydrated");
		assert_eq!(entry.bytes, pattern(usize::try_from(size).unwrap()));
	}
	assert_eq!(
		[
			world.populations("d"),
			world.populations(r"d\s"),
			world.populations(r"d\s\t")
		],
		[1, 1, 1]
	);
	assert_eq!(world.damaged(), Vec::new());
	assert_eq!(world.drive.writes(), Vec::new());
	// Each directory is in the populated set the cache keeps fresh.
	world.run(Duration::from_secs(5)).await;
	assert_eq!(world.drive.reported_dirs().0.len(), 4);
}

#[tokio::test(start_paused = true)]
async fn a_never_opened_folder_the_cache_has_not_listed_is_listed_from_the_server() {
	let world = unstarted();
	let d = world.unopened_dir(&root_dir(), "d");
	world.drive.add_remote_file(&d, "a.bin", &pattern(5000));
	world.drive.unlisted(&d);
	let world = world.started().await;
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.run(Duration::from_secs(20)).await;
	assert_eq!(world.on_disk(r"d\a.bin"), 5000);
	assert_eq!(world.populations("d"), 1);
}

#[tokio::test(start_paused = true)]
async fn unpinning_a_never_opened_tree_while_it_is_populated_stops_and_frees_it() {
	let world = unstarted();
	world.unopened_tree();
	let lists = world.drive.gate_calls(DriveCall::List);
	let world = world.started().await;
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	// `d`'s listing passes; `s`'s waits at the gate.
	lists.add_permits(1);
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.on_disk(r"d\a.bin"), 5000);
	assert_eq!(world.populations("d"), 1);
	world.local.user_pin(&root_path("d"), PinAttr::Unpinned);
	world.run(Duration::from_secs(10)).await;
	world.drive.stop_gating(DriveCall::List);
	lists.add_permits(16);
	world.run(Duration::from_secs(30)).await;
	assert_eq!(world.on_disk(r"d\a.bin"), 0, "freed as the unpin asks");
	assert_eq!(world.created_under(r"d\s"), 0, "`s` was not populated");
	assert_eq!(world.populations(r"d\s"), 0);
	assert!(lock(&world.reports.0).contains(&LocalReport::Freed {
		path: root_path(r"d\a.bin")
	}));
	assert_eq!(
		world.local.entry(&root_path("d")).unwrap().pin,
		PinAttr::Unspecified
	);
}

#[tokio::test(start_paused = true)]
async fn a_never_opened_folder_pinned_offline_is_populated_once_online_without_spinning() {
	let world = unstarted();
	let files = world.unopened_tree();
	let world = world.started().await;
	world.drive.set_offline(true);
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.run(Duration::from_secs(120)).await;
	assert_eq!(world.created_under("d"), 0);
	let parked: Vec<LocalReport> = world
		.parked()
		.into_iter()
		.filter(
			|report| matches!(report, LocalReport::Parked { path, .. } if *path == root_path("d")),
		)
		.collect();
	assert!(parked.len() <= 1, "reported at most once: {parked:?}");
	// A job asking again at once would read the directory thousands of times in two minutes.
	let looks = world.local.inspections(&root_path("d"));
	assert!(looks < 100, "read {looks} times");
	world.drive.set_offline(false);
	world.run(WATCH_SAFETY_NET * 2).await;
	for (path, size) in files {
		assert_eq!(world.on_disk(path), size, "{path} hydrated");
	}
}

#[tokio::test(start_paused = true)]
async fn a_user_listing_a_pinned_folder_as_the_provider_populates_it_places_each_item_once() {
	let world = unstarted();
	let files = world.unopened_tree();
	let lists = world.drive.gate_calls(DriveCall::List);
	let world = world.started().await;
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.run(Duration::from_secs(2)).await;
	// The provider's listing of `d` waits at the gate while a user lists `d`: the platform
	// populates it, and the dispatcher hints it.
	let d = world
		.local
		.entry(&root_path("d"))
		.unwrap()
		.identity
		.unwrap();
	let PlaceholderIdentity::Dir { uuid } = d else {
		panic!("a directory");
	};
	let cache_id = dir_id(&uuid.to_string());
	let specs: Vec<_> = world
		.drive
		.names(&cache_id)
		.iter()
		.map(|name| spec_for(&world.drive.child(&cache_id, name).unwrap()).unwrap())
		.collect();
	world.local.populate(&root_path("d"), &specs);
	world.engine.hints().populated(d);
	world.run(Duration::from_secs(2)).await;
	world.drive.stop_gating(DriveCall::List);
	lists.add_permits(16);
	world.run(Duration::from_secs(30)).await;
	assert_eq!(
		world.created_under("d") - world.created_under(r"d\s"),
		0,
		"the user's listing placed `d`'s items"
	);
	assert_eq!(world.populations("d"), 0);
	assert_eq!(
		world.populations(r"d\s"),
		1,
		"`s`, never listed, the provider populates"
	);
	assert_eq!(
		world.listed("d"),
		[root_path(r"d\a.bin"), root_path(r"d\s")]
	);
	for (path, size) in files {
		assert_eq!(world.on_disk(path), size, "{path} hydrated");
	}
}

#[tokio::test(start_paused = true)]
async fn a_remote_folder_added_under_a_pinned_folder_arrives_populated_and_hydrated() {
	let world = unstarted();
	let dir = world.placed_dir("d");
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	let world = world.started().await;
	let new = world.remote_dir(&dir, "new");
	world.drive.add_remote_file(&new, "x.bin", &pattern(6000));
	let deeper = world.remote_dir(&new, "deeper");
	world
		.drive
		.add_remote_file(&deeper, "y.bin", &pattern(7000));
	world.run(Duration::from_secs(30)).await;
	assert_eq!(world.on_disk(r"d\new\x.bin"), 6000);
	assert_eq!(world.on_disk(r"d\new\deeper\y.bin"), 7000);
	assert_eq!(
		[
			world.populations(r"d\new"),
			world.populations(r"d\new\deeper")
		],
		[1, 1]
	);
}

#[tokio::test(start_paused = true)]
async fn a_never_opened_folder_with_a_name_windows_cannot_hold_skips_and_reports_it() {
	let world = unstarted();
	let d = world.unopened_dir(&root_dir(), "d");
	world.drive.add_remote_file(&d, "ok.bin", &pattern(5000));
	let bad = world.drive.add_remote_file(&d, "a:b.bin", &pattern(10));
	let world = world.started().await;
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.run(Duration::from_secs(20)).await;
	assert_eq!(world.on_disk(r"d\ok.bin"), 5000);
	assert_eq!(world.listed("d"), [root_path(r"d\ok.bin")]);
	assert_eq!(world.populations("d"), 1, "populated with what it can hold");
	let skipped = lock(&world.reports.1).clone();
	assert_eq!(skipped.len(), 1);
	assert_eq!(skipped[0].id(), bad.stable_uuid);
	assert_eq!(skipped[0].name(), Some("a:b.bin"));
}

#[tokio::test(start_paused = true)]
async fn a_never_opened_folder_with_an_item_the_os_refuses_skips_and_reports_it() {
	let world = unstarted();
	let d = world.unopened_dir(&root_dir(), "d");
	world.drive.add_remote_file(&d, "ok.bin", &pattern(5000));
	let bad = world.drive.add_remote_file(&d, "refused.bin", &pattern(10));
	let refused = OsError::Rejected { code: -1 };
	world
		.local
		.fail_placements_named("refused.bin", LocalError::Os(refused));
	let world = world.started().await;
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.run(Duration::from_secs(20)).await;
	assert_eq!(world.on_disk(r"d\ok.bin"), 5000);
	assert_eq!(world.listed("d"), [root_path(r"d\ok.bin")]);
	assert_eq!(world.populations("d"), 1, "populated with what it can hold");
	assert_eq!(world.parked(), Vec::new());
	let skipped = lock(&world.reports.1).clone();
	assert_eq!(skipped.len(), 1, "{skipped:?}");
	assert_eq!(skipped[0].id(), bad.stable_uuid);
	assert_eq!(skipped[0].name(), Some("refused.bin"));
	assert_eq!(skipped[0].reason(), &SkipReason::Refused(refused));
}

#[tokio::test(start_paused = true)]
async fn the_walk_hydrates_pinned_files_after_a_restart() {
	let world = unstarted();
	let dir = world.placed_dir("d");
	world.placed_in(&dir, r"d\a.bin", &pattern(5000));
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.local.take_watch_events();
	let world = world.started().await;
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.on_disk(r"d\a.bin"), 5000);
}

#[tokio::test(start_paused = true)]
async fn a_damaged_pinned_file_is_dehydrated_stays_pinned_and_is_not_hydrated_again() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &garbled(&bytes), SIZE);
	world.local.user_pin(&root_path("a.bin"), PinAttr::Pinned);
	world.local.take_watch_events();
	let world = world.started().await;
	drop(world.read(file));
	world.run(Duration::from_secs(60)).await;
	let entry = world.local.entry(&root_path("a.bin")).unwrap();
	assert_eq!(entry.on_disk, 0);
	assert_eq!(entry.pin, PinAttr::Pinned);
	assert_eq!(world.hydrations("a.bin"), 0);
	assert_eq!(world.damaged().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn unpinning_a_clean_file_dehydrates_it_and_clears_the_unpinned_state() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	world.local.user_pin(&root_path("a.bin"), PinAttr::Unpinned);
	world.run(Duration::from_secs(10)).await;
	let entry = world.local.entry(&root_path("a.bin")).unwrap();
	assert_eq!(entry.on_disk, 0);
	assert_eq!(entry.pin, PinAttr::Unspecified);
	assert!(entry.in_sync);
	assert!(lock(&world.reports.0).contains(&LocalReport::Freed {
		path: root_path("a.bin")
	}));
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn unpinning_a_partly_downloaded_file_frees_it_and_downloads_none_of_the_rest() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	world.placed("a.bin", &bytes, &bytes, 0);
	let world = world.started().await;
	// Partly on disk once the start-up walk is over, which completes such a file.
	world
		.local
		.set_on_disk(&root_path("a.bin"), u64::try_from(SIZE / 2).unwrap());
	world.local.user_pin(&root_path("a.bin"), PinAttr::Unpinned);
	world.run(Duration::from_secs(10)).await;
	let entry = world.local.entry(&root_path("a.bin")).unwrap();
	assert_eq!(entry.on_disk, 0);
	assert!(entry.in_sync);
	assert!(
		!world
			.local
			.ops()
			.iter()
			.any(|op| matches!(op, LocalOp::Hydrated(_) | LocalOp::HashedPartial(_))),
		"{:?}",
		world.local.ops()
	);
	assert!(world.drive.reads().is_empty());
	assert_eq!(world.drive.writes(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn unpinning_a_modified_file_keeps_and_reports_it() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	world
		.local
		.user_write(&root_path("a.bin"), b"mine")
		.unwrap();
	world.local.user_pin(&root_path("a.bin"), PinAttr::Unpinned);
	world.run(Duration::from_secs(30)).await;
	let entry = world.local.entry(&root_path("a.bin")).unwrap();
	assert_eq!(entry.bytes, b"mine");
	assert_eq!(entry.on_disk, 4);
	assert_eq!(entry.pin, PinAttr::Unspecified);
	let reports = lock(&world.reports.0).clone();
	assert!(reports.contains(&LocalReport::KeptDownloaded {
		path: root_path("a.bin")
	}));
	assert!(
		reports
			.iter()
			.any(|report| matches!(report, LocalReport::Uploaded { .. })),
		"the edit is uploaded"
	);
	assert!(
		!reports
			.iter()
			.any(|report| matches!(report, LocalReport::Freed { .. }))
	);
}

#[tokio::test(start_paused = true)]
async fn unpinning_a_directory_frees_its_files_but_never_an_edit_before_it_is_uploaded() {
	let world = unstarted();
	let dir = world.placed_dir("d");
	let mut files = Vec::new();
	for (name, size) in [("a.bin", 5000), ("b.bin", 6000), ("c.bin", 7000)] {
		let path = format!(r"d\{name}");
		files.push(world.placed_in(&dir, &path, &pattern(size)));
		world
			.local
			.set_on_disk(&root_path(&path), u64::try_from(size).unwrap());
	}
	world
		.local
		.user_pin(&root_path(r"d\c.bin"), PinAttr::Pinned);
	let world = world.started().await;
	world
		.local
		.user_write(&root_path(r"d\b.bin"), b"mine")
		.unwrap();
	world.local.user_pin(&root_path("d"), PinAttr::Unpinned);
	world.run(Duration::from_secs(30)).await;
	assert_eq!(world.on_disk(r"d\a.bin"), 0);
	// The edit was being uploaded as the directory was unpinned: it reached the drive first, and
	// only then was its space freed.
	let PlaceholderIdentity::File { stable, .. } = files[1] else {
		unreachable!()
	};
	assert_eq!(world.drive.content(stable), b"mine");
	assert_eq!(world.on_disk(r"d\c.bin"), 7000);
	let pins: Vec<PinAttr> = ["d", r"d\a.bin", r"d\b.bin", r"d\c.bin"]
		.iter()
		.map(|path| world.local.entry(&root_path(path)).unwrap().pin)
		.collect();
	assert_eq!(
		pins,
		[
			PinAttr::Unspecified,
			PinAttr::Unspecified,
			PinAttr::Unspecified,
			PinAttr::Pinned
		]
	);
}

// ---- dehydrations another process asks for ----

impl World {
	/// Asks the engine to dehydrate `path` as another process would, and returns the answers so far.
	async fn user_dehydrate(&self, path: &str) -> Vec<LocalOp> {
		let (request, responder) = self.local.user_dehydrate(&root_path(path)).asked();
		self.engine.on_notify_dehydrate(request, responder);
		self.run(Duration::from_secs(2)).await;
		self.local
			.ops()
			.into_iter()
			.filter(|op| {
				matches!(
					op,
					LocalOp::DehydrateAnswered { .. } | LocalOp::DehydrateAbandoned(_)
				)
			})
			.collect()
	}
}

fn answered(path: &str, allowed: bool) -> LocalOp {
	LocalOp::DehydrateAnswered {
		path: root_path(path),
		allowed,
	}
}

#[tokio::test(start_paused = true)]
async fn a_dehydration_of_a_clean_file_is_allowed_and_done() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	assert_eq!(
		world.user_dehydrate("a.bin").await,
		[answered("a.bin", true)]
	);
	assert_eq!(world.on_disk("a.bin"), 0);
}

#[tokio::test(start_paused = true)]
async fn a_dehydration_of_an_append_only_edit_is_vetoed() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	world
		.local
		.user_append_only(&root_path("a.bin"), b"more")
		.unwrap();
	assert_eq!(
		world.user_dehydrate("a.bin").await,
		[answered("a.bin", false)]
	);
	assert_eq!(world.dehydrations("a.bin"), 0);
}

#[tokio::test(start_paused = true)]
async fn a_dehydration_of_a_file_with_a_job_waiting_is_vetoed() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	// The quiet window holds the hydration job queued.
	drop(world.read(file));
	world.run(Duration::from_secs(1)).await;
	assert_eq!(
		world.user_dehydrate("a.bin").await,
		[answered("a.bin", false)]
	);
	assert_eq!(world.dehydrations("a.bin"), 0);
}

#[tokio::test(start_paused = true)]
async fn a_dehydration_during_a_read_is_vetoed() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	let read = world.read(file);
	assert_eq!(
		world.user_dehydrate("a.bin").await,
		[answered("a.bin", false)]
	);
	drop(read);
}

#[tokio::test(start_paused = true)]
async fn a_dehydration_of_a_pinned_file_is_vetoed() {
	let world = unstarted();
	let dir = world.placed_dir("d");
	world.placed_in(&dir, r"d\a.bin", &pattern(5000));
	world.local.set_on_disk(&root_path(r"d\a.bin"), 5000);
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.local.take_watch_events();
	let world = world.started().await;
	world.run(Duration::from_secs(10)).await;
	assert_eq!(
		world.user_dehydrate(r"d\a.bin").await,
		[answered(r"d\a.bin", false)]
	);
}

// ---- races with the user, and limits ----

#[tokio::test(start_paused = true)]
async fn pinning_a_tree_larger_than_the_queue_hydrates_all_of_it() {
	// Every job slot taken by a directory job queuing more items than the queue holds.
	let config = LocalConfig {
		max_concurrent_jobs: NonZeroUsize::new(2).unwrap(),
		max_concurrent_hydrations: NonZeroUsize::new(2).unwrap(),
		max_queued: NonZeroUsize::new(8).unwrap(),
		..LocalConfig::default()
	};
	let world = unstarted_with(config);
	let dir = world.placed_dir("d");
	let mut files = Vec::new();
	for sub in 0..4 {
		let made = world.drive.add_remote_dir(&dir, &format!("s{sub}"));
		world.local.place_dir(
			&root_path(&format!(r"d\s{sub}")),
			PlaceholderIdentity::Dir {
				uuid: Uuid::parse_str(&made.uuid).unwrap(),
			},
		);
		for n in 0..6 {
			let path = format!(r"d\s{sub}\f{n}.bin");
			world.placed_in(&dir_id(&made.uuid), &path, &pattern(1000 + n));
			files.push((path, u64::try_from(1000 + n).unwrap()));
		}
	}
	let world = world.started().await;
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.run(Duration::from_secs(120)).await;
	for (path, size) in &files {
		assert_eq!(world.on_disk(path), *size, "{path} hydrated");
	}
}

#[tokio::test(start_paused = true)]
async fn pinning_a_never_opened_tree_larger_than_the_queue_hydrates_all_of_it() {
	let config = LocalConfig {
		max_concurrent_jobs: NonZeroUsize::new(2).unwrap(),
		max_concurrent_hydrations: NonZeroUsize::new(2).unwrap(),
		max_queued: NonZeroUsize::new(8).unwrap(),
		..LocalConfig::default()
	};
	let world = unstarted_with(config);
	let dir = world.unopened_dir(&root_dir(), "d");
	let mut files = Vec::new();
	for n in 0..10 {
		world
			.drive
			.add_remote_file(&dir, &format!("f{n}.bin"), &pattern(900 + n));
		files.push((format!(r"d\f{n}.bin"), u64::try_from(900 + n).unwrap()));
	}
	for sub in 0..4 {
		let made = world.remote_dir(&dir, &format!("s{sub}"));
		for n in 0..12 {
			world
				.drive
				.add_remote_file(&made, &format!("f{n}.bin"), &pattern(1000 + n));
			files.push((
				format!(r"d\s{sub}\f{n}.bin"),
				u64::try_from(1000 + n).unwrap(),
			));
		}
	}
	let world = world.started().await;
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.run(Duration::from_secs(300)).await;
	for (path, size) in &files {
		assert_eq!(world.on_disk(path), *size, "{path} hydrated");
	}
	for sub in 0..4 {
		assert_eq!(world.populations(&format!(r"d\s{sub}")), 1);
	}
}

#[tokio::test(start_paused = true)]
async fn a_version_dehydrated_after_its_check_is_checked_again_when_it_is_whole_again() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	drop(world.read(file));
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.hashes("a.bin"), 1, "verified once");
	assert_eq!(
		world.user_dehydrate("a.bin").await,
		[answered("a.bin", true)]
	);
	assert_eq!(world.on_disk("a.bin"), 0);
	// A reader pulls it back in several windows, and the second download is damaged.
	world
		.local
		.downloaded(&root_path("a.bin"), &garbled(&bytes));
	drop(world.read(file));
	world.run(Duration::from_secs(10)).await;
	assert_eq!(world.hashes("a.bin"), 2, "checked again");
	assert_eq!(world.damaged().len(), 1);
	assert_eq!(world.on_disk("a.bin"), 0);
}

#[tokio::test(start_paused = true)]
async fn a_file_pinned_as_its_space_is_freed_stays_pinned_and_on_disk() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	let path = root_path("a.bin");
	world
		.local
		.pin_during(&path, ProviderAct::Dehydrate, &path, PinAttr::Pinned);
	world.local.user_pin(&path, PinAttr::Unpinned);
	world.run(Duration::from_secs(20)).await;
	let entry = world.local.entry(&path).unwrap();
	assert_eq!(entry.pin, PinAttr::Pinned, "the user's pin stands");
	assert_eq!(entry.on_disk, u64::try_from(SIZE).unwrap());
	assert!(
		!lock(&world.reports.0)
			.iter()
			.any(|report| matches!(report, LocalReport::Freed { .. }))
	);
}

#[tokio::test(start_paused = true)]
async fn a_directory_pinned_again_while_its_unpin_is_passed_down_keeps_the_pin_and_its_files() {
	let world = unstarted();
	let dir = world.placed_dir("d");
	for (name, size) in [("a.bin", 5000), ("b.bin", 6000)] {
		let path = format!(r"d\{name}");
		world.placed_in(&dir, &path, &pattern(size));
		world
			.local
			.set_on_disk(&root_path(&path), u64::try_from(size).unwrap());
	}
	let world = world.started().await;
	world.local.pin_during(
		&root_path(r"d\b.bin"),
		ProviderAct::PinChange,
		&root_path("d"),
		PinAttr::Pinned,
	);
	world.local.user_pin(&root_path("d"), PinAttr::Unpinned);
	world.run(Duration::from_secs(30)).await;
	let pins: Vec<PinAttr> = ["d", r"d\a.bin", r"d\b.bin"]
		.iter()
		.map(|path| world.local.entry(&root_path(path)).unwrap().pin)
		.collect();
	assert_eq!(
		pins,
		[PinAttr::Pinned, PinAttr::Unspecified, PinAttr::Unspecified]
	);
	assert_eq!(world.on_disk(r"d\a.bin"), 5000);
	assert_eq!(world.on_disk(r"d\b.bin"), 6000);
}

#[tokio::test(start_paused = true)]
async fn a_pin_cleared_during_the_hash_still_finds_damage() {
	// A pin set again raises a look that queues the job anew; a pin cleared leaves a whole file
	// nothing queues again, so the hash's own verdict must stand.
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &garbled(&bytes), SIZE);
	world.local.user_pin(&root_path("a.bin"), PinAttr::Pinned);
	world.local.take_watch_events();
	world
		.local
		.pin_during_hash(&root_path("a.bin"), PinAttr::Unspecified);
	let world = world.started().await;
	drop(world.read(file));
	world.run(Duration::from_secs(30)).await;
	assert_eq!(world.hashes("a.bin"), 1, "{:?}", world.local.ops());
	assert_eq!(world.damaged().len(), 1);
	let entry = world.local.entry(&root_path("a.bin")).unwrap();
	assert_eq!(entry.on_disk, 0);
	assert_eq!(entry.pin, PinAttr::Unspecified);
}

#[tokio::test(start_paused = true)]
async fn a_file_unpinned_during_a_read_is_freed_once_the_read_ends() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	let file = world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	let read = world.read(file);
	world.local.user_pin(&root_path("a.bin"), PinAttr::Unpinned);
	world.run(Duration::from_secs(20)).await;
	assert_eq!(world.on_disk("a.bin"), u64::try_from(SIZE).unwrap());
	drop(read);
	world.run(Duration::from_secs(20)).await;
	assert_eq!(world.on_disk("a.bin"), 0);
}

#[tokio::test(start_paused = true)]
async fn a_file_under_an_unpinned_directory_in_a_pinned_one_is_freed() {
	let world = unstarted();
	let dir = world.placed_dir("d");
	let sub = world.drive.add_remote_dir(&dir, "s");
	world.local.place_dir(
		&root_path(r"d\s"),
		PlaceholderIdentity::Dir {
			uuid: Uuid::parse_str(&sub.uuid).unwrap(),
		},
	);
	world.placed_in(&dir_id(&sub.uuid), r"d\s\a.bin", &pattern(5000));
	world.local.set_on_disk(&root_path(r"d\s\a.bin"), 5000);
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.local.take_watch_events();
	let world = world.started().await;
	world.local.user_pin(&root_path(r"d\s"), PinAttr::Unpinned);
	world.run(Duration::from_secs(30)).await;
	assert_eq!(world.on_disk(r"d\s\a.bin"), 0);
}

#[tokio::test(start_paused = true)]
async fn an_item_of_a_pinned_directory_that_cannot_be_a_path_is_reported() {
	let world = unstarted();
	let dir = world.placed_dir("d");
	world.placed_in(&dir, r"d\a.bin", &pattern(5000));
	world.local.add_unrepresentable(&root_path("d"));
	let world = world.started().await;
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.run(Duration::from_secs(20)).await;
	assert_eq!(world.on_disk(r"d\a.bin"), 5000);
	assert!(lock(&world.reports.0).contains(&LocalReport::Skipped {
		path: root_path("d"),
		reason: LocalSkip::Unrepresentable,
	}));
}

#[tokio::test(start_paused = true)]
async fn an_allowed_dehydration_refused_as_busy_is_tried_again_and_given_up_after_its_bound() {
	let world = unstarted();
	let bytes = pattern(SIZE);
	world.placed("a.bin", &bytes, &bytes, SIZE);
	world.placed("b.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	world.local.busy_dehydrations(2);
	assert_eq!(
		world.user_dehydrate("a.bin").await,
		[answered("a.bin", true)]
	);
	assert_eq!(world.on_disk("a.bin"), 0, "the third try went through");
	world.local.busy_dehydrations(u32::MAX);
	assert_eq!(
		world.user_dehydrate("b.bin").await,
		[answered("a.bin", true), answered("b.bin", true)]
	);
	assert_eq!(
		world.on_disk("b.bin"),
		u64::try_from(SIZE).unwrap(),
		"kept; the platform fails its caller"
	);
}

#[tokio::test(start_paused = true)]
async fn an_unpin_that_cannot_reach_every_item_takes_back_what_it_passed_down() {
	let world = unstarted();
	let dir = world.placed_dir("d");
	for (name, size) in [("a.bin", 5000), ("b.bin", 6000), ("c.bin", 7000)] {
		let path = format!(r"d\{name}");
		world.placed_in(&dir, &path, &pattern(size));
		world
			.local
			.set_on_disk(&root_path(&path), u64::try_from(size).unwrap());
	}
	let world = world.started().await;
	world
		.local
		.fail_pin_change(&root_path(r"d\c.bin"), Some(LocalError::Busy));
	world.local.user_pin(&root_path("d"), PinAttr::Unpinned);
	for event in world.local.take_watch_events() {
		world.engine.on_local_event(event);
	}
	// The watcher's events for the provider's own pin changes are held back, as a busy watcher's
	// are, so no item's job runs before the user pins the directory again.
	sleep(Duration::from_secs(5)).await;
	drop(world.local.take_watch_events());
	let pins = || -> Vec<PinAttr> {
		["d", r"d\a.bin", r"d\b.bin", r"d\c.bin"]
			.iter()
			.map(|path| world.local.entry(&root_path(path)).unwrap().pin)
			.collect()
	};
	assert_eq!(
		pins(),
		[
			PinAttr::Unpinned,
			PinAttr::Unspecified,
			PinAttr::Unspecified,
			PinAttr::Unspecified
		]
	);
	// Pinned again before the unpin could be passed down whole: nothing under it is freed.
	world.local.user_pin(&root_path("d"), PinAttr::Pinned);
	world.local.fail_pin_change(&root_path(r"d\c.bin"), None);
	world.run(Duration::from_secs(60)).await;
	assert_eq!(
		pins(),
		[
			PinAttr::Pinned,
			PinAttr::Unspecified,
			PinAttr::Unspecified,
			PinAttr::Unspecified
		]
	);
	assert_eq!(world.on_disk(r"d\a.bin"), 5000);
	assert_eq!(world.on_disk(r"d\b.bin"), 6000);
	assert_eq!(world.on_disk(r"d\c.bin"), 7000);
}

#[tokio::test(start_paused = true)]
async fn a_dehydration_of_a_directory_or_with_an_unreadable_cache_row_is_vetoed() {
	let world = unstarted();
	world.placed_dir("d");
	let bytes = pattern(SIZE);
	world.placed("a.bin", &bytes, &bytes, SIZE);
	let world = world.started().await;
	assert_eq!(world.user_dehydrate("d").await, [answered("d", false)]);
	world.drive.fail_query(CacheError::remote("offline"));
	assert_eq!(
		world.user_dehydrate("a.bin").await,
		[answered("d", false), answered("a.bin", false)]
	);
	assert_eq!(world.on_disk("a.bin"), u64::try_from(SIZE).unwrap());
}
