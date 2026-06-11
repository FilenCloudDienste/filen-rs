//! The per-search engine task: owns the read-only DB connection (with the `filen_name_matches`
//! function registered, see [`hydrate`]) and the window subscriptions. All filtering, ordering,
//! windowing, and hydration run inside SQLite — the engine keeps no result state beyond each
//! window's last-delivered snapshot (for equality suppression and the terminal fire). Cache
//! events reach it as bare refresh PINGS: any committed batch touching the subtree schedules a
//! debounced re-query of the count and every window.
//!
//! Spawned via [`runtime::spawn_async`](crate::runtime::spawn_async) — on native that is a
//! dedicated thread running a current-thread tokio runtime (so `tokio::time` is available for
//! the debounce and blocking SQLite queries are fine: nothing else runs on it); the loop body is
//! plain async over channels, so the shape ports to a wasm worker later.
//!
//! Channel topology: the PINGS channel's only sender lives inside the sync-root callback on the
//! cache worker — its disconnect is the TERMINAL signal (root deleted server-side, or the worker
//! stopped). The COMMANDS channel's senders are the `Search` (strong) and window handles (weak);
//! the engine exits on an explicit `Shutdown` or when commands disconnect, and keeps answering
//! queries between the terminal signal and shutdown (with the closed pings arm DISABLED —
//! polling a closed receiver in `select!` would spin hot).

use std::{
	ops::Range,
	path::PathBuf,
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicUsize, Ordering},
	},
	time::Duration,
};

use rusqlite::Connection;
use uuid::Uuid;

use super::{
	config::{CompiledFilter, SearchConfig},
	hydrate::{self, Scope},
	result::SearchSnapshot,
};

/// Refresh coalescing interval: bursts of pings (a resync flood) collapse into at most one
/// re-query per interval. A refresh re-runs the count plus every window query — with a needle
/// that scans the whole scope (order of 100–300 ms on a 100k-item drive), so this is
/// deliberately coarser than UI-frame latency. `GetRange`/`SetConfig` replies are exempt.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Per-window callback; receives a FRESH snapshot whenever the window's contents (or the total)
/// change. Runs on the engine task: keep it cheap and non-blocking, and never move the
/// [`Search`](super::Search) (or anything owning it) into one.
pub type SearchWindowCallback = Box<dyn Fn(SearchSnapshot) + Send + 'static>;

/// State mirrored out of the engine for cheap synchronous reads on [`Search`](super::Search).
pub(super) struct SearchShared {
	pub(super) total: AtomicUsize,
	pub(super) live: AtomicBool,
}

pub(super) enum EngineMsg {
	GetRange {
		range: Range<usize>,
		callback: SearchWindowCallback,
		reply: tokio::sync::oneshot::Sender<Result<(SearchSnapshot, u64), String>>,
	},
	DropWindow(u64),
	SetConfig {
		config: SearchConfig,
		reply: tokio::sync::oneshot::Sender<Result<(), String>>,
	},
	Shutdown,
}

struct Window {
	requested_range: Range<usize>,
	callback: SearchWindowCallback,
	last_delivered: SearchSnapshot,
}

/// Sets `live = false` on ANY engine exit — including an unwind — so `is_live()` can never lie
/// about a dead engine. The guard only writes the flag; it never invokes callbacks (re-entering
/// a just-panicked callback during unwind would risk a double panic → abort).
struct LiveGuard(Arc<SearchShared>);

impl Drop for LiveGuard {
	fn drop(&mut self) {
		self.0.live.store(false, Ordering::Release);
	}
}

pub(super) struct EngineInit {
	pub(super) root: Uuid,
	/// `true` when `root` IS the account root: the scope queries then skip the recursive-CTE
	/// subtree walk entirely (everything cached lives under the account root).
	pub(super) is_account_root: bool,
	pub(super) config: SearchConfig,
	pub(super) db_path: PathBuf,
	pub(super) pings: tokio::sync::mpsc::UnboundedReceiver<()>,
	pub(super) commands: tokio::sync::mpsc::UnboundedReceiver<EngineMsg>,
	pub(super) shared: Arc<SearchShared>,
	/// Dropped (after the DB connection) when the loop exits — `Search::close()` awaits it.
	pub(super) finished: tokio::sync::oneshot::Sender<()>,
}

struct Engine {
	root: Uuid,
	is_account_root: bool,
	filter: CompiledFilter,
	conn: Option<Connection>,
	/// `Some` when opening the connection failed terminally — replied to gets/set_configs so the
	/// failure is visible.
	build_error: Option<String>,
	pings: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
	windows: std::collections::HashMap<u64, Window>,
	next_window_id: u64,
	shared: Arc<SearchShared>,
	live: bool,
}

