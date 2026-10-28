//! The sync engine for the JS bindings: the mobile one through UniFFI and the desktop one through
//! napi-rs, both handing TypeScript the same surface.
//!
//! Everything outside `uniffi_impl` and `napi_impl` is plain Rust that names no binding type, so
//! each binding can put its own attributes on the same items. The engine's own types that need no
//! conversion carry `#[js_type]` where they are defined; [`types`] holds the mirrors of the rest.
//!
//! Pair ids cross as `u64` (a `bigint` in TS on both bindings, where an `i64` would be a `number`
//! on napi): an engine id, a positive sqlite rowid, always fits.
//!
//! # Shape
//!
//! One [`SyncEngineCore`] per baseline DB per process holds the engine and what the binding knows
//! live about it; [`JsSyncEngine`] handles are the only strong owners outside calls in flight, so
//! a forgotten engine really drops. The core tracks at most one activity per pair (a [`PairSlot`]),
//! reserved under a std mutex before any await and freed only once the engine call it guards has
//! returned, so two racing calls cannot both pass the check. No lock is held across an await.
//!
//! Every pass's events go through an observer that only records and queues (it runs inside the
//! pass, under the account-wide drive-write lock), and reach the listener on the delivery thread
//! (see [`delivery`]), tagged with the pair and a pass id the binding mints.
//!
//! # Single instance
//!
//! The engine's OS lock keeps a second engine off a baseline DB across processes. Android on the
//! pinned toolchain has no file locks, so there only the in-process map here refuses a second
//! engine: an Android app must open the engine in exactly one process.

mod delivery;
#[cfg(feature = "napi")]
mod napi_impl;
mod types;
#[cfg(feature = "uniffi")]
mod uniffi_impl;

