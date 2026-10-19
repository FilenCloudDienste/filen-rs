use std::{borrow::Cow, path::PathBuf, sync::Mutex, time::Instant};

use filen_types::{auth::FileEncryptionVersion, fs::StableUuid};

use crate::{cache::CacheState, crypto::file::FileKey, fs::file::cache::CacheableFile};

use super::super::config::SearchConfig;
use super::*;

fn temp_db_path() -> PathBuf {
	std::env::temp_dir().join(format!("filen_search_engine_test_{}.db", Uuid::new_v4()))
}

fn test_file(parent: Uuid, name: &str) -> CacheableFile<'static> {
	let uuid = Uuid::new_v4();
	CacheableFile {
		uuid,
		stable_uuid: StableUuid::new_for_test(uuid),
		parent,
		chunks_size: 1,
		chunks: 1,
		favorited: false,
		region: Cow::Borrowed("region"),
		bucket: Cow::Borrowed("bucket"),
		timestamp: chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
		name: Cow::Owned(name.to_string()),
		size: 1,
		mime: Cow::Borrowed("text/plain"),
		key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3).unwrap(),
		last_modified: chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
		created: None,
		hash: None,
	}
}

struct TestEngine {
	commands: tokio::sync::mpsc::UnboundedSender<EngineMsg>,
	pings: Option<tokio::sync::mpsc::UnboundedSender<()>>,
	shared: Arc<SearchShared>,
	finished: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl TestEngine {
	/// Mirror the production shape: the engine future on its own thread + current-thread
	/// runtime (what `runtime::spawn_async` does on native).
	fn spawn(db_path: PathBuf, root: Uuid) -> Self {
		let (ping_sender, ping_receiver) = tokio::sync::mpsc::unbounded_channel();
		let (command_sender, command_receiver) = tokio::sync::mpsc::unbounded_channel();
		let shared = Arc::new(SearchShared {
			total: AtomicUsize::new(0),
			live: AtomicBool::new(true),
		});
		let (finished_sender, finished_receiver) = tokio::sync::oneshot::channel();
		let init = EngineInit {
			root,
			is_account_root: true,
			config: SearchConfig::new(),
			read_source: ReadSource(db_path),
			pings: ping_receiver,
			commands: command_receiver,
			shared: shared.clone(),
			finished: finished_sender,
		};
		std::thread::spawn(move || {
			let runtime = tokio::runtime::Builder::new_current_thread()
				.enable_all()
				.build()
				.unwrap();
			runtime.block_on(run(init));
		});
		Self {
			commands: command_sender,
			pings: Some(ping_sender),
			shared,
			finished: Some(finished_receiver),
		}
	}

	fn get_range(
		&self,
		range: Range<usize>,
		callback: SearchWindowCallback,
	) -> (SearchSnapshot, u64) {
		self.try_get_range(range, callback).unwrap()
	}

	fn try_get_range(
		&self,
		range: Range<usize>,
		callback: SearchWindowCallback,
	) -> Result<(SearchSnapshot, u64), String> {
		let (reply_sender, reply_receiver) = tokio::sync::oneshot::channel();
		self.commands
			.send(EngineMsg::GetRange {
				range,
				callback,
				reply: reply_sender,
			})
			.unwrap();
		reply_receiver.blocking_recv().unwrap()
	}

	fn send_ping(&self) {
		self.pings.as_ref().unwrap().send(()).unwrap();
	}

	fn set_config(&self, config: SearchConfig) {
		let (reply_sender, reply_receiver) = tokio::sync::oneshot::channel();
		self.commands
			.send(EngineMsg::SetConfig {
				config,
				reply: reply_sender,
			})
			.unwrap();
		reply_receiver.blocking_recv().unwrap().unwrap();
	}
}

type Captured = Arc<Mutex<Vec<SearchSnapshot>>>;

fn capturing_callback() -> (Captured, SearchWindowCallback) {
	let captured: Captured = Arc::new(Mutex::new(Vec::new()));
	let sink = captured.clone();
	let callback: SearchWindowCallback = Box::new(move |snapshot| {
		sink.lock().unwrap().push(snapshot);
	});
	(captured, callback)
}

fn wait_for<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
	let deadline = Instant::now() + timeout;
	loop {
		if let Some(value) = probe() {
			return value;
		}
		assert!(Instant::now() < deadline, "timed out waiting for condition");
		std::thread::sleep(Duration::from_millis(20));
	}
}