pub(super) async fn run(init: EngineInit) {
	let EngineInit {
		root,
		is_account_root,
		config,
		db_path,
		pings,
		mut commands,
		shared,
		finished,
	} = init;
	let _live_guard = LiveGuard(shared.clone());

	let mut engine = Engine {
		root,
		is_account_root,
		filter: CompiledFilter::compile(&config),
		conn: None,
		build_error: None,
		pings: Some(pings),
		windows: std::collections::HashMap::new(),
		next_window_id: 0,
		shared,
		live: true,
	};
	engine.build(&db_path);

	let mut debounce_deadline: Option<tokio::time::Instant> = None;
	loop {
		tokio::select! {
			biased;
			command = commands.recv() => {
				match command {
					None | Some(EngineMsg::Shutdown) => break,
					Some(EngineMsg::GetRange { range, callback, reply }) => {
						let _ = reply.send(engine.add_window(range, callback));
					}
					Some(EngineMsg::DropWindow(id)) => {
						engine.windows.remove(&id);
					}
					Some(EngineMsg::SetConfig { config, reply }) => {
						let result = engine.set_config(&config);
						// Refresh every window against the new filter right away —
						// `set_config` semantics stay crisp, no debounce lag.
						if result.is_ok() {
							engine.refresh();
							debounce_deadline = None;
						}
						let _ = reply.send(result);
					}
				}
			}
			ping = recv_ping(&mut engine.pings), if engine.pings.is_some() => {
				match ping {
					Some(()) => {
						if debounce_deadline.is_none() {
							debounce_deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
						}
					}
					None => {
						// TERMINAL: the worker dropped our registration (root deleted
						// server-side) or stopped entirely (flush/failure). Disable the pings
						// arm and fire each window ONCE with its last-delivered results — no
						// re-query: in the deleted-root case the subtree rows are already
						// cascade-deleted, so re-querying would deliver an empty "final" view.
						engine.pings = None;
						engine.go_terminal();
						debounce_deadline = None;
					}
				}
			}
			_ = sleep_until(debounce_deadline), if debounce_deadline.is_some() => {
				engine.refresh();
				debounce_deadline = None;
			}
		}
	}

	// Deterministic teardown order: close the read connection BEFORE signalling completion, so
	// a resolved `close()` means "callbacks ceased AND the connection is closed".
	drop(engine);
	drop(finished);
}

async fn recv_ping(pings: &mut Option<tokio::sync::mpsc::UnboundedReceiver<()>>) -> Option<()> {
	match pings {
		Some(receiver) => receiver.recv().await,
		// Unreachable: the select arm is guarded by `pings.is_some()`.
		None => None,
	}
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
	// The `None` case is unreachable: the select arm is guarded by `deadline.is_some()`.
	if let Some(deadline) = deadline {
		tokio::time::sleep_until(deadline).await;
	}
}

impl Engine {
	/// The query scope, derived per use because [`set_config`](Self::set_config) can flip
	/// `recursive`.
	fn scope(&self) -> Scope {
		if !self.filter.recursive {
			Scope::Children(self.root)
		} else if self.is_account_root {
			Scope::Account
		} else {
			Scope::Subtree(self.root)
		}
	}

	/// Open the read connection and prime the shared total. There is no index to build: queries
	/// always read the CURRENT committed DB, so batches committed before this point are covered
	/// by the queries themselves and later ones arrive as refresh pings.
	fn build(&mut self, db_path: &std::path::Path) {
		let result = (|| -> rusqlite::Result<(Connection, usize)> {
			let conn = hydrate::open_read_connection(db_path)?;
			let total = hydrate::count_results(&conn, self.scope(), &self.filter)?;
			Ok((conn, total))
		})();
		match result {
			Ok((conn, total)) => {
				self.shared.total.store(total, Ordering::Release);
				self.conn = Some(conn);
			}
			Err(e) => {
				log::error!("search engine failed to open its read connection: {e}");
				self.build_error = Some(e.to_string());
			}
		}
	}

	fn set_config(&mut self, config: &SearchConfig) -> Result<(), String> {
		if self.conn.is_none() {
			return Err(self
				.build_error
				.clone()
				.unwrap_or_else(|| "search connection unavailable".to_string()));
		}
		self.filter = CompiledFilter::compile(config);
		Ok(())
	}