use std::{
	collections::{HashMap, VecDeque, hash_map::Entry},
	mem,
	path::{Path, PathBuf},
	sync::{
		Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use chrono::Utc;
use filen_types::fs::Uuid;
use futures::future::join_all;
use tokio::sync::{mpsc::UnboundedSender, oneshot, watch};

pub use self::types::{SyncEventMessage, SyncPairInfo, SyncPassEvent, SyncPassReport, SyncRefusal};
#[cfg(feature = "uniffi")]
pub use self::uniffi_impl::SyncEngineListener;
use self::{
	delivery::{Delivery, Listener, ListenerSlot, spawn_delivery},
	types::{
		SyncConflictPage, SyncPairActivity, SyncPauseOptions, SyncPlanSummary, SyncWatchConfig,
		page, path_string,
	},
};
use crate::{
	Error, ErrorKind,
	auth::Client,
	fs::HasUUID,
	runtime::{do_on_commander, spawn, spawned},
	sync_engine::{
		Backlog, BaselineInUse, ConflictResolution, DeleteGuard, IgnoredPath, LegacyImport, PairId,
		PauseMode, PauseOptions, PlanOutcome, PlannedAction, PlannedConflict, SyncEngine,
		SyncEvent, SyncMode, SyncObserver, UnsyncablePath, WatchConfig, WatchHandle, WatchState,
		WatchStatus,
	},
};

#[cfg(feature = "napi")]
pub(crate) use self::types::sync_refusal;

/// What an engine call that panicked fails with.
const SYNC_ENGINE_PANICKED: &str = "a sync engine call panicked";

/// How many events [`JsSyncEngine::recent_events`] keeps, engine-wide.
const RECENT_EVENTS: usize = 500;

/// How often [`JsSyncEngine::close`] cuts loose a one-shot pass parked on a pause while it waits
/// for that pass to return.
const CLOSE_POLL: Duration = Duration::from_secs(1);

/// What a watch whose loop ended without saying how reports as its last error.
const LOOP_ENDED: &str = "the watch loop ended unexpectedly";

/// Every core in the process, by the baseline DB it has open (see [`claim_key`]). An entry lives
/// exactly as long as its [`BaselineClaim`], which may outlive the core while its engine shuts
/// down; its `Weak` is then dead, and the entry still refuses a second open.
static OPEN_ENGINES: LazyLock<Mutex<HashMap<PathBuf, Weak<SyncEngineCore>>>> =
	LazyLock::new(Mutex::default);

/// `mutex`'s guard, poison or not: every section under these locks leaves the data consistent, and
/// a panic elsewhere must not turn every later call into one.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A pair id as the engine takes it. One that does not fit was never handed out.
fn pair_id_in(pair_id: u64) -> Result<PairId, Error> {
	i64::try_from(pair_id).map_err(|_| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))
}

fn closed() -> Error {
	Error::custom(ErrorKind::InvalidState, "the sync engine is closed")
}

/// Unix epoch milliseconds now; 0 before the epoch.
fn now_ms() -> u64 {
	u64::try_from(Utc::now().timestamp_millis()).unwrap_or(0)
}

/// The key `db_path` is claimed under: its directory resolved, so two spellings of one file meet.
/// The directory must exist, as the engine requires anyway.
fn claim_key(db_path: &Path) -> Result<PathBuf, Error> {
	let file_name = db_path.file_name().ok_or_else(|| {
		Error::custom(
			ErrorKind::InvalidState,
			format!("the sync baseline path {} names no file", db_path.display()),
		)
	})?;
	let dir = db_path
		.parent()
		.filter(|dir| !dir.as_os_str().is_empty())
		.unwrap_or(Path::new("."));
	let dir = std::fs::canonicalize(dir).map_err(|error| {
		Error::custom_with_source(
			ErrorKind::IO,
			error,
			Some(format!(
				"resolving the directory of the sync baseline {}",
				db_path.display()
			)),
		)
	})?;
	Ok(dir.join(file_name))
}

/// The refusal a second open of a claimed baseline gets: the engine's own, so JS reads it the same
/// way whichever of the two refused.
fn baseline_in_use(db_path: PathBuf) -> Error {
	Error::custom_with_source(ErrorKind::IO, BaselineInUse { path: db_path }, None::<&str>)
}

/// This process's claim on one baseline DB, released when the last [`OpenEngine`] goes.
struct BaselineClaim {
	key: PathBuf,
}

impl Drop for BaselineClaim {
	fn drop(&mut self) {
		lock(&OPEN_ENGINES).remove(&self.key);
	}
}

/// The engine and the claim that keeps a second one off its DB in this process. Field order is
/// drop order: the engine goes before the claim.
struct OpenEngine {
	engine: Arc<SyncEngine>,
	_claim: BaselineClaim,
}

/// What the binding is running for a pair.
enum PairSlot {
	/// `watch_pair` is starting a watch.
	Starting {
		watch_id: u64,
	},
	Watching(RunningWatch),
	/// A watch is being stopped; whoever stopped it frees the slot once it has.
	Stopping {
		watch_id: u64,
		status: watch::Receiver<WatchStatus>,
	},
	/// `sync_once` is running a pass.
	SyncingOnce,
}

/// A watch the binding owns: dropping or garbage-collecting its [`JsSyncWatch`] does not stop it.
struct RunningWatch {
	watch_id: u64,
	handle: WatchHandle,
	status: watch::Receiver<WatchStatus>,
}

impl PairSlot {
	fn activity(&self) -> SyncPairActivity {
		match self {
			Self::Starting { .. } => SyncPairActivity::Starting,
			Self::Watching(_) => SyncPairActivity::Watching,
			Self::Stopping { .. } => SyncPairActivity::Stopping,
			Self::SyncingOnce => SyncPairActivity::SyncingOnce,
		}
	}

	fn watch_status(&self) -> Option<WatchStatus> {
		match self {
			Self::Watching(RunningWatch { status, .. }) | Self::Stopping { status, .. } => {
				Some(status.borrow().clone())
			}
			Self::Starting { .. } | Self::SyncingOnce => None,
		}
	}

	/// Why a watch of pair `pair_id` is refused while this slot holds it.
	fn refuses_watch(&self, pair_id: u64) -> Error {
		let why = match self {
			Self::SyncingOnce => "is running a one-shot pass",
			Self::Starting { .. } | Self::Watching(_) | Self::Stopping { .. } => {
				"is already being watched"
			}
		};
		Error::custom(
			ErrorKind::InvalidState,
			format!("sync pair {pair_id} {why}"),
		)
	}

	/// Why a one-shot pass of pair `pair_id` is refused while this slot holds it.
	fn refuses_pass(&self, pair_id: u64) -> Error {
		let why = match self {
			Self::SyncingOnce => "is running a one-shot pass",
			Self::Starting { .. } | Self::Watching(_) | Self::Stopping { .. } => {
				"is being watched; ask its watch for a pass with request_pass"
			}
		};
		Error::custom(
			ErrorKind::InvalidState,
			format!("sync pair {pair_id} {why}"),
		)
	}
}

/// What the binding learned about a pair from its passes' events, for a screen that mounts after
/// they were delivered.
#[derive(Default)]
struct PairLive {
	pass_in_flight: Option<u64>,
	last_report: Option<SyncPassReport>,
	last_pass_completed_at_ms: Option<u64>,
	last_pass_error: Option<String>,
	last_pass_error_kind: Option<ErrorKind>,
	/// The last pause this process made, until a resume clears it.
	pause: Option<PauseRecord>,
}

/// A `pause_pair` call the engine took, for a countdown to its escalation.
struct PauseRecord {
	/// Unix epoch milliseconds.
	at_ms: u64,
	/// What the pause was given, with the default filled in.
	options: SyncPauseOptions,
}

/// The part of the core every pass's observer writes to. Its own `Arc` rather than the core's, so
/// a watch loop holds no strong core, and the events of a pass still running when the core drops
/// still reach the listener.
struct LiveState {
	pairs: Mutex<HashMap<PairId, PairLive>>,
	/// The last [`RECENT_EVENTS`] events, oldest first, without `Progress` and `PassCompleted`.
	recent: Mutex<VecDeque<SyncPassEvent>>,
	deliveries: UnboundedSender<Delivery>,
	/// The next pass id.
	passes: AtomicU64,
}

impl LiveState {
	/// The observer for passes of `pair`. `loop_alive` is dropped with it, which is how a status
	/// pump learns that the watch loop owning it has ended.
	fn observer(
		self: &Arc<Self>,
		pair: PairId,
		pair_id: u64,
		loop_alive: Option<oneshot::Sender<()>>,
	) -> SyncObserver {
		let live = Arc::clone(self);
		let mut pass = None;
		Box::new(move |event| {
			let _loop_alive = &loop_alive;
			live.observe(pair, pair_id, &mut pass, event);
		})
	}

	/// Tag `event` with its pass, record it, and queue it for the listener. `pass` is the id of the
	/// pass in flight: every `PassStarted` mints a fresh one, as does any other event that finds
	/// none, and the pass's terminal event clears it. An `AwaitingRemote` comes before the pass's
	/// `PassStarted` and stands alone, so it clears it too.
	fn observe(&self, pair: PairId, pair_id: u64, pass: &mut Option<u64>, event: SyncEvent) {
		let pass_id = match *pass {
			Some(pass_id) if !matches!(event, SyncEvent::PassStarted { .. }) => pass_id,
			_ => self.passes.fetch_add(1, Ordering::Relaxed),
		};
		let ends = matches!(
			event,
			SyncEvent::PassCompleted { .. }
				| SyncEvent::PassFailed { .. }
				| SyncEvent::AwaitingRemote
		);
		*pass = (!ends).then_some(pass_id);
		let event = SyncPassEvent {
			pair_id,
			pass_id,
			emitted_at_ms: now_ms(),
			event: event.into(),
		};
		self.record(pair, &event);
		// Gone only once the delivery thread is, with nobody left to deliver to.
		let _ = self.deliveries.send(Delivery::Event(event));
	}

	fn record(&self, pair: PairId, event: &SyncPassEvent) {
		{
			let mut pairs = lock(&self.pairs);
			let live = pairs.entry(pair).or_default();
			match &event.event {
				SyncEventMessage::PassStarted { .. } => live.pass_in_flight = Some(event.pass_id),
				SyncEventMessage::PassCompleted { report } => {
					live.pass_in_flight = None;
					live.last_pass_completed_at_ms = Some(event.emitted_at_ms);
					live.last_pass_error = report.pass_error.clone();
					live.last_pass_error_kind = report.pass_error_kind;
					// The binding keeps its own copy for `last_report`; the listener gets the
					// event's. Capped, so at most a few thousand entries.
					live.last_report = Some(report.clone());
					return;
				}
				SyncEventMessage::PassFailed { error, kind } => {
					live.pass_in_flight = None;
					live.last_pass_error = Some(error.clone());
					live.last_pass_error_kind = Some(*kind);
				}
				SyncEventMessage::Progress { .. } => return,
				_ => {}
			}
		}
		let mut recent = lock(&self.recent);
		if recent.len() == RECENT_EVENTS {
			recent.pop_front();
		}
		// The ring keeps its own copy; the listener gets the original.
		recent.push_back(event.clone());
	}
}

/// One baseline DB's engine and everything the binding knows about it (see the module docs).
pub(super) struct SyncEngineCore {
	/// `None` once closed: every later call is refused at once.
	engine: Mutex<Option<Arc<OpenEngine>>>,
	slots: Mutex<HashMap<PairId, PairSlot>>,
	live: Arc<LiveState>,
	listener: ListenerSlot,
	/// The next watch id.
	watches: AtomicU64,
	/// Held across a pause or resume and its bookkeeping, so two racing calls record their pauses
	/// in the order the engine applied them.
	pausing: tokio::sync::Mutex<()>,
	/// The account the engine was opened for (its root folder), which a second open must match to
	/// attach.
	account: Uuid,
	/// The JS environment that opened it (see [`JsSyncEngine::open`]), which a second open must
	/// come from to attach.
	host: u64,
	/// Where [`Drop`] hands the shutdown: the last handle may go on the JS thread.
	runtime: tokio::runtime::Handle,
}

impl SyncEngineCore {
	/// The core for the baseline DB at `db_path`: the running one, if this process has it open for
	/// the same account from the same `host` (its listener replaced by `listener`), or a newly
	/// opened one. Refused with [`BaselineInUse`] when this process has it open for another account
	/// or from another host, or is still shutting an engine on it down. Must run on a runtime,
	/// which outlives the core.
	async fn open(
		client: Arc<Client>,
		db_path: PathBuf,
		host: u64,
		listener: Listener,
	) -> Result<Arc<Self>, Error> {
		let key = claim_key(&db_path)?;
		let account = client.root().uuid();
		let claimed = match lock(&OPEN_ENGINES).entry(key) {
			Entry::Occupied(entry) => Err(entry.get().upgrade()),
			Entry::Vacant(entry) => {
				// Claimed with a dead `Weak` while the engine opens, so a racing open is refused
				// rather than opening the DB a second time.
				let key = entry.key().clone();
				entry.insert(Weak::new());
				Ok(BaselineClaim { key })
			}
		};
		// Decided outside the map's lock: the core found there may be dropped right here, and the
		// claim of a core dropped where its shutdown cannot be spawned takes that lock.
		let claim = match claimed {
			Ok(claim) => claim,
			Err(Some(core)) if core.account == account && core.host == host && core.is_open() => {
				*lock(&core.listener) = listener;
				return Ok(core);
			}
			Err(_) => return Err(baseline_in_use(db_path)),
		};
		// A failed open drops the claim, and with it the entry.
		let engine = SyncEngine::open(client, db_path).await?;
		let listener: ListenerSlot = Arc::new(Mutex::new(listener));
		let deliveries = spawn_delivery(Arc::clone(&listener));
		// Its own copy: the claim keeps one to remove the entry by.
		let key = claim.key.clone();
		let core = Arc::new(Self {
			engine: Mutex::new(Some(Arc::new(OpenEngine {
				engine: Arc::new(engine),
				_claim: claim,
			}))),
			slots: Mutex::default(),
			live: Arc::new(LiveState {
				pairs: Mutex::default(),
				recent: Mutex::default(),
				deliveries,
				passes: AtomicU64::new(1),
			}),
			listener,
			watches: AtomicU64::new(1),
			pausing: tokio::sync::Mutex::new(()),
			account,
			host,
			runtime: tokio::runtime::Handle::current(),
		});
		lock(&OPEN_ENGINES).insert(key, Arc::downgrade(&core));
		Ok(core)
	}

	fn is_open(&self) -> bool {
		lock(&self.engine).is_some()
	}

	fn engine(&self) -> Result<Arc<OpenEngine>, Error> {
		lock(&self.engine)
			.as_ref()
			.map(Arc::clone)
			.ok_or_else(closed)
	}

	/// Reserve `pair`'s slot for `slot` and take the engine, refused with what `refuses` says of a
	/// slot already held. The engine is checked under the slots lock: [`close`](Self::close) takes
	/// the engine before it reads the slots, so a slot reserved here is one it sees. Lock order is
	/// slots, then engine; `close` never holds both.
	fn reserve(
		&self,
		pair: PairId,
		slot: PairSlot,
		refuses: impl FnOnce(&PairSlot) -> Error,
	) -> Result<Arc<OpenEngine>, Error> {
		let mut slots = lock(&self.slots);
		let open = self.engine()?;
		match slots.entry(pair) {
			Entry::Occupied(taken) => Err(refuses(taken.get())),
			Entry::Vacant(free) => {
				free.insert(slot);
				Ok(open)
			}
		}
	}

	/// Free `pair`'s slot if it still holds what `held` matches.
	fn free_slot(&self, pair: PairId, held: impl FnOnce(&PairSlot) -> bool) {
		let mut slots = lock(&self.slots);
		if slots.get(&pair).is_some_and(held) {
			slots.remove(&pair);
		}
	}

	/// Stop every watch, wait for every other activity to end, then let the engine go. A one-shot
	/// pass parked on a pause, or waiting for the cache to finish listing its remote folder, is cut
	/// loose until it returns; a transfer in flight is waited for.
	///
	/// A close that finds the engine already taken waits for the same activities to end, so it
	/// returns no earlier than the first; only the first, which holds the engine, cuts passes
	/// loose. Running the loop twice is safe: [`begin_stop`] hands each watch to one caller.
	async fn close(self: Arc<Self>) -> Result<(), Error> {
		let open = lock(&self.engine).take();
		loop {
			let (running, syncing_once) = {
				let mut slots = lock(&self.slots);
				if slots.is_empty() {
					break;
				}
				let watched: Vec<PairId> = slots
					.iter()
					.filter_map(|(pair, slot)| {
						matches!(slot, PairSlot::Watching(_)).then_some(*pair)
					})
					.collect();
				let running: Vec<(PairId, RunningWatch)> = watched
					.into_iter()
					.filter_map(|pair| Some((pair, begin_stop(&mut slots, pair, None)?)))
					.collect();
				let syncing_once: Vec<PairId> = slots
					.iter()
					.filter_map(|(pair, slot)| {
						matches!(slot, PairSlot::SyncingOnce).then_some(*pair)
					})
					.collect();
				(running, syncing_once)
			};
			join_all(
				running
					.into_iter()
					.map(|(pair, watch)| self.finish_stop(pair, watch)),
			)
			.await;
			if let Some(open) = &open {
				for pair in syncing_once {
					open.engine.cut_loose_pass(pair).await;
				}
			}
			// What is left frees itself: a watch being started sees the engine closed and stops it,
			// a one-shot pass frees its slot when it returns, another stop when it is done.
			if !lock(&self.slots).is_empty() {
				tokio::time::sleep(CLOSE_POLL).await;
			}
		}
		drop(open);
		Ok(())
	}

	/// Stop `watch`, which [`begin_stop`] took out of `pair`'s slot, and free the slot.
	async fn finish_stop(&self, pair: PairId, watch: RunningWatch) {
		let RunningWatch {
			watch_id, handle, ..
		} = watch;
		handle.stop().await;
		self.free_slot(
			pair,
			|slot| matches!(slot, PairSlot::Stopping { watch_id: id, .. } if *id == watch_id),
		);
	}

	/// Stop `pair`'s watch, or only watch `only` of it; a no-op when that is not running.
	async fn stop_watch(&self, pair: PairId, only: Option<u64>) {
		let running = begin_stop(&mut lock(&self.slots), pair, only);
		if let Some(watch) = running {
			self.finish_stop(pair, watch).await;
		}
	}

	/// [`stop_watch`](Self::stop_watch) of `pair_id`'s watch, whichever it is, for JS: refused
	/// rather than a no-op while a watch is starting, which would then run on unstopped.
	async fn stop_watch_for_js(&self, pair_id: u64) -> Result<(), Error> {
		let pair = pair_id_in(pair_id)?;
		self.engine()?;
		let running = {
			let mut slots = lock(&self.slots);
			if matches!(slots.get(&pair), Some(PairSlot::Starting { .. })) {
				return Err(Error::custom(
					ErrorKind::InvalidState,
					format!("sync pair {pair_id} is still starting its watch"),
				));
			}
			begin_stop(&mut slots, pair, None)
		};
		if let Some(watch) = running {
			self.finish_stop(pair, watch).await;
		}
		Ok(())
	}

	async fn list_pairs(&self) -> Result<Vec<SyncPairInfo>, Error> {
		let records = self.engine()?.engine.list_pairs().await?;
		let mut infos = Vec::with_capacity(records.len());
		for record in records {
			let pair = record.id;
			let mut info = SyncPairInfo::try_from(record)?;
			if let Some(slot) = lock(&self.slots).get(&pair) {
				info.activity = slot.activity();
				info.watch_status = slot.watch_status();
			}
			if let Some(live) = lock(&self.live.pairs).get(&pair) {
				info.pass_in_flight = live.pass_in_flight;
				info.last_pass_completed_at_ms = live.last_pass_completed_at_ms;
				info.last_pass_error.clone_from(&live.last_pass_error);
				info.last_pass_error_kind = live.last_pass_error_kind;
				// Only while the engine says the pair is paused: a resume whose sweep failed after
				// the pair had resumed leaves the record behind.
				if info.paused
					&& let Some(pause) = &live.pause
				{
					info.paused_at_ms = Some(pause.at_ms);
					// JS gets its own copy; the binding keeps the record.
					info.pause_options = Some(pause.options.clone());
				}
			}
			infos.push(info);
		}
		Ok(infos)
	}

	fn last_report(&self, pair_id: u64) -> Result<Option<SyncPassReport>, Error> {
		self.engine()?;
		let pair = pair_id_in(pair_id)?;
		// JS gets its own copy; the binding keeps the pair's.
		Ok(lock(&self.live.pairs)
			.get(&pair)
			.and_then(|live| live.last_report.clone()))
	}

	fn recent_events(&self) -> Result<Vec<SyncPassEvent>, Error> {
		self.engine()?;
		// JS gets its own copy; the ring keeps going.
		Ok(lock(&self.live.recent).iter().cloned().collect())
	}

	async fn add_pair(
		&self,
		local_root: String,
		remote_root: Uuid,
		mode: SyncMode,
	) -> Result<u64, Error> {
		let pair = self
			.engine()?
			.engine
			.add_pair(PathBuf::from(local_root), remote_root, mode)
			.await?;
		types::pair_id_out(pair)
	}

	async fn import_legacy_state(
		&self,
		pair_id: u64,
		state_dir: String,
	) -> Result<LegacyImport, Error> {
		let pair = pair_id_in(pair_id)?;
		self.engine()?
			.engine
			.import_legacy_state(pair, PathBuf::from(state_dir))
			.await
	}

	async fn remove_pair(&self, pair_id: u64) -> Result<(), Error> {
		let pair = pair_id_in(pair_id)?;
		self.engine()?.engine.remove_pair(pair).await?;
		// Its loop has already ended on the removal.
		self.stop_watch(pair, None).await;
		// Ids are reused, and a new pair must not inherit this one's history.
		lock(&self.live.pairs).remove(&pair);
		Ok(())
	}

	async fn watch_pair(
		self: Arc<Self>,
		pair_id: u64,
		config: Option<SyncWatchConfig>,
	) -> Result<JsSyncWatch, Error> {
		let pair = pair_id_in(pair_id)?;
		let watch_id = self.watches.fetch_add(1, Ordering::Relaxed);
		let open = self.reserve(pair, PairSlot::Starting { watch_id }, |taken| {
			taken.refuses_watch(pair_id)
		})?;
		let starting = |slot: &PairSlot| matches!(slot, PairSlot::Starting { watch_id: id } if *id == watch_id);
		let (loop_alive, loop_gone) = oneshot::channel();
		let observer = self.live.observer(pair, pair_id, Some(loop_alive));
		let config = config.map_or_else(WatchConfig::default, Into::into);
		let handle = match Arc::clone(&open.engine)
			.watch_with(pair, config, observer)
			.await
		{
			Ok(handle) => handle,
			Err(error) => {
				self.free_slot(pair, starting);
				return Err(error);
			}
		};
		drop(open);
		let status = handle.status();
		// Installed only while the engine is open: a `close` in between does not see a starting
		// watch, so this one stops it instead.
		let refused = {
			let mut slots = lock(&self.slots);
			if self.is_open() && slots.get(&pair).is_some_and(starting) {
				slots.insert(
					pair,
					PairSlot::Watching(RunningWatch {
						watch_id,
						handle,
						status: status.clone(),
					}),
				);
				None
			} else {
				Some(handle)
			}
		};
		if let Some(handle) = refused {
			handle.stop().await;
			self.free_slot(pair, starting);
			return Err(closed());
		}
		tokio::spawn(pump_status(
			StatusPump {
				core: Arc::downgrade(&self),
				deliveries: self.live.deliveries.clone(),
				pair,
				pair_id,
				watch_id,
				status: status.clone(),
			},
			loop_gone,
		));
		Ok(JsSyncWatch {
			core: Arc::downgrade(&self),
			pair,
			pair_id,
			watch_id,
			status,
		})
	}

	async fn sync_once(&self, pair_id: u64) -> Result<SyncPassReport, Error> {
		let pair = pair_id_in(pair_id)?;
		let open = self.reserve(pair, PairSlot::SyncingOnce, |taken| {
			taken.refuses_pass(pair_id)
		})?;
		let mut observer = self.live.observer(pair, pair_id, None);
		let report = open.engine.sync_once_observed(pair, &mut *observer).await;
		self.free_slot(pair, |slot| matches!(slot, PairSlot::SyncingOnce));
		report.map(Into::into)
	}

	async fn request_pass(&self, pair_id: u64) -> Result<(), Error> {
		let pair = pair_id_in(pair_id)?;
		let open = self.engine()?;
		if !matches!(lock(&self.slots).get(&pair), Some(PairSlot::Watching(_))) {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!("sync pair {pair_id} is not being watched"),
			));
		}
		open.engine.request_pass(pair).await
	}

	async fn reconfigure_pair(
		&self,
		pair_id: u64,
		mode: SyncMode,
		backlog: Backlog,
	) -> Result<(), Error> {
		let pair = pair_id_in(pair_id)?;
		self.engine()?
			.engine
			.reconfigure_pair(pair, mode, backlog)
			.await
	}

	async fn set_delete_guard(&self, pair_id: u64, guard: DeleteGuard) -> Result<(), Error> {
		let pair = pair_id_in(pair_id)?;
		self.engine()?.engine.set_delete_guard(pair, guard).await
	}

	async fn pause_pair(
		&self,
		pair_id: u64,
		options: Option<SyncPauseOptions>,
	) -> Result<(), Error> {
		let pair = pair_id_in(pair_id)?;
		let open = self.engine()?;
		let options = options.map_or_else(PauseOptions::default, Into::into);
		let _pausing = self.pausing.lock().await;
		// Taken before the engine call, which sets its escalation deadline on entry and may then
		// wait seconds behind another pair's commit: the countdown must not end after the deadline.
		let at_ms = now_ms();
		open.engine.pause_pair_with(pair, options).await?;
		lock(&self.live.pairs).entry(pair).or_default().pause = Some(PauseRecord {
			at_ms,
			options: options.into(),
		});
		Ok(())
	}

	async fn cancel_paused_actions(&self, pair_id: u64) -> Result<(), Error> {
		let pair = pair_id_in(pair_id)?;
		let open = self.engine()?;
		let _pausing = self.pausing.lock().await;
		open.engine.cancel_paused_actions(pair).await?;
		// The pause has escalated: no countdown is left to show.
		if let Some(pause) = lock(&self.live.pairs)
			.get_mut(&pair)
			.and_then(|live| live.pause.as_mut())
		{
			pause.options.mode = PauseMode::Cancel;
		}
		Ok(())
	}

	async fn resume_pair(&self, pair_id: u64) -> Result<(), Error> {
		let pair = pair_id_in(pair_id)?;
		let open = self.engine()?;
		let _pausing = self.pausing.lock().await;
		open.engine.resume_pair(pair).await?;
		if let Some(live) = lock(&self.live.pairs).get_mut(&pair) {
			live.pause = None;
		}
		Ok(())
	}

	async fn approve_deletions(&self, pair_id: u64, pass_token: &str) -> Result<(), Error> {
		let pair = pair_id_in(pair_id)?;
		self.engine()?
			.engine
			.approve_deletions(pair, pass_token)
			.await;
		Ok(())
	}

	async fn list_conflicts(
		&self,
		pair_id: u64,
		offset: u64,
		limit: u64,
	) -> Result<SyncConflictPage, Error> {
		let pair = pair_id_in(pair_id)?;
		let conflicts = self.engine()?.engine.list_conflicts(pair).await?;
		Ok(SyncConflictPage {
			// A page's copies of at most a thousand entries; the whole list goes right after.
			conflicts: page(&conflicts, offset, limit),
			total: conflicts.len() as u64,
		})
	}

	async fn resolve_conflict(
		&self,
		pair_id: u64,
		rel_path: &str,
		resolution: ConflictResolution,
	) -> Result<Option<String>, Error> {
		let pair = pair_id_in(pair_id)?;
		let quarantined = self
			.engine()?
			.engine
			.resolve_conflict(pair, rel_path, resolution)
			.await?;
		Ok(quarantined.map(path_string))
	}

	async fn retry_path(&self, pair_id: u64, rel_path: &str) -> Result<(), Error> {
		let pair = pair_id_in(pair_id)?;
		self.engine()?.engine.retry_path(pair, rel_path).await
	}

	async fn user_ignore(&self) -> Result<String, Error> {
		self.engine()?.engine.user_ignore().await
	}

	async fn set_user_ignore(&self, patterns: &str) -> Result<(), Error> {
		self.engine()?.engine.set_user_ignore(patterns).await
	}

	async fn plan_pair(&self, pair_id: u64) -> Result<JsSyncPlan, Error> {
		let pair = pair_id_in(pair_id)?;
		let outcome = self.engine()?.engine.plan_pair(pair).await?;
		Ok(JsSyncPlan::new(outcome))
	}
}