/// account_root with one cached file named "initial". The writer (`CacheState`) is returned
/// so tests can COMMIT rows before pinging — the engine queries the committed DB directly,
/// mirroring the production post-commit dispatch.
fn populated_db() -> (PathBuf, Uuid, CacheState) {
	let path = temp_db_path();
	let root = Uuid::new_v4();
	let mut state = CacheState::new_on_path(&path, root);
	state
		.upsert_files(std::iter::once(&test_file(root, "initial")))
		.unwrap();
	(path, root, state)
}

#[test]
fn adaptive_debounce_clamps_to_floor_and_ceiling() {
	assert_eq!(next_debounce(Duration::ZERO), DEBOUNCE, "floor");
	assert_eq!(
		next_debounce(Duration::from_millis(50)),
		DEBOUNCE,
		"cheap refreshes keep the minimum latency"
	);
	assert_eq!(
		next_debounce(Duration::from_millis(700)),
		Duration::from_millis(2100),
		"slow refreshes spend at most ~1/3 of the time re-querying"
	);
	assert_eq!(
		next_debounce(Duration::from_secs(10)),
		MAX_DEBOUNCE,
		"ceiling bounds staleness"
	);
}

#[test]
fn shutdown_resolves_even_with_other_command_senders_alive() {
	let (path, root, _state) = populated_db();
	let mut engine = TestEngine::spawn(path, root);
	let (_captured, callback) = capturing_callback();
	let (snapshot, _id) = engine.get_range(0..10, callback);
	assert_eq!(snapshot.results.len(), 1);

	// A second live sender (what an outstanding window handle amounts to) must NOT block
	// shutdown — the explicit Shutdown message wins over channel-disconnect semantics.
	let _extra_sender = engine.commands.clone();
	engine.commands.send(EngineMsg::Shutdown).unwrap();
	engine
		.finished
		.take()
		.unwrap()
		.blocking_recv()
		.unwrap_or(());
}

/// The per-search window cap bounds the batched refresh closure (one synchronous ReadTask
/// re-queries every window on wasm): the cap'th add errors, and dropping a window frees a
/// slot.
#[test]
fn window_cap_rejects_excess_windows_and_frees_on_drop() {
	let (path, root, _state) = populated_db();
	let engine = TestEngine::spawn(path, root);
	let mut ids = Vec::new();
	for _ in 0..MAX_WINDOWS {
		let (_captured, callback) = capturing_callback();
		let (_snapshot, id) = engine.get_range(0..1, callback);
		ids.push(id);
	}
	let (_captured, callback) = capturing_callback();
	let err = engine
		.try_get_range(0..1, callback)
		.expect_err("window {MAX_WINDOWS} must be rejected");
	assert!(err.contains("too many live search windows"), "got: {err}");

	engine.commands.send(EngineMsg::DropWindow(ids[0])).unwrap();
	let (_captured, callback) = capturing_callback();
	engine
		.try_get_range(0..1, callback)
		.expect("a freed slot admits a new window");
}

#[test]
fn terminal_fires_final_live_false_snapshot_and_keeps_serving() {
	let (path, root, _state) = populated_db();
	let mut engine = TestEngine::spawn(path, root);
	let (captured, callback) = capturing_callback();
	let (snapshot, _id) = engine.get_range(0..10, callback);
	assert!(snapshot.live);
	assert_eq!(snapshot.results.len(), 1);

	// The worker dropping the registration = pings sender dropped.
	engine.pings = None;

	let final_snapshot = wait_for(Duration::from_secs(2), || {
		captured.lock().unwrap().last().cloned()
	});
	assert!(!final_snapshot.live, "the terminal fire carries live=false");
	assert_eq!(
		final_snapshot.results.len(),
		1,
		"last-delivered results re-sent"
	);
	assert!(!engine.shared.live.load(Ordering::Acquire));

	let (_captured2, callback2) = capturing_callback();
	let (frozen, _id2) = engine.get_range(0..10, callback2);
	assert!(!frozen.live);
	assert_eq!(frozen.results.len(), 1);
}