	/// One window's fresh contents plus the current total, straight from the DB.
	fn snapshot(&self, range: &Range<usize>) -> rusqlite::Result<SearchSnapshot> {
		let Some(conn) = &self.conn else {
			return Ok(SearchSnapshot {
				results: Vec::new(),
				total: 0,
				live: self.live,
			});
		};
		let results = hydrate::window_results(conn, self.scope(), &self.filter, range)?;
		let total = hydrate::count_results(conn, self.scope(), &self.filter)?;
		Ok(SearchSnapshot {
			results,
			total,
			live: self.live,
		})
	}

	fn add_window(
		&mut self,
		range: Range<usize>,
		callback: SearchWindowCallback,
	) -> Result<(SearchSnapshot, u64), String> {
		if self.conn.is_none() {
			return Err(self
				.build_error
				.clone()
				.unwrap_or_else(|| "search connection unavailable".to_string()));
		}
		let snapshot = self.snapshot(&range).map_err(|e| e.to_string())?;
		let id = self.next_window_id;
		self.next_window_id += 1;
		self.windows.insert(
			id,
			Window {
				requested_range: range,
				callback,
				last_delivered: snapshot.clone(),
			},
		);
		Ok((snapshot, id))
	}

	/// Re-query the count plus EVERY window and fire the callbacks whose snapshots changed
	/// (identical snapshots are suppressed). There is no dirt tracking — a ping just means
	/// "something in the subtree changed" and the queries re-read committed truth. A failed
	/// query (e.g. a busy timeout) keeps that window's last-delivered snapshot; the next ping
	/// retries. A panicking callback is caught (mirroring the cache worker's dispatch) and its
	/// window dropped — it never kills the engine.
	fn refresh(&mut self) {
		let Some(conn) = &self.conn else {
			return;
		};
		let scope = self.scope();
		let total = match hydrate::count_results(conn, scope, &self.filter) {
			Ok(total) => total,
			Err(e) => {
				log::error!("search refresh failed counting results: {e}");
				return;
			}
		};
		self.shared.total.store(total, Ordering::Release);
		let ids: Vec<u64> = self.windows.keys().copied().collect();
		let mut poisoned: Vec<u64> = Vec::new();
		for id in ids {
			let window = &self.windows[&id];
			let results =
				match hydrate::window_results(conn, scope, &self.filter, &window.requested_range) {
					Ok(results) => results,
					Err(e) => {
						log::error!("search refresh failed querying window {id}: {e}");
						continue;
					}
				};
			let snapshot = SearchSnapshot {
				results,
				total,
				live: self.live,
			};
			let window = self.windows.get_mut(&id).expect("window present");
			if snapshot.results == window.last_delivered.results
				&& snapshot.total == window.last_delivered.total
				&& snapshot.live == window.last_delivered.live
			{
				continue;
			}
			window.last_delivered = snapshot.clone();
			let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
				(window.callback)(snapshot);
			}));
			if result.is_err() {
				log::error!("search window {id} callback panicked; dropping the window");
				poisoned.push(id);
			}
		}
		for id in poisoned {
			self.windows.remove(&id);
		}
	}

	/// The one-time terminal fire: every window gets its LAST-DELIVERED results re-sent with
	/// `live: false`.
	fn go_terminal(&mut self) {
		if !self.live {
			return;
		}
		self.live = false;
		self.shared.live.store(false, Ordering::Release);
		let mut poisoned: Vec<u64> = Vec::new();
		for (id, window) in &mut self.windows {
			window.last_delivered.live = false;
			let snapshot = window.last_delivered.clone();
			let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
				(window.callback)(snapshot);
			}));
			if result.is_err() {
				log::error!("search window {id} callback panicked; dropping the window");
				poisoned.push(*id);
			}
		}
		for id in poisoned {
			self.windows.remove(&id);
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{borrow::Cow, path::PathBuf, sync::Mutex, time::Instant};

	use filen_types::auth::FileEncryptionVersion;

	use crate::{cache::CacheState, crypto::file::FileKey, fs::file::cache::CacheableFile};

	use super::super::config::SearchConfig;
	use super::*;

	fn temp_db_path() -> PathBuf {
		std::env::temp_dir().join(format!("filen_search_engine_test_{}.db", Uuid::new_v4()))
	}

	fn test_file(parent: Uuid, name: &str) -> CacheableFile<'static> {
		CacheableFile {
			uuid: Uuid::new_v4(),
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
			key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3)
				.unwrap(),
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
				db_path,
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
			let (reply_sender, reply_receiver) = tokio::sync::oneshot::channel();
			self.commands
				.send(EngineMsg::GetRange {
					range,
					callback,
					reply: reply_sender,
				})
				.unwrap();
			reply_receiver.blocking_recv().unwrap().unwrap()
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

		// Terminal but still answering queries over the (now-frozen) cache.
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
}