impl Drop for SyncEngineCore {
	/// A core dropped without [`close`](JsSyncEngine::close) still stops its watches before its
	/// engine goes. Every other activity holds the core, so watches are all that can be left.
	/// Spawned, never run here: the last handle can go on the JS thread, and stopping a watch joins
	/// its filesystem watcher's thread.
	fn drop(&mut self) {
		let Some(open) = self
			.engine
			.get_mut()
			.unwrap_or_else(PoisonError::into_inner)
			.take()
		else {
			return;
		};
		let slots = mem::take(self.slots.get_mut().unwrap_or_else(PoisonError::into_inner));
		self.runtime.spawn(async move {
			join_all(slots.into_values().filter_map(|slot| match slot {
				PairSlot::Watching(watch) => Some(watch.handle.stop()),
				PairSlot::Starting { .. } | PairSlot::Stopping { .. } | PairSlot::SyncingOnce => {
					None
				}
			}))
			.await;
			drop(open);
		});
	}
}

/// Turn `pair`'s slot from `Watching` to `Stopping` and hand back the running watch, if it holds
/// one (watch `only`, when given).
fn begin_stop(
	slots: &mut HashMap<PairId, PairSlot>,
	pair: PairId,
	only: Option<u64>,
) -> Option<RunningWatch> {
	match slots.remove(&pair)? {
		PairSlot::Watching(watch) if only.is_none_or(|id| id == watch.watch_id) => {
			slots.insert(
				pair,
				PairSlot::Stopping {
					watch_id: watch.watch_id,
					status: watch.status.clone(),
				},
			);
			Some(watch)
		}
		other => {
			slots.insert(pair, other);
			None
		}
	}
}