#[test]
fn debounce_coalesces_a_burst_into_one_callback() {
	let (path, root, mut state) = populated_db();
	let engine = TestEngine::spawn(path, root);
	let (captured, callback) = capturing_callback();
	let (snapshot, _id) = engine.get_range(0..10, callback);
	assert_eq!(snapshot.total, 1);

	// Commit-then-ping, like the worker's callback does.
	for name in ["b1", "b2", "b3"] {
		let file = test_file(root, name);
		state.upsert_files(std::iter::once(&file)).unwrap();
		engine.send_ping();
	}

	let snapshot = wait_for(Duration::from_secs(2), || {
		captured.lock().unwrap().last().cloned()
	});
	assert_eq!(snapshot.total, 4);
	assert_eq!(snapshot.results.len(), 4);
	// The burst coalesced: exactly one (debounced) refresh, not three.
	std::thread::sleep(Duration::from_millis(400));
	assert_eq!(captured.lock().unwrap().len(), 1);
}

#[test]
fn total_change_after_the_window_still_fires_its_callback() {
	let (path, root, mut state) = populated_db();
	let engine = TestEngine::spawn(path, root);
	// Window exactly covers the single existing result.
	let (captured, callback) = capturing_callback();
	let (snapshot, _id) = engine.get_range(0..1, callback);
	assert_eq!(snapshot.total, 1);

	// Sorts AFTER the window ("initial" < "zzz") — contents unchanged, total changes.
	let file = test_file(root, "zzz");
	state.upsert_files(std::iter::once(&file)).unwrap();
	engine.send_ping();

	let snapshot = wait_for(Duration::from_secs(2), || {
		captured.lock().unwrap().last().cloned()
	});
	assert_eq!(snapshot.total, 2, "delivered totals never go stale");
	assert_eq!(snapshot.results.len(), 1, "window contents unchanged");
}

#[test]
fn set_config_refreshes_windows_immediately() {
	let (path, root, _state) = populated_db();
	let engine = TestEngine::spawn(path, root);
	let (captured, callback) = capturing_callback();
	let (snapshot, _id) = engine.get_range(0..10, callback);
	assert_eq!(snapshot.total, 1);

	engine.set_config(SearchConfig::new().with_name("no-such-name"));

	// The refresh runs before the set_config reply, no debounce involved — but poll anyway
	// to stay robust against scheduling.
	let snapshot = wait_for(Duration::from_secs(2), || {
		captured.lock().unwrap().last().cloned()
	});
	assert_eq!(snapshot.total, 0);
	assert!(snapshot.results.is_empty());
	assert_eq!(engine.shared.total.load(Ordering::Acquire), 0);
}

#[test]
fn panicking_window_callback_is_dropped_and_engine_survives() {
	let (path, root, mut state) = populated_db();
	let engine = TestEngine::spawn(path, root);
	let panicking: SearchWindowCallback = Box::new(|_| panic!("window callback panic"));
	let (_snapshot, _panicking_id) = engine.get_range(0..10, panicking);
	let (captured, callback) = capturing_callback();
	let (_snapshot2, _id2) = engine.get_range(0..10, callback);

	let file = test_file(root, "new");
	state.upsert_files(std::iter::once(&file)).unwrap();
	engine.send_ping();

	// The healthy window still gets its refresh; the engine did not die.
	let snapshot = wait_for(Duration::from_secs(2), || {
		captured.lock().unwrap().last().cloned()
	});
	assert_eq!(snapshot.total, 2);
	let (_captured3, callback3) = capturing_callback();
	let (alive, _id3) = engine.get_range(0..10, callback3);
	assert_eq!(alive.total, 2, "engine still answers after the panic");
}