/// What one watch's status pump needs: never the core itself, which it must not keep alive.
struct StatusPump {
	core: Weak<SyncEngineCore>,
	deliveries: UnboundedSender<Delivery>,
	pair: PairId,
	pair_id: u64,
	watch_id: u64,
	status: watch::Receiver<WatchStatus>,
}

fn is_terminal(status: &WatchStatus) -> bool {
	matches!(status.state, WatchState::Stopped | WatchState::PairRemoved)
}

/// `latest`, or, when it does not say the watch ended, a `Stopped` saying the loop ended anyway.
fn ended(latest: WatchStatus) -> WatchStatus {
	if is_terminal(&latest) {
		return latest;
	}
	WatchStatus {
		state: WatchState::Stopped,
		last_error: Some(LOOP_ENDED.to_owned()),
		// A loop that ended without saying so died, a panic for one: a broken invariant.
		last_error_kind: Some(ErrorKind::Internal),
		..latest
	}
}

/// Deliver a watch's status once, then every change, until it ends; then free the pair's slot if
/// it still holds this watch. `loop_gone` resolves when the watch loop has ended, for any reason (a
/// panic included): the status channel alone never closes while the filesystem watcher holds a
/// sender, so without it a loop that died would look quiet for ever.
async fn pump_status(pump: StatusPump, mut loop_gone: oneshot::Receiver<()>) {
	let StatusPump {
		core,
		deliveries,
		pair,
		pair_id,
		watch_id,
		mut status,
	} = pump;
	let mut current = status.borrow_and_update().clone();
	loop {
		let done = is_terminal(&current);
		let _ = deliveries.send(Delivery::Status {
			pair_id,
			status: current,
		});
		if done {
			break;
		}
		current = tokio::select! {
			changed = status.changed() => match changed {
				Ok(()) => status.borrow_and_update().clone(),
				Err(_) => ended(status.borrow().clone()),
			},
			_ = &mut loop_gone => ended(status.borrow().clone()),
		};
	}
	let Some(core) = core.upgrade() else {
		return;
	};
	let running = {
		let mut slots = lock(&core.slots);
		match slots.remove(&pair) {
			Some(PairSlot::Watching(watch)) if watch.watch_id == watch_id => Some(watch),
			Some(other) => {
				slots.insert(pair, other);
				None
			}
			None => None,
		}
	};
	if let Some(watch) = running {
		// Its loop is done, so this only lets go of the watcher.
		watch.handle.stop().await;
	}
}

/// Run `call` on the commander runtime, as a task of its own: a JS abort drops only the wait for
/// it, never an engine call half-way, nor a slot it reserved.
async fn detached<T, F, Fut>(core: Arc<SyncEngineCore>, call: F) -> Result<T, Error>
where
	T: Send + 'static,
	F: FnOnce(Arc<SyncEngineCore>) -> Fut + Send + 'static,
	Fut: Future<Output = Result<T, Error>> + Send + 'static,
{
	do_on_commander(move || spawned(call(core), SYNC_ENGINE_PANICKED)).await
}

/// A sync engine open on one baseline DB. Every method fails with "the sync engine is closed"
/// after [`close`](Self::close), on every handle of the same engine.
// `Clone` for napi, whose async methods move a clone of the receiver into the promise: it shares
// the core, so a clone lives only while its call is in flight.
#[derive(Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct JsSyncEngine {
	core: Arc<SyncEngineCore>,
}

impl JsSyncEngine {
	/// The engine on the baseline DB at `db_path`, opened or attached to (see
	/// [`SyncEngineCore::open`]), delivering to `listener`. Every binding's open calls this.
	///
	/// `host` names the JS environment opening it: an engine attaches only an open from the one
	/// that opened it, as napi closes an engine with its Node environment. UniFFI opens from one
	/// host, so a JS reload attaches.
	async fn open(
		client: Arc<Client>,
		db_path: String,
		host: u64,
		listener: Listener,
	) -> Result<Self, Error> {
		// A task of its own, as every engine call is: a JS abort must not drop the claim on the
		// baseline while the engine's open still runs on a blocking thread.
		let core = do_on_commander(move || {
			spawned(
				SyncEngineCore::open(client, PathBuf::from(db_path), host, listener),
				SYNC_ENGINE_PANICKED,
			)
		})
		.await?;
		Ok(Self { core })
	}
}

#[filen_macros::js_exports]
// On napi these are reached only through the wrappers napi-derive registers, which it does only
// outside `cfg(test)`, and the tests do not call every one.
#[cfg_attr(all(test, feature = "napi"), allow(dead_code))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl JsSyncEngine {
	/// Stop every watch and let the engine go. A pass parked on a pause, or waiting for the cache
	/// to finish listing its remote folder, is cut loose; a transfer in flight (at most one per
	/// active pair) is waited for. Idempotent: a call made while another
	/// is still closing returns once that one is done.
	#[napi_export]
	pub async fn close(&self) -> Result<(), Error> {
		detached(Arc::clone(&self.core), SyncEngineCore::close).await
	}

	/// Every pair, in registration order, with what the binding is running for each and what it
	/// saw of its passes.
	#[napi_export]
	pub async fn list_pairs(&self) -> Result<Vec<SyncPairInfo>, Error> {
		detached(Arc::clone(&self.core), |core| async move {
			core.list_pairs().await
		})
		.await
	}

	/// The report of the last pass of `pair_id` this process saw complete, capped like every
	/// report. Not persisted.
	#[napi_export]
	pub async fn last_report(&self, pair_id: u64) -> Result<Option<SyncPassReport>, Error> {
		self.core.last_report(pair_id)
	}

	/// The last 500 events of every pair, oldest first, without `Progress` and `PassCompleted`
	/// (its report is [`last_report`](Self::last_report)), to rebuild an activity feed after a JS
	/// reload. Not persisted.
	#[napi_export]
	pub async fn recent_events(&self) -> Result<Vec<SyncPassEvent>, Error> {
		self.core.recent_events()
	}

	/// Register a pair (idempotent for the same roots and mode) and return its id.
	#[napi_export]
	pub async fn add_pair(
		&self,
		local_root: String,
		remote_root: Uuid,
		mode: SyncMode,
	) -> Result<u64, Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.add_pair(local_root, remote_root, mode).await
		})
		.await
	}

	/// Import what the filen-sync pair this one replaces last recorded as synced, from that pair's
	/// state directory, `<dbPath>/state/v2/<pairUuid>/` of the old engine. Call it after
	/// [`add_pair`](Self::add_pair) and before the pair's first pass: it is refused once the pair has
	/// synced anything, and a second import before that replaces the first. A directory without
	/// both of filen-sync's tree files imports nothing. A pass then records a file uploaded without
	/// a BLAKE3 as synced where that state still matches both sides, instead of reading it as
	/// created on both.
	#[napi_export]
	pub async fn import_legacy_state(
		&self,
		pair_id: u64,
		state_dir: String,
	) -> Result<LegacyImport, Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.import_legacy_state(pair_id, state_dir).await
		})
		.await
	}

	/// Forget a pair, stopping its watch. Its files stay on both sides.
	#[napi_export]
	pub async fn remove_pair(&self, pair_id: u64) -> Result<(), Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.remove_pair(pair_id).await
		})
		.await
	}

	/// Start watching a pair: a pass now, then one on every change and on a safety-net timer.
	/// Refused while the pair is watched, running a one-shot pass, or paused. The engine owns the
	/// watch: dropping the returned handle does not stop it.
	#[napi_export]
	pub async fn watch_pair(
		&self,
		pair_id: u64,
		config: Option<SyncWatchConfig>,
	) -> Result<JsSyncWatch, Error> {
		detached(Arc::clone(&self.core), move |core| {
			core.watch_pair(pair_id, config)
		})
		.await
	}

	/// Stop a pair's watch, for a JS context that lost its [`JsSyncWatch`]. A no-op when the pair
	/// is not watched or already stopping. Refused while [`watch_pair`](Self::watch_pair) is still
	/// starting the watch, which no stop can reach yet: stop it once that call has returned.
	#[napi_export]
	pub async fn stop_watch(&self, pair_id: u64) -> Result<(), Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.stop_watch_for_js(pair_id).await
		})
		.await
	}

	/// Run one pass of an unwatched pair; its events reach the listener as a watch's do. Refused
	/// while the pair is watched (ask the watch with [`request_pass`](Self::request_pass)) or
	/// already running one.
	#[napi_export]
	pub async fn sync_once(&self, pair_id: u64) -> Result<SyncPassReport, Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.sync_once(pair_id).await
		})
		.await
	}

	/// Ask a watched pair's watch for a whole pass now (see [`SyncEngine::request_pass`]).
	#[napi_export]
	pub async fn request_pass(&self, pair_id: u64) -> Result<(), Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.request_pass(pair_id).await
		})
		.await
	}

	/// Switch a pair's mode; `backlog` says what becomes of what a backup mode held back. Does not
	/// wake a watch: follow with [`request_pass`](Self::request_pass) or
	/// [`sync_once`](Self::sync_once) to apply it now.
	#[napi_export]
	pub async fn reconfigure_pair(
		&self,
		pair_id: u64,
		mode: SyncMode,
		backlog: Backlog,
	) -> Result<(), Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.reconfigure_pair(pair_id, mode, backlog).await
		})
		.await
	}

	/// Hold a pass's deletions beyond `max(floor, ratio × tracked items)`, from the next pass on.
	/// Refused for a `ratio` outside `0.0..=1.0`. A new pair starts on floor 10, ratio 0.5.
	#[napi_export]
	pub async fn set_delete_guard(
		&self,
		pair_id: u64,
		floor: u64,
		ratio: f64,
	) -> Result<(), Error> {
		let guard = types::delete_guard(floor, ratio)?;
		detached(Arc::clone(&self.core), move |core| async move {
			core.set_delete_guard(pair_id, guard).await
		})
		.await
	}

	/// Turn off the mass-delete guard's volume check for a pair; the holds for untrustworthy
	/// evidence (an incomplete scan, a first sync, an unconverged or empty remote) still apply.
	#[napi_export]
	pub async fn set_delete_guard_unlimited(&self, pair_id: u64) -> Result<(), Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.set_delete_guard(pair_id, DeleteGuard::unlimited())
				.await
		})
		.await
	}

	/// Pause a pair (persisted). `None` suspends the pass in flight and cancels it after 300 s. A
	/// running watch survives the pause. The pause's time and options show on the pair's
	/// [`SyncPairInfo`] until [`resume_pair`](Self::resume_pair), in this process only.
	#[napi_export]
	pub async fn pause_pair(
		&self,
		pair_id: u64,
		options: Option<SyncPauseOptions>,
	) -> Result<(), Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.pause_pair(pair_id, options).await
		})
		.await
	}

	/// Give up the transfers a suspended pause parked now rather than at its `cancel_after_ms`. The
	/// pair stays paused, and its [`SyncPairInfo`] shows the pause's mode as `Cancel` from then on.
	/// Refused when it is not paused.
	#[napi_export]
	pub async fn cancel_paused_actions(&self, pair_id: u64) -> Result<(), Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.cancel_paused_actions(pair_id).await
		})
		.await
	}

	/// Resume a paused pair. A pair paused when the engine opened has no watch: start one with
	/// [`watch_pair`](Self::watch_pair) afterwards.
	#[napi_export]
	pub async fn resume_pair(&self, pair_id: u64) -> Result<(), Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.resume_pair(pair_id).await
		})
		.await
	}

	/// Let the next pass apply the deletions the guard held under `pass_token`, and only those. In
	/// memory only: lost when the process ends.
	#[napi_export]
	pub async fn approve_deletions(&self, pair_id: u64, pass_token: String) -> Result<(), Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.approve_deletions(pair_id, &pass_token).await
		})
		.await
	}

	/// A page of the conflicts the pair holds, in path order, read from the baseline (no network):
	/// at most `limit` of them, and never more than 1000, from `offset` on. `limit` 0 gives only
	/// the total.
	#[napi_export]
	pub async fn list_conflicts(
		&self,
		pair_id: u64,
		offset: u64,
		limit: u64,
	) -> Result<SyncConflictPage, Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.list_conflicts(pair_id, offset, limit).await
		})
		.await
	}

	/// Resolve the conflict held at `rel_path`, from the next pass on. Returns where the local copy
	/// was quarantined when `KeepRemote` replaced different content.
	#[napi_export]
	pub async fn resolve_conflict(
		&self,
		pair_id: u64,
		rel_path: String,
		resolution: ConflictResolution,
	) -> Result<Option<String>, Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.resolve_conflict(pair_id, &rel_path, resolution).await
		})
		.await
	}

	/// Plan a path that kept failing again on the next pass, without waiting out the engine's
	/// hourly retry.
	#[napi_export]
	pub async fn retry_path(&self, pair_id: u64, rel_path: String) -> Result<(), Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.retry_path(pair_id, &rel_path).await
		})
		.await
	}

	/// The device-wide user ignore patterns (gitignore syntax), `""` when unset.
	#[napi_export]
	pub async fn user_ignore(&self) -> Result<String, Error> {
		detached(Arc::clone(&self.core), |core| async move {
			core.user_ignore().await
		})
		.await
	}

	/// Replace the device-wide user ignore patterns; a text with a line that does not parse is
	/// refused whole, naming that line. Wakes every watch.
	#[napi_export]
	pub async fn set_user_ignore(&self, patterns: String) -> Result<(), Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.set_user_ignore(&patterns).await
		})
		.await
	}

	/// What a pass of the pair would do now, without doing it: a dry run that reads both sides.
	/// Works on a paused pair.
	#[napi_export]
	pub async fn plan_pair(&self, pair_id: u64) -> Result<JsSyncPlan, Error> {
		detached(Arc::clone(&self.core), move |core| async move {
			core.plan_pair(pair_id).await
		})
		.await
	}
}

/// A dry run's outcome, read a page at a time: on a first sync of a large tree its lists run to
/// hundreds of MB, which JS should neither copy whole nor keep alive unseen. Every page holds at
/// most 1000 entries; one past the end is empty.
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct JsSyncPlan {
	/// Counted when the plan is made, so it outlives [`release`](Self::release).
	summary: SyncPlanSummary,
	/// `None` once released.
	outcome: Mutex<Option<PlanOutcome>>,
}

impl JsSyncPlan {
	fn new(outcome: PlanOutcome) -> Self {
		Self {
			summary: SyncPlanSummary::from(&outcome),
			outcome: Mutex::new(Some(outcome)),
		}
	}

	fn page_of<T: Clone>(
		&self,
		list: impl FnOnce(&PlanOutcome) -> &[T],
		offset: u64,
		limit: u64,
	) -> Vec<T> {
		lock(&self.outcome)
			.as_ref()
			.map(|outcome| page(list(outcome), offset, limit))
			.unwrap_or_default()
	}
}

/// A plan JS collects without releasing is freed like a released one, off the JS thread.
impl Drop for JsSyncPlan {
	fn drop(&mut self) {
		self.release();
	}
}

#[filen_macros::js_exports]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl JsSyncPlan {
	/// The plan's counts; the same after [`release`](Self::release).
	#[napi_export]
	pub fn summary(&self) -> SyncPlanSummary {
		// JS gets its own copy; the plan keeps its summary.
		self.summary.clone()
	}

	/// The actions a pass would apply, in apply order.
	#[napi_export]
	pub fn actions(&self, offset: u64, limit: u64) -> Vec<PlannedAction> {
		self.page_of(|outcome| outcome.actions.as_slice(), offset, limit)
	}

	/// What the mass-delete guard would hold back.
	#[napi_export]
	pub fn held(&self, offset: u64, limit: u64) -> Vec<PlannedAction> {
		self.page_of(|outcome| outcome.held.as_slice(), offset, limit)
	}

	/// The paths that would be held as conflicts.
	#[napi_export]
	pub fn conflicts(&self, offset: u64, limit: u64) -> Vec<PlannedConflict> {
		self.page_of(|outcome| outcome.conflicts.as_slice(), offset, limit)
	}

	/// The paths the engine will not act on, and why.
	#[napi_export]
	pub fn unsyncable(&self, offset: u64, limit: u64) -> Vec<UnsyncablePath> {
		self.page_of(|outcome| outcome.unsyncable.as_slice(), offset, limit)
	}

	/// The top of each subtree the ignore rules would hide.
	#[napi_export]
	pub fn ignored(&self, offset: u64, limit: u64) -> Vec<IgnoredPath> {
		self.page_of(|outcome| outcome.ignored.as_slice(), offset, limit)
	}

	/// Let the plan's lists go now rather than when JS collects this object: every list is empty
	/// afterwards, and [`summary`](Self::summary) stays. Idempotent.
	#[napi_export]
	pub fn release(&self) {
		// Taken under the lock, and freed on a thread of its own: the call comes on the JS thread,
		// and a first sync's lists are millions of allocations.
		if let Some(released) = lock(&self.outcome).take() {
			spawn(move || drop(released));
		}
	}
}

/// One watch of one pair. The engine owns the watch: dropping this does not stop it.
// `Clone` for napi, as `JsSyncEngine`: every field shares what it points at.
#[derive(Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct JsSyncWatch {
	core: Weak<SyncEngineCore>,
	pair: PairId,
	pair_id: u64,
	watch_id: u64,
	status: watch::Receiver<WatchStatus>,
}

#[filen_macros::js_exports]
// On napi these are reached only through the wrappers napi-derive registers, which it does only
// outside `cfg(test)`, and the tests do not call every one.
#[cfg_attr(all(test, feature = "napi"), allow(dead_code))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl JsSyncWatch {
	/// The pair this watch syncs.
	#[napi_export]
	pub fn pair_id(&self) -> u64 {
		self.pair_id
	}

	/// The watch's latest status.
	#[napi_export]
	pub fn status(&self) -> WatchStatus {
		self.status.borrow().clone()
	}

	/// Stop this watch and wait for its loop to end. A later watch of the same pair is left alone.
	/// Idempotent.
	#[napi_export]
	pub async fn stop(&self) -> Result<(), Error> {
		let Some(core) = self.core.upgrade() else {
			return Ok(());
		};
		let (pair, watch_id) = (self.pair, self.watch_id);
		detached(core, move |core| async move {
			core.stop_watch(pair, Some(watch_id)).await;
			Ok(())
		})
		.await
	}
}

#[cfg(test)]
mod tests;
