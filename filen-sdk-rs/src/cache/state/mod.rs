use std::{
	collections::HashMap,
	iter::once,
	path::Path,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use crate::{
	ErrorKind,
	auth::Client,
	fs::{
		categories::{DirType, Normal},
		dir::cache::CacheableDir,
		file::cache::CacheableFile,
	},
	io::{RemoteDirectory, RemoteFile},
	socket::DecryptedSocketEvent,
	util::PeekableReceiver,
};
use filen_types::traits::CowHelpers;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

// The worker is hosted by `runtime::spawn_async`: on native that is a dedicated thread's
// current-thread tokio runtime (tokio::time available); on wasm it is a web worker with no tokio
// runtime, so wasmtimer drives the retry timer.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::{Instant as TimerInstant, sleep_until};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::{std::Instant as TimerInstant, tokio::sleep_until};

use crate::cache::{
	CacheError,
	handle::{CacheMessage, ResyncProgress},
	search::ReadTask,
	sql::PersistedEvent,
};
const BATCH_SIZE: usize = 256;

/// Whether the membership gate must vet an event's parent before an upsert. SOCKET events are
/// [`Checked`](Self::Checked): the account-wide subscription delivers events for the whole
/// drive, and out-of-root ones must not be stored. Resync synthetics are
/// [`TrustedSynthetic`](Self::TrustedSynthetic): the diff computed them against the ANCHORED
/// subtree listing, so in-root membership holds by construction and the per-event ancestry
/// walk (one recursive CTE per event, ~166k of them on a populate) is pure waste — a trusted
/// `Move` in particular is always an in-root re-parent, never a move-out delete.
#[derive(Clone, Copy, PartialEq)]
enum EventTrust {
	Checked,
	TrustedSynthetic,
}

/// One post-commit dispatch unit: an applied event (shared via `Arc` so fan-out to multiple owning
/// roots reuses one allocation) paired with the sync roots that should receive it.
type DispatchEntry = (Arc<CacheEvent<'static>>, Vec<Uuid>);

/// The drain's cross-batch state: the gap-free frontier (and whether it broke), plus the sticky
/// resync flag — threaded through [`CacheState::apply_drain_batch`] by value so an aborted bulk
/// pass leaves the caller's copy untouched for the per-event re-run.
#[derive(Clone, Copy)]
struct DrainCursor {
	frontier: Option<u64>,
	frontier_broken: bool,
	resync_needed: bool,
}
/// Bounded-memory backstop: when the worker cannot drain fast enough and the channel reaches this
/// many buffered events, the socket router SHEDS further events and latches a `shed` flag; the
/// worker then records a durable resync — so a shed costs a resync, never a silent loss.
/// This bounds the worst-case PEAK (each event is on the order of a few hundred bytes, so tens of
/// MiB here) — important on the mobile (UniFFI) target. The cap is the channel's CAPACITY, i.e.
/// semaphore permits: nothing is preallocated, and `try_send` enforces the bound atomically.
const EVENT_SHED_CAP: usize = 50_000;

/// The resync's drive-lock poll cadence (fibonacci ramp capped at 2 s) and how many polls one
/// acquisition makes before giving up. This is a SINGLE, PATIENT acquisition: the worker drains
/// events and serves read queries CONCURRENTLY while it waits (see `run_resync`), so it does not
/// need to give up early for liveness — and a single acquisition keeps ONE lock uuid for its
/// whole wait, so if the server queues waiters we hold our place instead of going to the back of
/// the line behind the unbounded FS-op acquirers on every retry (the starvation that froze
/// resyncs under contention). ~60 polls x 2 s ~= 2 min before falling back to the retry timer.
const RESYNC_LOCK_MAX_SLEEP: Duration = Duration::from_secs(2);
const RESYNC_LOCK_PATIENT_ATTEMPTS: usize = 60;
/// After a patient acquisition still times out (or any listing failure), the worker waits this
/// long before re-attempting — so even a quiet account with no further events re-tries.
const RESYNC_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// `None` under unit-test construction (no live client); the resync path logs and no-ops when
/// it is absent.
#[derive(Clone)]
pub(crate) struct ResyncDeps {
	pub(crate) client: Arc<Client>,
}

/// One sync root's lock-held listing: `(root uuid, the node to materialize as the diff anchor —
/// `None` for the account root, listed dirs, listed files)`.
type RootListing = (
	Uuid,
	Option<RemoteDirectory>,
	Vec<RemoteDirectory>,
	Vec<RemoteFile>,
);

/// The whole lock-held resync listing handed from [`run_resync`](CacheState::run_resync)'s network
/// "island" to [`finalize_resync`](CacheState::finalize_resync). Named rather than a 4-tuple so the
/// scalar `any_transient`/`remote_under_lock` fields stay legible at the seam (and so the two
/// `#[allow(clippy::type_complexity)]`s the tuple needed are gone).
struct ResyncListing {
	/// Per-root listings to apply.
	per_root_raw: Vec<RootListing>,
	/// Roots the server reported definitively gone — `finalize_resync` evicts their subtrees.
	deleted_roots: Vec<Uuid>,
	/// At least one root failed with a transient (non-not-found) error this pass.
	any_transient: bool,
	/// The drive snapshot id read under the lock; the watermark advances to it on a clean apply.
	remote_under_lock: u64,
}

pub(crate) struct CacheState {
	pub(crate) db: rusqlite::Connection,
	/// Bounded at [`EVENT_SHED_CAP`]: the socket router must never block, so it uses `try_send` and
	/// sheds + flags a resync when full.
	event_receiver: tokio::sync::mpsc::Receiver<CacheThreadEvent>,
	control_receiver: PeekableReceiver<CacheControlMessage>,
	msg_sender: tokio::sync::mpsc::Sender<Vec<CacheMessage>>,
	/// Shed latch: set by the socket router when the BOUNDED event channel was full and it had
	/// to drop events. The worker observes it once per drain and records a durable resync to recover the
	/// dropped events, then clears it.
	shed: Arc<AtomicBool>,
	/// The account-root uuid — the single `roots` row, used for DB init. NOT necessarily a sync root
	/// (see `sync_roots`).
	pub(crate) root_uuid: Uuid,
	/// The account-root rowid (the lone `roots.id`), cached by `init_db` and fixed for the DB's
	/// lifetime. Bound directly into every bulk upsert's `items.root_id` so the apply path never
	/// re-derives it via a per-row `roots` subquery. Placeholder `0` until `init_db` sets it (rowids
	/// start at 1, so `0` is never a valid value that could leak into a write).
	pub(crate) root_id: i64,
	/// Configured sync roots → their live registrations. An item is cached iff it
	/// descends from one of these roots (the membership gate, in `sql/membership.rs`); EMPTY ⇒
	/// nothing is cached. The production worker starts EMPTY — registrations arrive via
	/// [`CacheControlMessage::AddSyncRoot`], and a uuid stops being a sync root when its LAST
	/// registration is removed.
	sync_roots: HashMap<Uuid, RootRegistrations>,
	/// Client + runtime handle for the write-locked resync island. `None` in unit tests.
	resync: Option<ResyncDeps>,
	/// Deadline of the one-shot retry timer armed by a NON-converged resync attempt (lock
	/// contention, transient listing failures, partial convergence): the run loop's
	/// lowest-priority select arm sleeps until it and re-attempts. The retry itself is gated on
	/// the durable `needs_resync` flag, so a stale fire is a no-op. `None` ⇒ nothing scheduled.
	resync_retry: Option<TimerInstant>,
	/// Search read queries served against THIS connection between drains — the wasm read path
	/// (the wasm VFS supports neither WAL nor a second connection). Native search engines read
	/// via their own connection and never send here. `None` once every sender is gone (the arm
	/// disables itself, like the engine's ping arm) and in unit-test construction.
	read_tasks: Option<UnboundedReceiver<ReadTask>>,
}

#[cfg(test)]
impl CacheState {
	pub(crate) fn new_in_memory() -> Self {
		let root_uuid = Uuid::new_v4();
		let (event_sender, event_receiver) = tokio::sync::mpsc::channel(EVENT_SHED_CAP);
		let (control_sender, control_receiver) = tokio::sync::mpsc::unbounded_channel();
		drop(event_sender);
		drop(control_sender);
		let (msg_sender, msg_receiver) = tokio::sync::mpsc::channel(1);
		drop(msg_receiver);

		let mut state = Self {
			db: rusqlite::Connection::open_in_memory().unwrap(),
			event_receiver,
			control_receiver: PeekableReceiver::new(control_receiver),
			msg_sender,
			shed: Arc::new(AtomicBool::new(false)),
			root_uuid,
			// Placeholder; `init_db` (called below) replaces it with the real account-root rowid.
			root_id: 0,
			sync_roots: whole_account_sync_roots(root_uuid),
			resync: None,
			resync_retry: None,
			read_tasks: None,
		};
		state.init_db().unwrap();
		state
	}
}

/// `new_on_path` is ALSO compiled under `bench-internals` (the criterion insertion benchmark needs a
/// file-backed `CacheState`); the in-memory test constructors stay test-only.
#[cfg(any(test, feature = "bench-internals"))]
impl CacheState {
	/// Open a file-backed cache at `path` with a caller-chosen `root_uuid`. Tests also use it to
	/// reopen the SAME DB and assert state survives a restart — `init_db` only wipes on a
	/// `user_version` mismatch, so a matching-version reopen preserves the data.
	pub(crate) fn new_on_path(path: &Path, root_uuid: Uuid) -> Self {
		let (event_sender, event_receiver) = tokio::sync::mpsc::channel(EVENT_SHED_CAP);
		let (control_sender, control_receiver) = tokio::sync::mpsc::unbounded_channel();
		drop(event_sender);
		drop(control_sender);
		let (msg_sender, msg_receiver) = tokio::sync::mpsc::channel(1);
		drop(msg_receiver);

		let mut state = Self {
			db: rusqlite::Connection::open(path).unwrap(),
			event_receiver,
			control_receiver: PeekableReceiver::new(control_receiver),
			msg_sender,
			shed: Arc::new(AtomicBool::new(false)),
			root_uuid,
			// Placeholder; `init_db` (called below) replaces it with the real account-root rowid.
			root_id: 0,
			sync_roots: whole_account_sync_roots(root_uuid),
			resync: None,
			resync_retry: None,
			read_tasks: None,
		};
		state.init_db().unwrap();
		state
	}
}

#[cfg(test)]
impl CacheState {
	/// Like [`new_in_memory`](Self::new_in_memory) but RETAINS the producer side (the event sender, the
	/// control sender, and the shed latch) so a test can flood the worker the way the real callback does
	/// and then drive the drain.
	pub(crate) fn new_in_memory_with_producer() -> (Self, TestProducer) {
		let root_uuid = Uuid::new_v4();
		let (event_sender, event_receiver) = tokio::sync::mpsc::channel(EVENT_SHED_CAP);
		let (control_sender, control_receiver) = tokio::sync::mpsc::unbounded_channel();
		let (msg_sender, msg_receiver) = tokio::sync::mpsc::channel(100);
		drop(msg_receiver);
		let shed = Arc::new(AtomicBool::new(false));

		let mut state = Self {
			db: rusqlite::Connection::open_in_memory().unwrap(),
			event_receiver,
			control_receiver: PeekableReceiver::new(control_receiver),
			msg_sender,
			shed: shed.clone(),
			root_uuid,
			// Placeholder; `init_db` (called below) replaces it with the real account-root rowid.
			root_id: 0,
			sync_roots: whole_account_sync_roots(root_uuid),
			resync: None,
			resync_retry: None,
			read_tasks: None,
		};
		state.init_db().unwrap();

		let producer = TestProducer {
			events: event_sender,
			control: control_sender,
			shed,
		};
		(state, producer)
	}

	pub(crate) fn set_test_sync_roots(&mut self, map: HashMap<Uuid, SyncRootCallback>) {
		self.sync_roots = map
			.into_iter()
			.map(|(uuid, callback)| (uuid, vec![(0, callback)]))
			.collect();
	}
}

/// Producer-side handles retained by [`CacheState::new_in_memory_with_producer`] for tests.
#[cfg(test)]
#[allow(dead_code)] // `control` kept for symmetry / future shutdown tests
pub(crate) struct TestProducer {
	pub(crate) events: tokio::sync::mpsc::Sender<CacheThreadEvent>,
	pub(crate) control: UnboundedSender<CacheControlMessage>,
	pub(crate) shed: Arc<AtomicBool>,
}

#[derive(Debug)]
pub(crate) enum ManualEvent {
	/// A directly-injected recursive directory listing (not from the socket). Upsert-only: it adds/
	/// refreshes the listed dirs and files but never deletes, and does not touch the drain watermark.
	ListDirRecursive(Vec<RemoteDirectory>, Vec<RemoteFile>),
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum CacheThreadEvent {
	Socket(CacheEventMaybeDecrypted<'static>),
	Manual(ManualEvent),
}

/// A per-sync-root notification callback. Invoked POST-COMMIT on the worker thread with a
/// borrowing iterator over the events applied to that root's subtree, each call wrapped in
/// `catch_unwind`. `Send` so it can be moved to the worker thread; the iterator borrows a local owned
/// `Vec`, never the rusqlite transaction.
pub type SyncRootCallback = Box<dyn Fn(&mut dyn Iterator<Item = &CacheEvent<'_>>) + Send + 'static>;

/// The live registrations for one sync root: `(registration_id, callback)` pairs. Multiple
/// [`SyncRootHandle`](crate::cache::SyncRootHandle)s may target the same uuid; each holds its own
/// registration, every callback is notified on dispatch, and the uuid stops being a sync root only
/// when its last registration is removed.
pub(crate) type RootRegistrations = Vec<(u64, SyncRootCallback)>;

/// Default whole-account sync: the root uuid gates all membership and its no-op callback lets tests
/// exercise the drain/apply/resync machinery without subscribing to notifications. Also compiled
/// under `bench-internals` (used by `new_on_path`).
#[cfg(any(test, feature = "bench-internals"))]
fn whole_account_sync_roots(account_root: Uuid) -> HashMap<Uuid, RootRegistrations> {
	let mut sync_roots: HashMap<Uuid, RootRegistrations> = HashMap::new();
	sync_roots.insert(account_root, vec![(0, Box::new(|_| {}))]);
	sync_roots
}

/// Ack for [`CacheControlMessage::AddSyncRoot`]: `Ok(())` once the registration is live
/// (validation passed), `Err` if the uuid was rejected.
pub(crate) type AddSyncRootAck = tokio::sync::oneshot::Sender<Result<(), Box<CacheError>>>;
/// Ack for [`CacheControlMessage::RemoveRegistration`]: `Ok(true)` iff the root's cached
/// subtree was evicted (the registration was the last one for its uuid and `evict` was requested).
pub(crate) type RemoveRegistrationAck = tokio::sync::oneshot::Sender<Result<bool, Box<CacheError>>>;

/// Control-plane messages delivered to the worker over the control channel — sync-root
/// reconfiguration that must be serialized against the drain. Distinct from the data-plane
/// [`CacheThreadEvent`] channel, which carries the events to apply.
pub(crate) enum CacheControlMessage {
	/// Register a `(registration_id, callback)` pair for `uuid`. A NEW uuid is validated
	/// (`get_dir`) and converged; an additional registration on an already-active uuid skips both.
	/// The ack fires after validation + insert, BEFORE the convergence resync.
	AddSyncRoot {
		uuid: Uuid,
		registration_id: u64,
		callback: SyncRootCallback,
		ack: AddSyncRootAck,
	},
	/// Remove one registration. When it was the last one for `uuid`, the uuid stops being a sync
	/// root, and if `evict` its cached subtree is deleted — protecting any still-active nested root.
	/// `ack` is `None` for the fire-and-forget [`SyncRootHandle`](crate::cache::SyncRootHandle) Drop
	/// path.
	RemoveRegistration {
		uuid: Uuid,
		registration_id: u64,
		evict: bool,
		ack: Option<RemoveRegistrationAck>,
	},
	Shutdown,
}

fn make_socket_event_callback(
	events: tokio::sync::mpsc::Sender<CacheThreadEvent>,
	shed: Arc<AtomicBool>,
) -> impl Fn(&DecryptedSocketEvent<'_>) + Send + 'static {
	move |event| {
		if let Some(event) = CacheEventMaybeDecrypted::from_decrypted_event(event) {
			route_thread_event(
				CacheThreadEvent::Socket(event.into_owned_cow()),
				&events,
				&shed,
			);
		}
	}
}

/// Route one event onto the worker's BOUNDED event channel — a full channel SHEDS the event
/// (drops it) and latches `shed`. This runs on the SDK socket runtime, which must not block or
/// touch the DB, so the non-blocking `try_send` is exactly right: the channel's capacity
/// ([`EVENT_SHED_CAP`]) bounds the worst-case PEAK memory atomically (no shadow counter to keep
/// in sync), and a shed is recovered by the resync the worker triggers when it sees the latch.
/// Because a single FIFO channel preserves order on its own, the capacity is the ONLY
/// backpressure — there is no second "overflow" channel or spill latch to manage.
fn route_thread_event(
	event: CacheThreadEvent,
	events: &tokio::sync::mpsc::Sender<CacheThreadEvent>,
	shed: &AtomicBool,
) {
	match events.try_send(event) {
		Ok(()) => {}
		Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
			// `event` intentionally dropped.
			if !shed.swap(true, Ordering::AcqRel) {
				tracing::warn!(
					"cache event channel reached its {EVENT_SHED_CAP}-event cap; shedding events under \
					 sustained load — a resync will recover the gap"
				);
			}
		}
		// The worker has shut down; nothing is listening.
		Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
	}
}

/// Sleep until the resync-retry deadline. The `None` case is unreachable: the select arm is
/// guarded by `is_some()`.
async fn resync_retry_sleep(deadline: Option<TimerInstant>) {
	if let Some(deadline) = deadline {
		sleep_until(deadline).await;
	}
}

/// Run one search read task against the worker's connection, catching panics: a poisoned query
/// must not kill the worker (native unwinds; on wasm panics abort regardless). The panicked
/// task's reply sender drops, surfacing as an error on the engine side.
fn serve_read_task(task: ReadTask, db: &rusqlite::Connection) {
	if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task(db))).is_err() {
		tracing::error!("a search read task panicked; the query errors on the engine side");
	}
}

/// The `None` receiver case is unreachable: the select arm is guarded by `is_some()`.
async fn recv_read_task(read_tasks: &mut Option<UnboundedReceiver<ReadTask>>) -> Option<ReadTask> {
	match read_tasks {
		Some(receiver) => receiver.recv().await,
		None => None,
	}
}

/// Body of the read-task `select!` arm, shared by the three select loops (the worker `run` loop and
/// `run_resync`'s lock-wait + listing loops). A FREE fn over the two fields it touches — not a
/// `&mut self` method — so it composes with the listing loop, where the `island` future holds a live
/// `&self.msg_sender` borrow across the same select (a whole-`self` borrow would conflict).
fn handle_read_task(
	read_task: Option<ReadTask>,
	read_tasks: &mut Option<UnboundedReceiver<ReadTask>>,
	db: &rusqlite::Connection,
) {
	match read_task {
		// Wasm search read: run against this connection, between drains. The query is a quick
		// indexed/scan SELECT — never a write. A poisoned query must not kill the worker (mirrors
		// `dispatch_batch`); its dropped reply sender surfaces as an error on the engine side.
		Some(task) => serve_read_task(task, db),
		// Every sender gone (no live searches and the shared state dropped): disable the arm —
		// polling a closed receiver in select would spin hot.
		None => *read_tasks = None,
	}
}

type InitResult = (
	CacheState,
	Box<dyn Fn(&DecryptedSocketEvent<'_>) + Send + 'static>,
	UnboundedSender<CacheControlMessage>,
	tokio::sync::mpsc::Sender<CacheThreadEvent>,
	UnboundedSender<ReadTask>,
);

impl CacheState {
	pub(crate) fn new(
		cache_path: &Path,
		root_uuid: Uuid,
		msg_sender: tokio::sync::mpsc::Sender<Vec<CacheMessage>>,
		client: Arc<Client>,
	) -> Result<InitResult, crate::Error> {
		let connection = rusqlite::Connection::open(cache_path).map_err(|e| {
			crate::Error::custom_with_source(
				ErrorKind::Internal,
				e,
				Some("Failed to open SQLite database"),
			)
		})?;

		let (event_sender, event_receiver) = tokio::sync::mpsc::channel(EVENT_SHED_CAP);
		let (control_sender, control_receiver) = tokio::sync::mpsc::unbounded_channel();
		let (read_task_sender, read_task_receiver) = tokio::sync::mpsc::unbounded_channel();
		let shed = Arc::new(AtomicBool::new(false));

		let mut cache_state = CacheState {
			db: connection,
			event_receiver,
			control_receiver: PeekableReceiver::new(control_receiver),
			msg_sender,
			shed: shed.clone(),
			root_uuid,
			// Placeholder; `init_db` (called below) replaces it with the real account-root rowid.
			root_id: 0,
			// Starts EMPTY (nothing cached); registrations arrive via `AddSyncRoot` control messages.
			sync_roots: HashMap::new(),
			resync: Some(ResyncDeps { client }),
			resync_retry: None,
			read_tasks: Some(read_task_receiver),
		};

		cache_state.init_db().map_err(|e| {
			crate::Error::custom_with_source(
				ErrorKind::Internal,
				e,
				Some("Failed to set up SQLite database"),
			)
		})?;

		// Register the search matcher on the worker's connection too: the wasm read path runs
		// search queries here (see `read_tasks`); on native this is a harmless extra (searches
		// use their own connection, which registers it itself).
		crate::cache::search::register_name_matches(&cache_state.db).map_err(|e| {
			crate::Error::custom_with_source(
				ErrorKind::Internal,
				e,
				Some("Failed to register the search matcher"),
			)
		})?;

		// The callback owns the event sender + the shed latch; a clone of `event_sender` is returned for
		// `SyncRootHandle` to inject Manual events (list_dir_recursive) onto the same channel.
		let callback = make_socket_event_callback(event_sender.clone(), shed);

		Ok((
			cache_state,
			Box::new(callback),
			control_sender,
			event_sender,
			read_task_sender,
		))
	}

	pub(crate) async fn run(mut self) {
		// Startup / app-resume recovery, in order:
		// 1. Apply anything a prior session persisted to `events` but did not drain (e.g. an abrupt
		//    close) so the watermark reflects it BEFORE the gap-check — this lets a clean local catch-up
		//    avoid a needless network resync.
		// 2. Catch up on changes that landed while the cache was entirely offline (a durably-flagged
		//    hole, or the remote drive id having advanced past our watermark).
		self.drain_pending(None);
		self.maybe_run_startup_resync().await;
		loop {
			// `biased` checks arms top-down: control (shutdown) first, then search read tasks
			// (quick SELECTs — prioritized over events so a sustained event flood can never
			// starve a search query; the engine's debounce bounds read volume, so reads cannot
			// starve events in return), then the single event channel, then the resync retry
			// timer. A lone FIFO event channel needs no spill latch or first-class overflow
			// arm — order is intrinsic, so the previous two-channel TOCTOU dance is gone.
			tokio::select! {
				biased;
				control_event = self.control_receiver.recv() => {
					// A pending control message is selected ahead of a non-empty event arm. `recv`
					// drains the peek buffer first, so a message a `run_resync` aborted on (peeked
					// but left queued) is applied HERE, before any event/read/retry work.
					// A `Shutdown` — or every control sender having been dropped WITHOUT one (the
					// `None` case: the last `SyncRootHandle` and the worker references are gone, or a
					// closed channel observed via the peek) — is the NORMAL clean-shutdown path.
					// Either way, drain everything currently buffered into the durable store before
					// exiting so it is not lost.
					let shutdown = match control_event {
						Some(first) => self.process_control_burst(first).await,
						None => true,
					};
					if shutdown {
						tracing::debug!("Cache shutting down; draining buffered events first...");
						self.drain_pending(None);
						return;
					}
				},
				read_task = recv_read_task(&mut self.read_tasks), if self.read_tasks.is_some() => {
					handle_read_task(read_task, &mut self.read_tasks, &self.db);
				},
				event = self.event_receiver.recv() => {
					let Some(event) = event else {
						tracing::debug!("Event channel closed, draining and shutting down cache...");
						self.drain_pending(None); // don't drop buffered events on disconnect
						return;
					};
					self.drain_pending(Some(event));
					// A drain that observed a hole/corrupt row/failed apply set needs_resync; heal it now.
					self.maybe_run_resync().await;
				},
				_ = resync_retry_sleep(self.resync_retry), if self.resync_retry.is_some() => {
					// A previous resync attempt did not converge (drive-lock contention or a
					// transient failure) and scheduled this one-shot re-attempt. The flag-gated
					// retry makes a stale fire harmless.
					self.resync_retry = None;
					self.maybe_run_resync().await;
				},
			}
		}
	}

	/// Sort one channel event for the drain: a Socket event becomes a `CacheEvent` to PERSIST (a
	/// `FrontierAdvance` becomes a `NoOp` marker so its id still joins the ordered frontier), while a
	/// Manual event is DEFERRED to apply AFTER the ordered socket drain (an inline Manual upsert
	/// must neither clobber, nor be clobbered by, socket events in the same drain). Pure classification —
	/// no DB access — so the whole drained burst can be persisted together in one transaction.
	fn classify_thread_event(
		event: CacheThreadEvent,
		to_persist: &mut Vec<CacheEvent<'static>>,
		deferred: &mut Vec<ManualEvent>,
	) {
		match event {
			CacheThreadEvent::Socket(CacheEventMaybeDecrypted::Decrypted(cache_event)) => {
				to_persist.push(cache_event);
			}
			CacheThreadEvent::Socket(CacheEventMaybeDecrypted::FrontierAdvance { id }) => {
				to_persist.push(CacheEvent {
					id: Some(id),
					event: CacheEventType::NoOp,
				});
			}
			CacheThreadEvent::Manual(manual_event) => deferred.push(manual_event),
		}
	}

	/// Persist the buffered channel into `events`, apply the durable store in order, then apply any
	/// deferred Manual (legacy `list_dir`) events. `first` is the event that woke the select loop
	/// (already removed from the channel), or `None` when draining on shutdown/disconnect.
	fn drain_pending(&mut self, first: Option<CacheThreadEvent>) {
		let mut deferred = Vec::new();
		let mut to_persist: Vec<CacheEvent<'static>> = Vec::new();
		if let Some(event) = first {
			Self::classify_thread_event(event, &mut to_persist, &mut deferred);
		}
		while let Ok(event) = self.event_receiver.try_recv() {
			Self::classify_thread_event(event, &mut to_persist, &mut deferred);
		}

		// Persist the WHOLE drained burst in ONE transaction (a single WAL commit/fsync instead of one
		// per event), so the worker clears the channel fast and its in-RAM residency stays small. On
		// failure the batch rolls back and we record a DURABLE resync: the events were already
		// removed from the channel, but a resync is a full subtree reconcile, so it recovers them; if the
		// resync-flag write ALSO fails, the startup gap-check (`remote > watermark`) is the backstop.
		if let Err(e) = self.insert_events_batch(&to_persist) {
			self.surface_one(e);
			self.mark_needs_resync_surfacing_errors();
		}

		// Apply the ordered, persisted socket events first. The drain records `needs_resync` ATOMICALLY
		// (inside `commit_drain_batch`) when it observes a hole, so there is no separate best-effort flag
		// write here; a hole detected here is healed by the subsequent `maybe_run_resync`
		// in `run`, or by the startup gap-check on the next boot.
		if let Err(e) = self.drain_persisted() {
			self.surface_one(e);
		}

		// if the socket router shed events under sustained over-rate (the channel hit its cap),
		// record a durable resync so the dropped events are recovered — the subsequent `maybe_run_resync`
		// in `run` (or the startup gap-check) re-lists and converges. Consume the latch once; any shed
		// that happens after this re-sets it for the next cycle, so the signal is never lost.
		if self.shed.swap(false, Ordering::AcqRel) {
			tracing::warn!("cache shed events under load; recording a resync to recover them");
			if let Err(e) = self.mark_needs_resync() {
				// The durable flag write failed — RE-ARM the shed latch so the next drain
				// retries the mark instead of silently losing the resync signal. (The startup gap-check
				// remains a backstop, but it only fires on the next boot.)
				self.shed.store(true, Ordering::Release);
				self.surface_one(e);
			}
		}

		for manual in deferred {
			if let Err(errors) = self.handle_manual_event(manual) {
				self.surface_errors(errors);
			}
		}
	}

	// TODO: distinguish a Full status channel (keep running) from a Closed one (main thread gone →
	// shut down). For now errors are surfaced best-effort.
	fn surface_errors(&self, errors: Vec<CacheError>) {
		let _ = self.msg_sender.try_send(vec![CacheMessage::Error(errors)]);
	}

	/// Surface a single boxed error — the common `surface_errors(vec![*e])` shape unboxed once.
	fn surface_one(&self, e: Box<CacheError>) {
		self.surface_errors(vec![*e]);
	}

	/// Apply one decoded event to the cache. Every branch must be idempotent (the drain may re-apply
	/// a hole-held / crash-replayed event — see [`CacheState::drain_persisted`]).
	fn apply_event(
		&mut self,
		event: CacheEventType<'_>,
		trust: EventTrust,
	) -> Result<(), Vec<CacheError>> {
		match event {
			CacheEventType::File(file_event) => self.handle_file_event(file_event, trust),
			CacheEventType::Dir(dir_event) => self.handle_dir_event(dir_event, trust),
			CacheEventType::Global(global_event) => self.handle_global_event(global_event),
			// Frontier-advance marker: no DB mutation; the drain still advances the watermark for it.
			CacheEventType::NoOp => Ok(()),
		}
	}

	/// Drain the durable `events` store: load each batch in apply order, apply events idempotently,
	/// advance the contiguous-prefix watermark, and delete consumed rows, until the store is empty.
	/// Returns `true` if a resync is needed — a hole was observed (a genuinely SDK-dropped id), an event
	/// failed to apply, or a row was quarantined as corrupt. This return is informational: the drain ALSO
	/// records the durable `needs_resync` flag atomically with the batch commit, and that flag — not this
	/// bool — is what the subsequent `maybe_run_resync` acts on.
	///
	/// INVARIANTS (D1 — there is deliberately NO single grand transaction; these are load-bearing):
	/// - Every apply dispatched via [`CacheState::apply_event`] MUST be idempotent. On a crash,
	///   un-deleted rows re-load and either re-apply (upsert / delete-of-missing are no-ops) or are
	///   deduped (`id <= watermark`).
	/// - Watermark-before-delete: the watermark advance and the row deletions commit together, AFTER
	///   the applies (see [`CacheState::commit_drain_batch`]) — so a crash never deletes an
	///   applied-but-un-watermarked event from `events` (which would lose it).
	/// - The single-step frontier advance (`id == frontier + 1`) is correct ONLY because
	///   `load_event_batch` returns `ORDER BY synthetic DESC, drive_message_id ASC, seq ASC`.
	fn drain_persisted(&mut self) -> Result<bool, Box<CacheError>> {
		// Read the persisted watermark ONCE. Within a single drain, ids arrive strictly ascending and
		// unique (the `ORDER BY drive_message_id ASC` + the partial unique index), so the dedup gate
		// can only ever fire for an id already applied in a PRIOR (possibly crashed) drain — i.e.
		// `id <= watermark`. A running in-batch high-water-mark would be redundant and would not
		// survive a crash anyway.
		let watermark = self.watermark()?;
		// The contiguous-prefix frontier, carried across batches. Once the prefix breaks — a hole, a
		// failed apply, or a corrupt row — it stays broken for the rest of this drain (`frontier_broken`)
		// so no later id can free-seed or jump the watermark past the lost id (a `None` frontier
		// would otherwise accept ANY later id as "contiguous").
		let mut cursor = DrainCursor {
			frontier: watermark,
			frontier_broken: false,
			resync_needed: false,
		};

		loop {
			let (batch, corrupt_seqs) = self.load_event_batch(BATCH_SIZE)?;
			if batch.is_empty() && corrupt_seqs.is_empty() {
				break;
			}
			// FAST PATH: the whole batch's applies land in ONE write transaction — one WAL
			// commit instead of one per event, which dominated large resync drains (a 166k-item
			// populate paid 166k commits). A failed apply rolls the entire batch back (nothing
			// surfaced, nothing quarantined yet) and the SLOW PATH re-runs the same rows —
			// still present in `events` — with per-event transactions, isolating and
			// quarantining the poison event with unchanged semantics. Apply failures are
			// corruption/disk-full class, so the slow path effectively never runs.
			//
			// Known residual: a root-deletion event applying INSIDE the bulk tx mutates the
			// in-memory `sync_roots` map and notifies the app pre-commit; if the batch then
			// rolls back, the re-run converges (the map mutation is idempotent and the DB
			// delete re-lands), matching the old per-event behavior when a delete failed.
			cursor = match self.apply_drain_batch(batch, corrupt_seqs, watermark, cursor, true)? {
				Some(cursor) => cursor,
				None => {
					let (batch, corrupt_seqs) = self.load_event_batch(BATCH_SIZE)?;
					self.apply_drain_batch(batch, corrupt_seqs, watermark, cursor, false)?
						.expect("the per-event drain path never aborts")
				}
			};
		}
		Ok(cursor.resync_needed)
	}

	/// Process ONE loaded drain batch: apply every event, then commit the watermark advance +
	/// consumed-row deletes, then dispatch. `bulk` wraps all of it in a single transaction and
	/// returns `Ok(None)` (after rolling back) on the first apply failure so the caller can
	/// re-run the same rows with `bulk = false`, where each apply commits alone (via
	/// `execute_chunked`'s own transactions) and a failure quarantines just that event.
	fn apply_drain_batch(
		&mut self,
		batch: Vec<PersistedEvent>,
		corrupt_seqs: Vec<i64>,
		watermark: Option<u64>,
		cursor: DrainCursor,
		bulk: bool,
	) -> Result<Option<DrainCursor>, Box<CacheError>> {
		let mut cursor = cursor;
		if bulk {
			self.db
				.execute_batch("BEGIN")
				.map_err(|e| Box::new(CacheError::db(e, "begin drain batch".to_string())))?;
		}
		if !corrupt_seqs.is_empty() {
			// A corrupt row is a lost event of (conservatively) unknown id — break the prefix and
			// force a resync rather than risk advancing the watermark past it.
			cursor.resync_needed = true;
			cursor.frontier_broken = true;
		}

		let frontier_before = cursor.frontier;
		let mut to_delete = corrupt_seqs; // corrupt rows are quarantined (deleted) with this batch
		// Per-root dispatch: collect (applied event, owning roots) for delivery AFTER commit.
		// The event is held in an `Arc` so fan-out to multiple owning roots shares one allocation
		// instead of deep-cloning the (string-heavy) payload per root.
		let mut dispatch_buffer: Vec<DispatchEntry> = Vec::new();

		for pe in batch {
			to_delete.push(pe.seq);
			// snapshot the pre-delete/pre-move parent BEFORE applying, and keep a clone of the
			// event for the post-commit callback (the original's payload is moved into `apply_event`).
			let pre_parent = self.pre_parent_snapshot(&pe.event.event);
			let event_for_dispatch = Arc::new(pe.event.clone());
			let apply_result = match pe.id {
				// id = None: a synthetic diff event. Resyncs now apply synthetics directly
				// (apply_synthetics_direct) — this arm only consumes LEGACY rows persisted by a
				// pre-direct-apply session. Idempotent (upsert / delete-of-missing), no
				// watermark interaction.
				None => self.apply_event(pe.event.event, EventTrust::Checked),
				// Already applied in a prior (possibly crashed) drain: skip, but still delete.
				Some(id) if watermark.is_some_and(|w| id <= w) => continue,
				Some(id) => {
					let result = self.apply_event(pe.event.event, EventTrust::Checked);
					if result.is_ok() {
						// Advance the frontier ONLY along an unbroken, gap-free run from `watermark`.
						// `checked_add` avoids a u64 overflow if a malformed id reached the store.
						if !cursor.frontier_broken
							&& cursor.frontier.is_none_or(|f| f.checked_add(1) == Some(id))
						{
							cursor.frontier = Some(id);
						} else {
							// a hole below `id` (an SDK-dropped event), or a
							// previously-broken prefix — never advance the watermark past it.
							cursor.frontier_broken = true;
							cursor.resync_needed = true;
						}
					}
					result
				}
			};
			match apply_result {
				Err(errors) => {
					// In bulk mode, abort the fast path: roll the whole batch back and report
					// NOTHING — the slow-path re-run owns the surfacing and quarantining
					// (reporting here too would double-count every error).
					if bulk {
						let _ = self.db.execute_batch("ROLLBACK");
						return Ok(None);
					}
					// Quarantine the failing event (already queued for deletion), surface the error,
					// break the prefix, and keep draining — do NOT abort the loop.
					self.surface_errors(errors);
					cursor.frontier_broken = true;
					cursor.resync_needed = true;
				}
				Ok(()) => {
					// Applied OK → resolve which sync roots this event touches and queue it for the
					// post-commit dispatch (using the post-apply state + the pre-snapshot parent).
					let owners = self.resolve_dispatch_owners(&event_for_dispatch, pre_parent);
					if !owners.is_empty() {
						dispatch_buffer.push((event_for_dispatch, owners));
					}
				}
			}
		}

		// Write the watermark only if the frontier actually moved this batch (and past the value
		// the drain started from). Watermark-before-delete holds in both modes: per-event mode
		// committed each apply already, and bulk mode commits applies + watermark + deletes
		// together, which trivially cannot delete an unapplied event.
		let advanced = match cursor.frontier {
			Some(f) if cursor.frontier != frontier_before && watermark.is_none_or(|w| f > w) => {
				Some(f)
			}
			_ => None,
		};
		// Record `needs_resync` ATOMICALLY with this batch's watermark advance + row deletes when
		// the contiguous frontier is broken, so the "resync needed" signal can never be lost to a
		// failed best-effort write after the hole's evidence is already deleted. The
		// flag write is idempotent across batches.
		let committed = self
			.commit_drain_batch(advanced, &to_delete, cursor.resync_needed)
			.and_then(|()| {
				if bulk {
					self.db
						.execute_batch("COMMIT")
						.map_err(|e| Box::new(CacheError::db(e, "commit drain batch".to_string())))
				} else {
					Ok(())
				}
			});
		if let Err(e) = committed {
			// Never leak an open transaction to the next select-loop arm.
			if bulk {
				let _ = self.db.execute_batch("ROLLBACK");
			}
			return Err(e);
		}

		self.dispatch_batch(dispatch_buffer);
		Ok(Some(cursor))
	}

	/// Resync core, independent of the async island so it is unit-testable. Given EACH sync
	/// root's freshly-listed subtree (as owned cacheables, paired with that root's anchor uuid) and the
	/// snapshot's drive message id, converge the cache: per root, stage its listing into `diff_incoming`
	/// and compute its diff synthetics (anchored at that root); then atomically persist ALL roots'
	/// synthetics with the advanced watermark + cleared `needs_resync`, and drain so they apply.
	///
	/// The whole thing is crash-safe by re-listing: synthetics are idempotent upserts /
	/// delete-of-missing applied directly from RAM (see [`apply_synthetics_direct`]), and the
	/// watermark + flag commit LAST — so a crash mid-apply leaves the old watermark and the
	/// durable flag (or the startup gap-check) to force a fresh listing. All roots' synthetics
	/// apply before the ONE watermark commit, so a crash cannot clear `needs_resync` for a
	/// half-converged root set. If the post-commit drain observes a *fresh* gap (a real
	/// buffered event above the snapshot id), `needs_resync` is re-set so the next worker cycle
	/// resyncs.
	///
	/// `mark_resync` keeps the durable `needs_resync` flag SET in the commit (instead of clearing
	/// it) — passed when the listing pass skipped at least one root transiently, so the skipped
	/// root gets a durable retry while the converged roots' progress still commits.
	fn apply_resync(
		&mut self,
		per_root: Vec<(
			Uuid,
			Vec<CacheableDir<'static>>,
			Vec<CacheableFile<'static>>,
		)>,
		remote_under_lock: u64,
		mark_resync: bool,
	) -> Result<(), Box<CacheError>> {
		let db_err =
			|e: rusqlite::Error, context: &str| Box::new(CacheError::db(e, context.to_string()));

		// Diff each sync root's listing against its cached subtree (anchored at that root) and ACCUMULATE
		// all roots' synthetics. They are committed together (below) so a crash mid-loop cannot clear
		// `needs_resync` after one root while leaving another un-converged.
		//
		// `per_root` contains only roots that listed successfully (deleted/transient roots were handled
		// before reaching here). An empty `per_root` means an empty config or an all-roots-deleted
		// resync — nothing to diff, so the empty-listing guard below must not fire for it.
		let had_listings = !per_root.is_empty();
		let mut per_root_synthetics: Vec<(Uuid, Vec<CacheEvent<'static>>)> = Vec::new();
		let mut any_listed = false;
		for (anchor, dirs, files) in per_root {
			any_listed |= !dirs.is_empty() || !files.is_empty();

			// uuid → cacheable maps: the payload source for this root's create/move/change synthetics.
			let dir_map: HashMap<Uuid, CacheableDir<'static>> =
				dirs.into_iter().map(|dir| (dir.uuid, dir)).collect();
			let file_map: HashMap<Uuid, CacheableFile<'static>> =
				files.into_iter().map(|file| (file.uuid, file)).collect();

			self.reset_diff_incoming()
				.map_err(|e| db_err(e, "resetting diff_incoming for resync"))?;
			self.insert_dirs_into_diff_incoming(dir_map.values())
				.map_err(|e| db_err(e, "staging listed dirs for resync"))?;
			self.insert_files_into_diff_incoming(file_map.values())
				.map_err(|e| db_err(e, "staging listed files for resync"))?;

			let synthetics = self
				.compute_resync_synthetics(anchor, &dir_map, &file_map)
				.map_err(|e| db_err(e, "computing resync diff"))?;
			per_root_synthetics.push((anchor, synthetics));
		}

		// A successful-but-empty listing converges to a full subtree deletion. A transient
		// failure surfaces as a SKIP in `run_resync` (so that root is absent from `per_root`, never
		// reaching here), so an all-empty result among the roots that DID list is either a genuinely-emptied
		// drive or a rare backend glitch. We proceed — the cache is rebuildable from the server on the next
		// resync — but log loudly when every root that listed came back EMPTY while the cache still holds
		// items. (Guarded by `had_listings` so an all-skipped resync, which deletes nothing, stays quiet.)
		if had_listings && !any_listed {
			// `type != 0` = all non-root items (type 0 = root, 1 = dir, 2 = file). The lone bit of inline
			// SQL in this module — kept here because it is a one-off guard-rail count, not part of the
			// reusable sql-layer surface.
			let cached_non_root: i64 = self
				.db
				.query_row("SELECT COUNT(*) FROM items WHERE type != 0", [], |row| {
					row.get(0)
				})
				.map_err(|e| db_err(e, "counting cached items for the empty-listing guard"))?;
			if cached_non_root > 0 {
				tracing::warn!(
					"resync: every sync root listed EMPTY but the cache holds {cached_non_root} \
					 item(s); converging will delete them. Proceeding — expected only if the drive was \
					 genuinely emptied."
				);
			}
		}

		// The per-anchor fast owner path is only sound when EVERY active root listed this
		// cycle: a transiently-skipped nested root would otherwise permanently miss the
		// notifications an ancestor's converging diff carries for its subtree (the retry diff
		// sees already-converged rows and emits nothing). With partial coverage, fall back to
		// exact per-event owner resolution across the board.
		let all_roots_listed = self
			.sync_roots
			.keys()
			.all(|root| per_root_synthetics.iter().any(|(anchor, _)| anchor == root));
		for (anchor, synthetics) in per_root_synthetics {
			self.apply_synthetics_direct(anchor, synthetics, all_roots_listed)?;
		}
		// The watermark jump + flag write commit LAST — strictly after every synthetic landed —
		// so a crash anywhere above leaves the old watermark and a detectable gap (the durable
		// flag for triggered resyncs; the startup gap-check otherwise), forcing a fresh listing.
		self.commit_resync_watermark(remote_under_lock, mark_resync)?;

		// Drain any REAL events persisted before or during the resync. If the drain observes a
		// FRESH hole (a buffered event above the snapshot id broke the frontier),
		// `commit_drain_batch` re-sets `needs_resync` atomically — even though the commit above
		// just cleared it — so the next worker cycle resyncs again.
		self.drain_persisted()?;

		// Best-effort: fold the apply burst's WAL back into the main DB now, while we are idle
		// anyway. A populate-scale apply leaves a WAL as large as the data it wrote; without an
		// explicit checkpoint, the engine's read snapshots can pin it (blocking the passive
		// auto-checkpoints) and every post-resync read pays WAL-lookup overhead on top of a cold
		// page cache. TRUNCATE also returns the disk space. (No-op without WAL, e.g. wasm.)
		if let Err(e) = self
			.db
			.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
		{
			tracing::debug!("post-resync wal_checkpoint failed (non-fatal): {e}");
		}
		Ok(())
	}

	/// Apply resync synthetics DIRECTLY from RAM — no durable events-store round-trip. The old
	/// path rkyv-serialized every synthetic into `events`, read them back in batches, applied,
	/// and deleted them again: pure overhead at populate scale, buying only crash-resume from
	/// the store — which the listing makes redundant, since synthetics are idempotent and the
	/// durable flag/startup gap-check re-derive them from a FRESH listing after a crash
	/// (recovery costs a re-download instead of a local replay; an accepted trade).
	///
	/// Applies run in [`crate::cache::sql::CHUNK_SIZE`]-sized transactions IN EMISSION ORDER —
	/// the diff's creates/moves → deletes → changes ordering is load-bearing (a descendant
	/// moving out of a same-resync-deleted dir must re-land before the cascade delete). Each
	/// chunk dispatches to the sync-root callbacks post-commit, like the drain. The caller
	/// commits the watermark AFTER all of it. The per-chunk work is delegated to
	/// [`apply_synthetic_chunk`](Self::apply_synthetic_chunk), which multi-row-batches the
	/// dominant simple-upsert case.
	fn apply_synthetics_direct(
		&mut self,
		anchor: Uuid,
		synthetics: Vec<CacheEvent<'static>>,
		all_roots_listed: bool,
	) -> Result<(), Box<CacheError>> {
		let db_err = |e: rusqlite::Error, what: &str| Box::new(CacheError::db(e, what.to_string()));
		// Dispatch owners, resolved ONCE per anchor instead of one ancestry CTE per event:
		// every New/Changed/MetadataChanged synthetic's target sits inside the anchor's
		// subtree, and nested roots are covered by their OWN per-root diff emitting the same
		// items. Only Move/Removed/Archived need the exact per-event resolution — their
		// OLD-position roots (a nested root the item left, a deleted nested root itself) are
		// not derivable from the anchor — and those shapes are rare outside steady state.
		let mut anchor_owners = Vec::new();
		self.extend_owning_roots(anchor, &mut anchor_owners);
		let mut events = synthetics.into_iter().peekable();
		while events.peek().is_some() {
			self.db
				.execute_batch("BEGIN")
				.map_err(|e| db_err(e, "begin synthetic apply chunk"))?;
			match self.apply_synthetic_chunk(&mut events, &anchor_owners, all_roots_listed) {
				Ok(dispatch_buffer) => {
					self.db.execute_batch("COMMIT").map_err(|e| {
						let _ = self.db.execute_batch("ROLLBACK");
						db_err(e, "commit synthetic apply chunk")
					})?;
					self.dispatch_batch(dispatch_buffer);
				}
				Err(mut errors) => {
					// Synthetics are our own diff output — a failure is corruption/disk-full
					// class. Roll the chunk back, keep the durable flag SET so the retry
					// re-lists, surface the rest, and abort with the first error (the caller
					// must NOT advance the watermark).
					let _ = self.db.execute_batch("ROLLBACK");
					let first = if errors.is_empty() {
						CacheError::db(
							rusqlite::Error::QueryReturnedNoRows,
							"synthetic apply failed without error detail".to_string(),
						)
					} else {
						errors.swap_remove(0)
					};
					self.surface_errors(errors);
					self.mark_needs_resync_surfacing_errors();
					return Err(Box::new(first));
				}
			}
		}
		Ok(())
	}

	/// Apply up to [`CHUNK_SIZE`](crate::cache::sql::CHUNK_SIZE) synthetics from `events` inside the
	/// caller's already-open transaction, returning the post-commit dispatch buffer.
	///
	/// The dominant case at populate scale — `New`/`Changed` under a fully-listed resync, whose
	/// dispatch owners are exactly `anchor_owners` — is accumulated and applied as multi-row batches
	/// (one prepared statement per ~[`MULTI_ROW_CHUNK`](crate::cache::sql) rows instead of one per
	/// event). Everything else (moves, deletes, or any event needing exact per-event owner
	/// resolution) flushes the pending batch FIRST — preserving the diff's load-bearing creates/moves
	/// → deletes → changes ordering — then applies one event at a time exactly as the old path did.
	/// Dispatch entries are pushed in strict emission order regardless of batching, so the
	/// post-commit dispatch sequence is unchanged.
	///
	/// On the first apply failure, returns the collected errors so the caller can roll the chunk
	/// back; the per-batch precondition that a batch holds no duplicate uuid is met because synthetic
	/// uuids are unique within a resync.
	fn apply_synthetic_chunk(
		&mut self,
		events: &mut std::iter::Peekable<std::vec::IntoIter<CacheEvent<'static>>>,
		anchor_owners: &[Uuid],
		all_roots_listed: bool,
	) -> Result<Vec<DispatchEntry>, Vec<CacheError>> {
		let mut dispatch_buffer: Vec<DispatchEntry> = Vec::new();
		let mut file_batch: Vec<CacheableFile<'static>> = Vec::new();
		let mut dir_batch: Vec<CacheableDir<'static>> = Vec::new();
		let mut processed = 0;
		while processed < crate::cache::sql::CHUNK_SIZE {
			let Some(event) = events.next() else { break };
			processed += 1;
			let needs_exact_owners = !all_roots_listed
				|| matches!(
					&event.event,
					CacheEventType::File(
						FileEvent::Move(_) | FileEvent::Removed(_) | FileEvent::Archived(_)
					) | CacheEventType::Dir(DirEvent::Move(_) | DirEvent::Removed(_))
				);
			// Fast path: a simple upsert whose dispatch owners are exactly the anchor's. It needs no
			// pre-apply parent snapshot or post-apply owner walk, so it can be deferred into a
			// multi-row batch without changing what (or in what order) gets dispatched.
			if !needs_exact_owners
				&& matches!(
					&event.event,
					CacheEventType::File(FileEvent::New(_) | FileEvent::Changed(_))
						| CacheEventType::Dir(DirEvent::New(_) | DirEvent::Changed(_))
				) {
				if !anchor_owners.is_empty() {
					dispatch_buffer.push((Arc::new(event.clone()), anchor_owners.to_vec()));
				}
				match event.event {
					CacheEventType::File(FileEvent::New(file) | FileEvent::Changed(file)) => {
						file_batch.push(file)
					}
					CacheEventType::Dir(DirEvent::New(dir) | DirEvent::Changed(dir)) => {
						dir_batch.push(dir)
					}
					_ => unreachable!("guarded by the matches! above"),
				}
				continue;
			}

			// Anything else: flush the pending upsert batch FIRST so accumulated creates/moves land
			// before this event (the diff orders deletes after the creates/moves they may cascade),
			// then apply it one event at a time with the exact owner resolution the old path used.
			self.flush_synthetic_batches(&mut file_batch, &mut dir_batch)?;
			let pre_parent = if needs_exact_owners {
				self.pre_parent_snapshot(&event.event)
			} else {
				None
			};
			let event_for_dispatch = Arc::new(event.clone());
			self.apply_event(event.event, EventTrust::TrustedSynthetic)?;
			let owners = if needs_exact_owners {
				self.resolve_dispatch_owners(&event_for_dispatch, pre_parent)
			} else {
				anchor_owners.to_vec()
			};
			if !owners.is_empty() {
				dispatch_buffer.push((event_for_dispatch, owners));
			}
		}
		// Flush whatever the chunk ended on.
		self.flush_synthetic_batches(&mut file_batch, &mut dir_batch)?;
		Ok(dispatch_buffer)
	}

	/// Flush the accumulated multi-row upsert batches, draining both. Dirs go before files, but the
	/// order is immaterial: `items.parent` has no self-FK, and each `files`/`dirs` row derives its
	/// own `items.id` independently. A failure maps to a one-element error vec so the chunk caller
	/// rolls back.
	fn flush_synthetic_batches(
		&mut self,
		files: &mut Vec<CacheableFile<'static>>,
		dirs: &mut Vec<CacheableDir<'static>>,
	) -> Result<(), Vec<CacheError>> {
		if !dirs.is_empty() {
			self.upsert_dirs(dirs.drain(..))
				.map_err(|e| vec![CacheError::db(e, "bulk upsert synthetic dirs".to_string())])?;
		}
		if !files.is_empty() {
			self.upsert_files(files.drain(..))
				.map_err(|e| vec![CacheError::db(e, "bulk upsert synthetic files".to_string())])?;
		}
		Ok(())
	}

	/// Run the write-locked resync, surfacing any error to the main thread.
	async fn run_resync_surfacing_errors(&mut self) {
		if let Err(e) = self.run_resync().await {
			self.surface_one(e);
		}
	}

	/// Record the durable `needs_resync` flag; if even that write fails, surface it best-effort (the
	/// next session's startup gap-check is the backstop). The bare `if let Err(e) = mark_needs_resync()
	/// { surface_errors(vec![*e]) }` shape recurs across the drain/resync paths; this names it.
	fn mark_needs_resync_surfacing_errors(&self) {
		if let Err(e) = self.mark_needs_resync() {
			self.surface_one(e);
		}
	}

	/// Abandon the in-flight (non-converged) resync attempt cleanly: durably flag `needs_resync`,
	/// tell progress consumers the attempt finished without converging, and — only when `arm_retry`
	/// — schedule a self-retry so a quiet account re-attempts. No lock is held and nothing is
	/// committed at any caller, so this drops no work. `arm_retry` is `false` on the abort paths
	/// where a queued control message (Shutdown / Add / Remove) will itself re-drive the loop, so a
	/// timer would be redundant; it leaves `resync_retry` untouched in that case.
	fn abort_resync_unconverged(&mut self, arm_retry: bool) {
		self.mark_needs_resync_surfacing_errors();
		Self::send_resync_progress(
			&self.msg_sender,
			ResyncProgress::Finished { converged: false },
		);
		if arm_retry {
			self.arm_resync_retry();
		}
	}

	/// Arm a one-shot resync re-attempt after [`RESYNC_RETRY_INTERVAL`].
	fn arm_resync_retry(&mut self) {
		self.resync_retry = Some(TimerInstant::now() + RESYNC_RETRY_INTERVAL);
	}

	/// Schedule a resync re-attempt iff this attempt did NOT converge, else clear any pending one. A
	/// non-converged attempt must self-arm: on a QUIET account (no events to trigger the next drain) a
	/// pending resync would otherwise sit until some unrelated event — e.g. a freshly added root
	/// staying unpopulated indefinitely.
	fn schedule_retry_if_unconverged(&mut self, converged: bool) {
		if converged {
			self.resync_retry = None;
		} else {
			self.arm_resync_retry();
		}
	}

	/// Process `first` plus every control message already queued behind it, then run AT MOST ONE
	/// convergence resync if the burst added any new sync root — so a multi-root startup (N
	/// `add_sync_root` calls in quick succession) converges with a single listing pass instead of N.
	/// Returns `true` if a `Shutdown` was encountered (the caller drains and exits; messages queued
	/// behind the Shutdown are intentionally not processed).
	async fn process_control_burst(&mut self, first: CacheControlMessage) -> bool {
		let mut added_new_root = false;
		let mut message = Some(first);
		while let Some(msg) = message {
			match msg {
				CacheControlMessage::Shutdown => return true,
				CacheControlMessage::AddSyncRoot {
					uuid,
					registration_id,
					callback,
					ack,
				} => {
					added_new_root |= self
						.handle_add_sync_root(uuid, registration_id, callback, ack)
						.await;
				}
				CacheControlMessage::RemoveRegistration {
					uuid,
					registration_id,
					evict,
					ack,
				} => {
					self.handle_remove_registration(uuid, registration_id, evict, ack)
						.await;
				}
			}
			message = self.control_receiver.try_recv();
		}
		if added_new_root {
			// Durably schedule the convergence FIRST: the adds were already acked Ok, and a
			// transient resync failure returns Ok after only a log — without the flag the new
			// root(s) would silently stay unpopulated for the session (live events for them are
			// membership-gated out while the watermark keeps advancing, so no hole ever re-flags).
			// A fully successful resync clears the flag atomically in the watermark commit.
			self.mark_needs_resync_surfacing_errors();
			// Resync ALL roots so the new root(s) are populated AND the watermark stays accurate for
			// every root. Resyncing only the new roots could advance the watermark past a pending gap
			// in an existing root and clear `needs_resync`, masking it. Redundant listings of
			// already-current roots are accepted in v1; a per-root-bookmark skip is a future
			// optimization.
			tracing::debug!("sync root(s) added; resyncing to populate");
			self.run_resync_surfacing_errors().await;
		}
		false
	}

	/// Handle `AddSyncRoot`: register `(registration_id, callback)` for `uuid`, validating the
	/// uuid first when it is not already an active sync root. Returns whether `uuid` is NEWLY active
	/// (the caller runs the convergence resync once per control burst).
	///
	/// VALIDATION: a subdir `uuid` is checked with `get_dir` BEFORE it is inserted, so a bad key
	/// can never enter `sync_roots` and make every later resync's `get_dir` fail (which would
	/// re-trigger a resync on each event: a tight loop). A DEFINITIVE not-found rejects with
	/// [`CacheError::InvalidSyncRoot`] and ALSO wipes any stale subtree a prior session cached under
	/// the uuid — re-adding after a restart is the only path that learns about an offline deletion,
	/// and without the wipe those rows would be stranded forever (membership-gated out of live
	/// events, anchored by no resync diff). Any other validation failure (network/server) rejects
	/// with [`CacheError::SyncRootUnavailable`]; the app retries the same uuid. The account root
	/// needs no check — it always exists and resyncs via `client.root()`, not `get_dir`. (The
	/// validating `get_dir` is then repeated by `run_resync`'s listing; the extra round-trip is
	/// accepted since `AddSyncRoot` is rare.) An ALREADY-ACTIVE uuid skips validation and the resync —
	/// its existence is established and its subtree is already converged; the new callback simply
	/// joins the root's registrations. A COVERED uuid (cached, with its ancestry reaching an active
	/// root) likewise skips both — see the fast path comment in the body.
	async fn handle_add_sync_root(
		&mut self,
		uuid: Uuid,
		registration_id: u64,
		callback: SyncRootCallback,
		ack: AddSyncRootAck,
	) -> bool {
		if let Some(registrations) = self.sync_roots.get_mut(&uuid) {
			registrations.push((registration_id, callback));
			let _ = ack.send(Ok(()));
			return false;
		}
		// Covered-add fast path: a uuid whose CACHED ancestry reaches a currently-active sync
		// root needs neither validation nor a convergence resync. Coverage means the subtree's
		// events were flowing the whole time, so cached existence IS event-stream truth (a
		// server-side deletion would have arrived as an event and removed the rows), and any gap
		// is already durably flagged — the scheduled resync lists ALL roots including this one,
		// so skipping the immediate resync masks nothing. An uncached uuid has no ancestry rows
		// and falls through; a DB error here also just falls through to the (always-correct)
		// validate-and-converge slow path.
		if uuid != self.root_uuid
			&& matches!(self.in_any_sync_root(uuid, &self.sync_roots), Ok(true))
		{
			tracing::debug!(
				"AddSyncRoot {uuid}: covered by an active sync root; registering directly"
			);
			self.sync_roots
				.insert(uuid, vec![(registration_id, callback)]);
			let _ = ack.send(Ok(()));
			return false;
		}
		if uuid != self.root_uuid
			&& let Some(deps) = self.resync.clone()
			&& let Err(e) = deps.client.get_dir(uuid).await
		{
			let error = if matches!(
				e.kind(),
				ErrorKind::FolderNotFound | ErrorKind::FileNotFound
			) {
				tracing::warn!("AddSyncRoot {uuid}: directory no longer exists ({e}); rejecting");
				// Definitively gone: wipe the stale cached subtree from any prior session
				// (the cascade trigger recurses). The cascade also wipes any still-registered
				// NESTED root, so snapshot + drop + notify those first, exactly like the socket
				// `DirEvent::Removed` arm. A delete of an uncached uuid is an idempotent no-op.
				let dead_roots = self.sync_roots_deleted_by(uuid);
				match self.delete_items(once(uuid)) {
					Ok(()) => self.handle_deleted_sync_roots(dead_roots),
					Err(del_e) => self.surface_errors(db_err_vec(
						del_e,
						format!("wiping the stale subtree of deleted sync root {uuid}"),
					)),
				}
				CacheError::invalid_sync_root(uuid, e.to_string())
			} else {
				tracing::warn!(
					"AddSyncRoot {uuid}: validation failed transiently ({e}); rejecting"
				);
				CacheError::sync_root_unavailable(uuid, e.to_string())
			};
			let _ = ack.send(Err(Box::new(error)));
			return false;
		}
		self.sync_roots
			.insert(uuid, vec![(registration_id, callback)]);
		let _ = ack.send(Ok(()));
		true
	}

	/// Handle `RemoveRegistration`: drop one `(uuid, registration_id)` registration and, when the
	/// ack is absent (the fire-and-forget Drop path), surface any eviction error to the status
	/// callback instead.
	async fn handle_remove_registration(
		&mut self,
		uuid: Uuid,
		registration_id: u64,
		evict: bool,
		ack: Option<RemoveRegistrationAck>,
	) {
		let result = self.remove_registration(uuid, registration_id, evict).await;
		match (ack, result) {
			(Some(ack), result) => {
				let _ = ack.send(result);
			}
			(None, Err(e)) => self.surface_one(e),
			(None, Ok(_)) => {}
		}
	}

	/// Drop one `(uuid, registration_id)` registration. The uuid stays an active sync root while
	/// other registrations remain — `evict` is then SKIPPED too (deleting the subtree out from under a
	/// still-active root would fight the membership gate). Removing the last registration stops
	/// syncing `uuid`, and with `evict` also deletes its cached subtree. An unknown uuid/registration
	/// (e.g. the root was already dropped server-side) is a harmless no-op — a stale handle's Drop
	/// must never error. Returns `Ok(true)` iff the subtree was evicted.
	async fn remove_registration(
		&mut self,
		uuid: Uuid,
		registration_id: u64,
		evict: bool,
	) -> Result<bool, Box<CacheError>> {
		let Some(registrations) = self.sync_roots.get_mut(&uuid) else {
			tracing::debug!("RemoveRegistration: {uuid} is not an active sync root; ignoring");
			return Ok(false);
		};
		let before = registrations.len();
		registrations.retain(|(id, _)| *id != registration_id);
		if registrations.len() == before {
			tracing::debug!(
				"RemoveRegistration: registration {registration_id} not found for sync root {uuid}; ignoring"
			);
			return Ok(false);
		}
		if !registrations.is_empty() {
			return Ok(false);
		}
		self.sync_roots.remove(&uuid);
		if !evict {
			return Ok(false);
		}
		self.evict_removed_root(uuid).await?;
		Ok(true)
	}

	/// Delete the cached subtree of a JUST-removed sync root (`uuid` must already be out of
	/// `sync_roots`), protecting any still-active nested root.
	async fn evict_removed_root(&mut self, uuid: Uuid) -> Result<(), Box<CacheError>> {
		let db_err =
			|e: rusqlite::Error, context: &str| Box::new(CacheError::db(e, context.to_string()));
		if uuid == self.root_uuid {
			// Evicting the account root = wipe every non-root item (the account-root node stays).
			self.delete_all_non_root()
				.map_err(|e| db_err(e, "evicting the account-root subtree"))?;
			// the flat `delete_all_non_root` also wiped any STILL-ACTIVE subdir sync
			// root's subtree (it cannot protect them — there is no per-root anchor for a flat delete). The
			// local map mutation is not an id event, so without an explicit resync those roots would stay
			// empty (their ancestry gone → the membership gate drops their live events, and dispatch
			// can't resolve them). Mark durably FIRST — exactly like the `DeleteAll` arm, whose wipe
			// creates the same ancestry-less state — so a transiently-failing re-convergence is
			// retried by a later drain instead of stranding the survivors empty; then re-converge.
			if !self.sync_roots.is_empty() {
				self.mark_needs_resync()?;
				self.run_resync_surfacing_errors().await;
			}
			return Ok(());
		}
		let protected: Vec<Uuid> = self.sync_roots.keys().copied().collect();
		self.evict_sync_root_subtree(uuid, &protected)
			.map_err(|e| db_err(e, &format!("evicting sync root {uuid}")))
	}

	/// The configured sync roots that a deletion of `deleted` removes: `deleted` itself if it
	/// is a sync root, plus any sync root NESTED under it (the `cascade_on_delete` trigger wipes the
	/// whole subtree). MUST be called BEFORE the delete, while the descendants' ancestry rows are still
	/// present. The account root is excluded from the nested scan — it can only be "deleted by" a removal
	/// of itself (the `== deleted` arm), never via an ancestor, and it is never removed server-side — so
	/// the common whole-account config issues no ancestry query here.
	fn sync_roots_deleted_by(&self, deleted: Uuid) -> Vec<Uuid> {
		self.sync_roots
			.keys()
			.copied()
			.filter(|&root| {
				root == deleted
					|| (root != self.root_uuid
						&& self
							.ancestors_of(root)
							.map(|ancestry| ancestry.contains(&deleted))
							.unwrap_or(false))
			})
			.collect()
	}

	/// Drop server-deleted sync roots from the active set and notify the app. After this the
	/// roots no longer gate membership or receive dispatch; the app must re-issue `add_sync_root` to
	/// resume syncing them (e.g. if the directory is restored from trash).
	fn handle_deleted_sync_roots(&mut self, deleted_roots: Vec<Uuid>) {
		if deleted_roots.is_empty() {
			return;
		}
		for root in &deleted_roots {
			self.sync_roots.remove(root);
			tracing::warn!("sync root {root} was deleted server-side; dropped from the active set");
		}
		// The app MUST learn these roots are gone (it has to re-issue `add_sync_root` to resume them —
		// see `CacheMessage::SyncRootsDeleted`). If the status channel is full the notification is lost
		// and the roots stay silently unsynced until restart, so at least make that visible in the log.
		if self
			.msg_sender
			.try_send(vec![CacheMessage::SyncRootsDeleted(deleted_roots.clone())])
			.is_err()
		{
			tracing::error!(
				"status channel full; dropped SyncRootsDeleted notification for {deleted_roots:?} \
				 — app will not resume these roots until restart"
			);
		}
	}

	/// The PRE-apply parent of a delete/move target. A `Removed`/`Archived` erases the item and
	/// a `Move` rewrites its parent, so the dispatcher must read the parent BEFORE applying to resolve
	/// which sync root the event left. `None` if the target is not (or no longer) cached.
	fn pre_parent_snapshot(&self, event: &CacheEventType<'_>) -> Option<Uuid> {
		let target = dispatch_presnapshot_target(event)?;
		let parent: rusqlite::Result<Option<Uuid>> = self.db.query_row(
			"SELECT parent FROM items WHERE uuid = ?1",
			rusqlite::params![target],
			|row| row.get(0),
		);
		parent.ok().flatten()
	}

	fn extend_owning_roots(&self, uuid: Uuid, owners: &mut Vec<Uuid>) {
		if let Ok(roots) = self.owning_sync_roots(uuid, &self.sync_roots) {
			owners.extend(roots);
		}
	}

	/// Which sync roots should be notified of `event`. A create/change/metadata event is
	/// resolved from the target's post-apply position; a `Move` notifies BOTH the old (pre-move) and new
	/// enclosing roots; a delete notifies the old enclosing root (from the pre-snapshot) plus the target
	/// itself if it was a sync root; `DeleteAll` notifies every root (account-global).
	fn resolve_dispatch_owners(
		&self,
		event: &CacheEvent<'_>,
		pre_parent: Option<Uuid>,
	) -> Vec<Uuid> {
		let mut owners = Vec::new();
		match &event.event {
			CacheEventType::File(file_event) => match file_event {
				FileEvent::New(f) | FileEvent::Changed(f) => {
					self.extend_owning_roots(f.uuid, &mut owners)
				}
				FileEvent::Move(f) => {
					self.extend_owning_roots(f.uuid, &mut owners);
					if let Some(parent) = pre_parent {
						self.extend_owning_roots(parent, &mut owners);
					}
				}
				FileEvent::Removed(uuid) | FileEvent::Archived(uuid) => {
					if self.sync_roots.contains_key(uuid) {
						owners.push(*uuid);
					}
					if let Some(parent) = pre_parent {
						self.extend_owning_roots(parent, &mut owners);
					}
				}
				FileEvent::MetadataChanged { uuid, .. } => {
					self.extend_owning_roots(*uuid, &mut owners)
				}
			},
			CacheEventType::Dir(dir_event) => match dir_event {
				DirEvent::New(d) | DirEvent::Changed(d) => {
					self.extend_owning_roots(d.uuid, &mut owners)
				}
				DirEvent::Move(d) => {
					self.extend_owning_roots(d.uuid, &mut owners);
					if let Some(parent) = pre_parent {
						self.extend_owning_roots(parent, &mut owners);
					}
				}
				DirEvent::Removed(uuid) => {
					if self.sync_roots.contains_key(uuid) {
						owners.push(*uuid);
					}
					if let Some(parent) = pre_parent {
						self.extend_owning_roots(parent, &mut owners);
					}
				}
				DirEvent::MetadataChanged { uuid, .. } | DirEvent::ColorChanged { uuid, .. } => {
					self.extend_owning_roots(*uuid, &mut owners)
				}
			},
			// Account-global wipe → every sync root is affected.
			CacheEventType::Global(GlobalEvent::DeleteAll) => {
				owners.extend(self.sync_roots.keys().copied());
			}
			// Cache no-ops (trash/version) and frontier markers notify nobody.
			CacheEventType::Global(_) | CacheEventType::NoOp => {}
		}
		owners.sort();
		owners.dedup();
		owners
	}

	/// Deliver the batch's applied events to each owning sync root's callback, POST-COMMIT
	/// on the worker thread over an owned `Vec`. Each callback is wrapped in `catch_unwind` so a panic
	/// in one root's external code is surfaced as a status error and never stalls other roots or the
	/// drain. (No-op when there is nothing to dispatch.)
	fn dispatch_batch(&self, dispatch: Vec<DispatchEntry>) {
		if dispatch.is_empty() {
			return;
		}
		let mut per_root: HashMap<Uuid, Vec<Arc<CacheEvent<'static>>>> = HashMap::new();
		for (event, owners) in dispatch {
			for owner in owners {
				// Cheap `Arc` clone (refcount bump), not a deep payload clone.
				per_root.entry(owner).or_default().push(event.clone());
			}
		}
		for (root, events) in per_root {
			// A root deleted during this same drain has already been removed from `sync_roots` by
			// `handle_deleted_sync_roots` (which notified the app via `SyncRootsDeleted`). Skipping its
			// now-absent callbacks here is correct — those events belonged to a root the app knows is gone.
			let Some(registrations) = self.sync_roots.get(&root) else {
				continue;
			};
			// Every registration on the root gets the full batch, each under its own `catch_unwind`,
			// so one handle's panicking callback never starves a sibling registration.
			for (registration_id, callback) in registrations {
				let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
					callback(&mut events.iter().map(|event| &**event));
				}));
				if let Err(panic) = result {
					let message = panic_message(&panic);
					tracing::error!(
						"sync-root {root} callback (registration {registration_id}) panicked: {message}"
					);
					let _ = self.msg_sender.try_send(vec![CacheMessage::Error(vec![
						CacheError::sync_root_callback_panic(format!(
							"sync root {root}: {message}"
						)),
					])]);
				}
			}
		}
	}

	/// Per-drain check: a hole/corrupt-row/failed-apply observed during LIVE operation set
	/// `needs_resync`; heal it now. A no-op when the flag is clear, so the common path costs one cheap
	/// `cache_meta` read per drain.
	async fn maybe_run_resync(&mut self) {
		match self.needs_resync() {
			Ok(true) => {
				tracing::debug!("resync pending (hole flagged during live operation); resyncing");
				self.run_resync_surfacing_errors().await;
			}
			Ok(false) => {}
			Err(e) => self.surface_one(e),
		}
	}

	/// Whether a STARTUP resync is warranted, given the latest remote drive message id.
	///
	/// Resync iff EITHER a hole was durably flagged in a prior session (`needs_resync`), OR the remote
	/// drive counter has advanced past our watermark — i.e. events landed on the drive while this cache
	/// was offline (app closed / socket down) and the socket will never redeliver them. When the remote
	/// id EQUALS the watermark, the cache is fully caught up, so we skip: this is the "do not resync if
	/// the drive id has not increased" gate. `get_last_event_ids().drive` is the same strictly-monotonic
	/// counter as the watermark's `drive_message_id`. A `None` watermark (nothing applied yet) compares
	/// as `0`, so a fresh cache against a non-empty drive resyncs to populate.
	fn startup_should_resync(&self, remote_drive_id: u64) -> Result<bool, Box<CacheError>> {
		Ok(self.needs_resync()? || remote_drive_id > self.watermark()?.unwrap_or(0))
	}

	/// Startup / app-resume check (called once when the worker boots). Reads the remote drive id and
	/// resyncs only if [`startup_should_resync`](Self::startup_should_resync) says so — catching up any
	/// changes that landed while the cache was offline, while skipping a needless full listing when
	/// nothing changed. Falls back to the durable-flag-only check if there is no client (unit tests) or
	/// the remote read fails (the live socket will still surface any later hole).
	async fn maybe_run_startup_resync(&mut self) {
		let Some(deps) = self.resync.clone() else {
			self.maybe_run_resync().await;
			return;
		};
		let remote = match deps.client.get_last_event_ids().await {
			Ok(ids) => ids.drive,
			Err(e) => {
				tracing::warn!(
					"startup gap-check: could not read the remote drive id ({e}); falling back to \
					 the durable resync flag"
				);
				self.maybe_run_resync().await;
				return;
			}
		};
		match self.startup_should_resync(remote) {
			Ok(true) => {
				tracing::debug!(
					"startup resync: remote drive id {remote} is ahead of watermark {:?} (or a hole \
					 was flagged); catching up",
					self.watermark().ok().flatten()
				);
				self.run_resync_surfacing_errors().await;
			}
			Ok(false) => tracing::debug!(
				"startup: cache is up to date (remote drive id {remote} == watermark); no resync"
			),
			// FAIL OPEN: if the decision itself errors (a `cache_meta` read failed), resync
			// rather than skip — a needless full listing is far cheaper than silently missing a gap.
			Err(e) => {
				tracing::warn!(
					"startup gap-check failed to read cache state; resyncing to be safe"
				);
				self.surface_one(e);
				self.run_resync_surfacing_errors().await;
			}
		}
	}

	/// The write-locked resync: list every sync root's subtree under the drive lock, read the
	/// snapshot drive message id under the SAME lock (so listing + watermark are consistent), then
	/// converge the cache via [`apply_resync`].
	///
	/// The drive lock is acquired with a SINGLE patient acquisition
	/// ([`RESYNC_LOCK_MAX_SLEEP`]/[`RESYNC_LOCK_PATIENT_ATTEMPTS`]) that drains events and serves
	/// read queries CONCURRENTLY while it waits (so event application is never frozen), rather
	/// than a short attempt that gives up: giving up re-acquired with a fresh lock uuid each
	/// retry, forfeiting our place behind the unbounded FS-op acquirers and starving the resync
	/// under contention. If even the patient acquisition times out, nothing is committed — it
	/// durably marks `needs_resync` and arms [`RESYNC_RETRY_INTERVAL`]; any listing failure does
	/// the same. The listing reads happen UNDER the lock; events are NOT drained then (that would
	/// advance the watermark past the snapshot id being committed).
	///
	/// CONTRACT: `list_dir_recursive` is a single `dir/download` of the whole subtree, so
	/// the returned tree is ANCESTOR-CLOSED — every returned item's parent chain up to the sync root is
	/// also returned. The diff passes (orphan sweep, cascade-on-delete) rely on this: an item absent
	/// from the listing cannot have a listed descendant. MEMORY: the whole subtree plus
	/// its synthetic events are held in RAM, like `list_dir_recursive` itself (which documents a >1GiB
	/// footprint for very large trees); bounding this is deferred.
	async fn run_resync(&mut self) -> Result<(), Box<CacheError>> {
		let Some(deps) = self.resync.clone() else {
			tracing::warn!(
				"resync requested but client/runtime deps are absent (test construction?); skipping"
			);
			return Ok(());
		};

		let account_root = self.root_uuid;
		let sync_roots: Vec<Uuid> = self.sync_roots.keys().copied().collect();

		// Progress brackets: `Started` fires BEFORE the drive-lock wait (another device can hold
		// the lock for a while), and every exit path past this point fires `Finished`, so a
		// consumer's spinner can never hang on a failed attempt.
		Self::send_resync_progress(
			&self.msg_sender,
			ResyncProgress::Started {
				roots: sync_roots.clone(),
			},
		);

		// FAST PATH — no sync roots: there is nothing to list, so nothing needs the drive lock
		// OR a consistent snapshot (the lock only ever made the listing consistent with the
		// snapshot id). Read the remote drive id WITHOUT the lock and advance the watermark to it,
		// so the startup gap-check stops re-firing. This is the common fresh-worker-boot case (the
		// gap-check runs before any `AddSyncRoot` is processed) and every integration test's
		// worker startup — taking the account-wide WRITE lock there is pure waste and serializes
		// the whole suite. SAFE: with zero roots the cache mirrors nothing, so jumping the
		// watermark cannot skip an event that affects it, and a later add's convergence resync
		// re-reads a fresh snapshot UNDER the lock to populate its root consistently.
		if sync_roots.is_empty() {
			Self::send_resync_progress(&self.msg_sender, ResyncProgress::Applying);
			let converged = match deps.client.get_last_event_ids().await {
				Ok(ids) => match self.commit_resync_watermark(ids.drive, false) {
					Ok(()) => true,
					Err(e) => {
						// The watermark write failed; keep the durable flag set (best-effort —
						// it is the same DB) so the armed retry actually re-fires this session,
						// matching the listing-failure arm below.
						self.surface_one(e);
						self.mark_needs_resync_surfacing_errors();
						false
					}
				},
				Err(e) => {
					// Couldn't reach the server to read the snapshot id — leave the watermark be
					// (the gap-check re-fires on the next event) and arm the retry timer for a
					// quiet account.
					tracing::debug!("resync: no sync roots; deferring watermark advance ({e})");
					self.mark_needs_resync_surfacing_errors();
					false
				}
			};
			Self::send_resync_progress(&self.msg_sender, ResyncProgress::Finished { converged });
			self.schedule_retry_if_unconverged(converged);
			return Ok(());
		}

		// List EACH sync root's subtree under ONE drive lock, reading the snapshot id under the same lock
		// so every root's listing is consistent at `remote_under_lock`. A subdir root is resolved via
		// `get_dir` (which also yields the node to materialize so the diff has an anchor row); the account
		// root uses its already-materialized `roots` row.
		// PATIENT lock acquisition. A SINGLE acquisition (one lock uuid) that NEVER gives up early:
		// giving up and re-acquiring with a fresh uuid forfeited our place in the server's waiter
		// set every retry, so the unbounded FS-op acquirers (which keep ONE uuid and wait) starved
		// the resync under contention. We drain events and serve read queries CONCURRENTLY while we
		// wait, so the worker stays live (the reason the acquisition used to be bounded — but
		// bounding is what lost the race). Draining happens only BEFORE we hold the lock; the
		// snapshot id we read once we hold it is >= any drained watermark (remote ids are
		// monotonic), so committing it never regresses the watermark.
		let lock = {
			let mut acquire = std::pin::pin!(
				deps.client
					.lock_drive_bounded(RESYNC_LOCK_MAX_SLEEP, RESYNC_LOCK_PATIENT_ATTEMPTS)
			);
			loop {
				tokio::select! {
					biased;
					acquired = &mut acquire => break acquired,
					event = self.event_receiver.recv() => match event {
						// `drain_pending` also drains the rest of the channel and handles its own
						// errors, so one recv keeps the whole backlog applied during the wait.
						Some(event) => self.drain_pending(Some(event)),
						// Channel closed = all event senders gone = the worker is shutting down.
						// Abandon the resync (don't wait out the lock). Mark `needs_resync` so next
						// session re-runs it — consistent with every other non-converged exit here;
						// the startup gap-check is the backstop if even this write fails. Emitting
						// `Finished` keeps progress consumers tidy.
						None => {
							// A queued Shutdown will re-drive the run loop, so no retry timer.
							self.abort_resync_unconverged(false);
							return Ok(());
						}
					},
					_ = self.control_receiver.peek() => {
						// A `Shutdown` or a registration change (Add/Remove) arrived mid-wait (or the
						// channel closed). STOP waiting on the contended lock either way: a Shutdown must
						// let the worker exit promptly instead of blocking out the patient wait, and an
						// add/remove makes this attempt's root snapshot stale so it should restart against
						// the new set. Aborting here is clean — NO lock is held yet and nothing is
						// committed. `peek` LEFT the message queued (a closed channel stays observable as
						// `None`), so whoever consumes the channel next applies it: the enclosing
						// `process_control_burst`'s drain loop, or `run`'s control arm. Mark `needs_resync`
						// so a fresh resync follows an add/remove, and finish. The queued message will
						// re-drive the loop, so no retry timer.
						self.abort_resync_unconverged(false);
						return Ok(());
					},
					read_task = recv_read_task(&mut self.read_tasks), if self.read_tasks.is_some() => {
						handle_read_task(read_task, &mut self.read_tasks, &self.db);
					},
				}
			}
		};
		let lock = match lock {
			Ok(lock) => lock,
			Err(e) => {
				if matches!(e.kind(), ErrorKind::RetryFailed) {
					tracing::debug!(
						"resync: drive lock still contended after a patient wait; retrying in {RESYNC_RETRY_INTERVAL:?}"
					);
				} else {
					tracing::warn!(
						"resync: drive lock acquisition failed ({e}); retrying in {RESYNC_RETRY_INTERVAL:?}"
					);
				}
				self.abort_resync_unconverged(true);
				return Ok(());
			}
		};

		// Now hold the lock and list each root. Serve read queries during the network listing, but
		// do NOT drain events here — that would advance the watermark past `remote_under_lock`,
		// the snapshot id we commit below.
		let msg_sender = &self.msg_sender;
		let island = async {
			let remote_under_lock = deps.client.get_last_event_ids().await?.drive;
			let mut per_root_raw: Vec<RootListing> = Vec::with_capacity(sync_roots.len());
			// Roots the server reported GONE (a definitive not-found): `finalize_resync` deletes their
			// cached subtrees, drops them from the active set, and notifies the app. Kept distinct from a
			// transient skip so a deleted root is removed rather than re-listed (and re-failed) forever.
			let mut deleted_roots: Vec<Uuid> = Vec::new();
			// Set when a root fails with a NON-not-found (network/server) error. `finalize_resync` uses it
			// to keep an all-transient resync from advancing the watermark past a gap it never reconciled.
			let mut any_transient = false;
			for (root_index, root) in sync_roots.iter().enumerate() {
				let root_node: Option<RemoteDirectory> = if *root == account_root {
					// The account root always exists and resyncs via `client.root()`, not `get_dir`.
					None
				} else {
					match deps.client.get_dir(*root).await {
						Ok(node) => Some(node),
						Err(e)
							if matches!(
								e.kind(),
								ErrorKind::FolderNotFound | ErrorKind::FileNotFound
							) =>
						{
							// Gone server-side (deleted while offline, or a cascade we missed). A not-found
							// is definitive, so drop it rather than skip-and-retry.
							tracing::warn!(
								"resync: sync root {root} no longer exists ({e}); dropping it"
							);
							deleted_roots.push(*root);
							continue;
						}
						Err(e) => {
							// Transient: skip and retry on a later resync (a single unreachable root must
							// not stall the others). The lock + snapshot-id calls above stay fatal.
							tracing::debug!(
								"resync: skipping sync root {root} (get_dir failed: {e})"
							);
							any_transient = true;
							continue;
						}
					}
				};
				let dir_type: DirType<'_, Normal> = match &root_node {
					Some(node) => node.into(),
					None => deps.client.root().into(),
				};
				// Forward the listing's byte ticks (cumulative; the HTTP layer throttles them to
				// ~200 ms) straight onto the status channel.
				let progress = |bytes_downloaded: u64, total_bytes: Option<u64>| {
					Self::send_resync_progress(
						msg_sender,
						ResyncProgress::Listing {
							root: *root,
							root_index,
							root_count: sync_roots.len(),
							bytes_downloaded,
							total_bytes,
						},
					);
				};
				let listed = deps
					.client
					.list_dir_recursive::<Normal, _>(&dir_type, Some(&progress), ())
					.await;
				drop(dir_type);
				match listed {
					Ok((dirs, files)) => per_root_raw.push((*root, root_node, dirs, files)),
					// A folder deleted between `get_dir` and the listing reads as not-found here too —
					// treat it the same (drop), but never the account root (it cannot be deleted).
					Err(e)
						if *root != account_root
							&& matches!(
								e.kind(),
								ErrorKind::FolderNotFound | ErrorKind::FileNotFound
							) =>
					{
						tracing::warn!(
							"resync: sync root {root} vanished during listing ({e}); dropping it"
						);
						deleted_roots.push(*root);
					}
					Err(e) => {
						tracing::debug!("resync: skipping sync root {root} (listing failed: {e})");
						any_transient = true;
					}
				}
			}
			Ok(ResyncListing {
				per_root_raw,
				deleted_roots,
				any_transient,
				remote_under_lock,
			})
		};
		// Serve search read queries WHILE the listing runs (minutes on a large account): the wasm
		// read path has no other server, so without this every search query would queue for the
		// listing's duration. The tasks are pure SELECTs against `self.db`, which the island never
		// touches (its DB work is in `finalize_resync`, after it returns). (Events are served the
		// same way during the lock wait above, but NOT here — see the snapshot-consistency note.)
		let listing: Result<_, crate::Error> = {
			let mut island = std::pin::pin!(island);
			loop {
				tokio::select! {
					biased;
					listing = &mut island => break listing,
					read_task = recv_read_task(&mut self.read_tasks), if self.read_tasks.is_some() => {
						handle_read_task(read_task, &mut self.read_tasks, &self.db);
					},
				}
			}
		};
		// Release the drive lock now — `finalize_resync` is local-only DB work.
		drop(lock);

		let listing = match listing {
			Ok(listing) => listing,
			Err(e) => {
				// A snapshot-id read or per-root listing failed (the lock was already held) —
				// nothing is committed. Durably mark the flag rather than relying on the trigger
				// having set it (the startup gap-check resyncs on a watermark lag WITHOUT
				// marking), then arm the retry timer so even a quiet account re-attempts.
				tracing::debug!(
					"resync listing failed ({e}); retrying in {RESYNC_RETRY_INTERVAL:?}"
				);
				self.abort_resync_unconverged(true);
				return Ok(());
			}
		};

		Self::send_resync_progress(&self.msg_sender, ResyncProgress::Applying);
		let result = self.finalize_resync(listing);
		// `converged` is read back from the durable flag — the engine's own definition of "work
		// remains": a partial or all-transient resync keeps it set (or re-marks it) in the same
		// transaction that commits any progress, so `Ok` + a clear flag is exactly "nothing
		// pending".
		let converged = result.is_ok() && matches!(self.needs_resync(), Ok(false));
		Self::send_resync_progress(&self.msg_sender, ResyncProgress::Finished { converged });
		self.schedule_retry_if_unconverged(converged);
		result
	}

	/// Best-effort resync-progress emission: lossy `try_send` like every status-channel message,
	/// so a tick can never block the worker (or the listing's HTTP task it is called from) and
	/// is harmless to drop. An associated fn taking the sender directly, so the listing progress
	/// closure can emit while `self` is otherwise borrowed.
	fn send_resync_progress(
		msg_sender: &tokio::sync::mpsc::Sender<Vec<CacheMessage>>,
		progress: ResyncProgress,
	) {
		let _ = msg_sender.try_send(vec![CacheMessage::ResyncProgress(progress)]);
	}

	/// Apply the outcome of [`run_resync`]'s locked listing: drop any sync roots the server reported
	/// GONE, decide whether the listing is trustworthy enough to commit, then converge the roots that
	/// listed.
	///
	/// `deleted_roots` returned a definitive not-found: each is removed from the cache (subtree included),
	/// dropped from the active set, and reported to the app, regardless of the commit decision below. A
	/// not-found is definite, so this is done unconditionally; it is crash-idempotent (a crash before the
	/// watermark advances just re-detects the same not-found next resync).
	///
	/// `any_transient` records that at least one root failed with a NON-not-found (network/server) error.
	/// If NO root listed successfully yet something failed transiently, the resync reconciled nothing it
	/// can trust: it leaves `needs_resync` set and does NOT advance the watermark, so a later cycle retries.
	/// Committing an empty convergence here would advance the watermark past — and clear the resync flag
	/// for — a gap that was never healed (silent data loss). An empty config or an all-roots-deleted resync
	/// has `any_transient == false`, so it still commits + advances the watermark, keeping the gap-check
	/// from looping.
	fn finalize_resync(&mut self, listing: ResyncListing) -> Result<(), Box<CacheError>> {
		let ResyncListing {
			per_root_raw,
			deleted_roots,
			any_transient,
			remote_under_lock,
		} = listing;
		// Remove the deleted roots' cached subtrees (the cascade trigger recurses) so we don't leak items
		// under a root that no longer gates membership, then drop them from the active set + notify. The
		// account root is never in `deleted_roots` (it is never `get_dir`'d and cannot be deleted).
		if !deleted_roots.is_empty() {
			match self.delete_items(deleted_roots.iter().copied()) {
				// Only drop the roots from the active set + notify the app once their subtrees are
				// actually gone, mirroring the socket `DirEvent::Removed` path.
				Ok(()) => self.handle_deleted_sync_roots(deleted_roots),
				Err(e) => {
					// The delete failed mid-run. Do NOT remove these roots from `sync_roots`: leaving
					// them active lets the next resync re-detect the server-side deletion and retry the
					// cleanup, instead of stranding their rows untracked forever. Flag a resync so that
					// retry actually happens.
					self.surface_errors(db_err_vec(
						e,
						"deleting server-deleted sync-root subtrees".to_string(),
					));
					self.mark_needs_resync_surfacing_errors();
				}
			}
		}

		// An all-transient resync must not advance the watermark / clear the flag.
		if per_root_raw.is_empty() && any_transient {
			tracing::debug!(
				"resync: every sync root failed to list transiently; leaving needs_resync set for retry"
			);
			return Ok(());
		}

		// Convert each root's listing to owned cacheables (a non-cacheable record is surfaced, not
		// fatal), and materialize each subdir root's node so the diff's subtree CTE and the
		// membership ancestry walk have an anchor row.
		let mut errors = Vec::new();
		let mut per_root: Vec<(
			Uuid,
			Vec<CacheableDir<'static>>,
			Vec<CacheableFile<'static>>,
		)> = Vec::with_capacity(per_root_raw.len());
		for (root, root_node, dirs, files) in per_root_raw {
			if let Some(node) = root_node {
				// Materialize the subdir root's own node so the diff CTE + the membership ancestry walk
				// have an anchor row. If that fails, SKIP this root: diffing without an anchor row could
				// emit spurious synthetics (no subtree to scope deletes, no parent context for creates).
				// Leaving the root un-converged is safe — the next resync retries it.
				let materialized = match CacheableDir::try_from(node) {
					Ok(cacheable) => self.upsert_dirs(once(&cacheable)).map_err(|e| {
						CacheError::db(e, format!("materializing sync-root node {root}"))
					}),
					Err((node, e)) => Err(CacheError::dir_cacheable_conversion(node, e.into())),
				};
				if let Err(err) = materialized {
					errors.push(err);
					continue;
				}
			}
			let (cdirs, cfiles) = convert_listing(dirs, files, &mut errors);
			per_root.push((root, cdirs, cfiles));
		}
		if !errors.is_empty() {
			self.surface_errors(errors);
		}

		// A PARTIAL transient (some roots listed, some skipped) commits the converged roots'
		// progress but keeps `needs_resync` SET in the same transaction, so the skipped roots are
		// durably retried by a later drain instead of stranded stale (or, for a freshly added
		// root, stranded empty) until the next unrelated gap.
		self.apply_resync(per_root, remote_under_lock, any_transient)
	}

	/// Membership gate: is the upsert target's `parent` inside any configured sync root? An
	/// out-of-root `New`/`Move`/`Changed` must NOT be stored — the account-wide socket subscription
	/// would otherwise mirror the whole account into `items` — but the event still advances the
	/// watermark (the caller returns `Ok(())` so the drain treats it as applied). `Removed`/`Archived`
	/// need no gate (delete-of-missing is already idempotent), nor do metadata patches (they no-op on a
	/// row that is not cached). Errors surface as the apply path's `Vec<CacheError>`.
	///
	/// KNOWN LIMITATION: membership is resolved from the CACHED ancestry. If a `New`
	/// arrives for an in-root item whose parent chain is not yet cached (a gap dropped the parent's
	/// `New`), this returns false and the item is dropped, watermark-advanced. This does NOT occur in
	/// whole-account mode (the account root is the seed, so under ascending delivery every parent is
	/// cached before its child). In a selective (subdir-root) config it can occur during a gap, but the
	/// gap itself is detected (a hole → `needs_resync`) and the resync re-lists the root ancestor-closed,
	/// re-creating the chain — so it self-heals on the next resync. The narrow unhealed edge (the dropped
	/// `New` is the latest event with no follow-up to expose a hole) is accepted in v1: the alternatives
	/// (mark `needs_resync` on every gated event → resync storm in selective mode; or upsert optimistically
	/// → re-mirror the account + un-GC-able orphans) are worse than the bounded staleness.
	fn parent_in_sync_root(&self, parent: Uuid) -> Result<bool, Vec<CacheError>> {
		self.in_any_sync_root(parent, &self.sync_roots)
			.map_err(|e| {
				db_err_vec(
					e,
					format!("checking sync-root membership for parent {parent}"),
				)
			})
	}

	fn handle_file_event(
		&mut self,
		event: FileEvent,
		trust: EventTrust,
	) -> Result<(), Vec<CacheError>> {
		match event {
			FileEvent::New(file) | FileEvent::Changed(file) => {
				// skip the upsert for an out-of-root file, but still advance the watermark. (A
				// New/Changed has no prior in-root row to leave stale, unlike a Move — see below.)
				if trust == EventTrust::Checked && !self.parent_in_sync_root(file.parent)? {
					return Ok(());
				}
				self.upsert_files(once(&file)).map_err(|e| {
					// Identify the item by uuid only — the file payload carries the FileKey, which must
					// never reach an error string or log.
					db_err_vec(e, format!("failed to upsert file: {}", file.uuid))
				})
			}
			FileEvent::Move(file) => {
				// A move WITHIN/INTO a sync root re-parents (upsert). A move OUT of every sync root must
				// DELETE the now-stale cached row: skipping would leave the item under its
				// OLD parent, where a later cascade-delete of that parent would wrongly remove a still-live
				// item. delete-of-missing is a no-op if it was never cached; dispatch still notifies the
				// old root via the pre-move parent snapshot.
				if trust == EventTrust::TrustedSynthetic || self.parent_in_sync_root(file.parent)? {
					self.upsert_files(once(&file)).map_err(|e| {
						// uuid only — the file payload carries the FileKey (never log key material).
						db_err_vec(e, format!("failed to upsert moved file: {}", file.uuid))
					})
				} else {
					self.delete_items(once(file.uuid)).map_err(|e| {
						db_err_vec(e, format!("failed to delete moved-out file: {}", file.uuid))
					})
				}
			}
			FileEvent::Archived(uuid) | FileEvent::Removed(uuid) => self
				.delete_items(once(uuid))
				.map_err(|e| db_err_vec(e, format!("failed to delete file with uuid: {}", uuid))),
			FileEvent::MetadataChanged { uuid, meta } => {
				self.update_file_meta(uuid, &meta).map_err(|e| {
					// uuid only — `meta` holds the FileKey; never serialize it into an error/log.
					db_err_vec(e, format!("updating file meta for uuid: {uuid}"))
				})
			}
		}
	}

	fn handle_dir_event(
		&mut self,
		event: DirEvent,
		trust: EventTrust,
	) -> Result<(), Vec<CacheError>> {
		match event {
			DirEvent::New(dir) | DirEvent::Changed(dir) => {
				// Skip the upsert for an out-of-root dir, but still advance the watermark. A dir that IS
				// itself a sync root is the exception (same as the Move arm): a root whose own parent is
				// out-of-root must still apply its own New/Changed, else its metadata goes stale.
				if trust == EventTrust::Checked
					&& !self.sync_roots.contains_key(&dir.uuid)
					&& !self.parent_in_sync_root(dir.parent)?
				{
					return Ok(());
				}
				self.upsert_dirs(once(&dir))
					.map_err(|e| db_err_vec(e, format!("failed to upsert dir: {}", dir.uuid)))
			}
			DirEvent::Move(dir) => {
				// As for files: a move OUT of every sync root deletes the stale cached
				// subtree (cascade handles its children) rather than leaving it under the old parent.
				//
				// EXCEPTION: a dir that IS itself a sync root must be UPSERTED (re-parented), never
				// deleted — it stays a configured root wherever it moves, so its subtree must survive. The
				// parent gate would otherwise see its new (out-of-root) parent and wrongly wipe the whole
				// root. (Files are never sync roots, so the file arm above needs no such check.)
				if trust == EventTrust::TrustedSynthetic
					|| self.sync_roots.contains_key(&dir.uuid)
					|| self.parent_in_sync_root(dir.parent)?
				{
					self.upsert_dirs(once(&dir)).map_err(|e| {
						db_err_vec(e, format!("failed to upsert moved dir: {}", dir.uuid))
					})
				} else {
					// The cascade also wipes any nested sync root under this dir, so drop + notify them
					// here (as the Removed arm does) — otherwise a nested root lingers as a zombie.
					let deleted_roots = self.sync_roots_deleted_by(dir.uuid);
					self.delete_items(once(dir.uuid)).map_err(|e| {
						db_err_vec(e, format!("failed to delete moved-out dir: {}", dir.uuid))
					})?;
					self.handle_deleted_sync_roots(deleted_roots);
					Ok(())
				}
			}
			DirEvent::Removed(uuid) => {
				// snapshot which sync roots this removal drops (the target itself, or — via the
				// cascade trigger — nested roots under it) BEFORE the delete erases their ancestry, then
				// drop + notify only once the delete actually lands.
				let deleted_roots = self.sync_roots_deleted_by(uuid);
				self.delete_items(once(uuid)).map_err(|e| {
					db_err_vec(e, format!("failed to delete dir with uuid: {}", uuid))
				})?;
				self.handle_deleted_sync_roots(deleted_roots);
				Ok(())
			}
			DirEvent::MetadataChanged { uuid, meta } => self
				.update_dir_name(uuid, &meta)
				.map_err(|e| db_err_vec(e, format!("failed to update dir name for uuid: {uuid}"))),
			DirEvent::ColorChanged { uuid, color } => {
				self.update_dir_color(uuid, &color).map_err(|e| {
					db_err_vec(
						e,
						format!(
							"failed to update dir color for uuid {} and color {:?}",
							uuid, color
						),
					)
				})
			}
		}
	}

	fn handle_global_event(&mut self, event: GlobalEvent) -> Result<(), Vec<CacheError>> {
		match event {
			GlobalEvent::DeleteAll => {
				self.delete_all_non_root().map_err(|e| {
					db_err_vec(
						e,
						"failed to delete all non-root items from cache".to_string(),
					)
				})?;
				// The wipe also removed the ancestry rows the membership gate walks, so every subsequent
				// live event for a configured sync root would be dropped until a resync re-lists it.
				// Schedule one (the next drain's maybe_run_resync heals it). Whole-account mode keeps its
				// account-root row, so a resync there just re-lists the now-empty drive — also correct.
				self.mark_needs_resync().map_err(|e| vec![*e])
			}
			GlobalEvent::TrashEmpty => {
				Ok(())
				// noop, we don't track trashed items
			}
			GlobalEvent::DeleteVersioned => {
				Ok(())
				// todo, implement version tracking
			}
		}
	}

	fn handle_manual_event(&mut self, event: ManualEvent) -> Result<(), Vec<CacheError>> {
		match event {
			ManualEvent::ListDirRecursive(dirs, files) => {
				// Eagerly convert FIRST (collecting any per-item conversion errors), then upsert, so a DB
				// error on the bulk insert is surfaced ALONGSIDE the conversion errors rather than
				// discarding the ones accumulated so far.
				let mut errors = Vec::new();
				let (cdirs, cfiles) = convert_listing(dirs, files, &mut errors);
				if let Err(e) = self.upsert_dirs(cdirs.iter()) {
					errors.push(CacheError::db(
						e,
						"failed bulk-inserting listed dirs".to_string(),
					));
				} else if let Err(e) = self.upsert_files(cfiles.iter()) {
					errors.push(CacheError::db(
						e,
						"failed bulk-inserting listed files".to_string(),
					));
				}
				if errors.is_empty() {
					Ok(())
				} else {
					Err(errors)
				}
			}
		}
	}
}

/// Convert a freshly-listed remote subtree into owned cacheables for the resync diff. A
/// record that cannot be made cacheable (e.g. a non-uuid parent) is pushed to `errors` (non-fatal) and
/// skipped, so one bad record never aborts the whole resync.
fn convert_listing(
	dirs: Vec<RemoteDirectory>,
	files: Vec<RemoteFile>,
	errors: &mut Vec<CacheError>,
) -> (Vec<CacheableDir<'static>>, Vec<CacheableFile<'static>>) {
	let mut cacheable_dirs = Vec::with_capacity(dirs.len());
	for dir in dirs {
		match CacheableDir::try_from(dir) {
			Ok(cacheable) => cacheable_dirs.push(cacheable),
			Err((dir, e)) => errors.push(CacheError::dir_cacheable_conversion(dir, e.into())),
		}
	}
	let mut cacheable_files = Vec::with_capacity(files.len());
	for file in files {
		match CacheableFile::try_from(file) {
			Ok(cacheable) => cacheable_files.push(cacheable),
			Err((file, e)) => errors.push(CacheError::file_cacheable_conversion(file, e.into())),
		}
	}
	(cacheable_dirs, cacheable_files)
}

fn db_err_vec(error: rusqlite::Error, context: String) -> Vec<CacheError> {
	vec![CacheError::db(error, context)]
}

/// The target uuid whose PRE-apply parent the dispatcher must snapshot — only delete/move events erase
/// or rewrite the parent; everything else resolves from the post-apply `items` state.
fn dispatch_presnapshot_target(event: &CacheEventType<'_>) -> Option<Uuid> {
	match event {
		CacheEventType::File(FileEvent::Move(f)) => Some(f.uuid),
		CacheEventType::File(FileEvent::Removed(uuid) | FileEvent::Archived(uuid)) => Some(*uuid),
		CacheEventType::Dir(DirEvent::Move(d)) => Some(d.uuid),
		CacheEventType::Dir(DirEvent::Removed(uuid)) => Some(*uuid),
		_ => None,
	}
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
	if let Some(s) = panic.downcast_ref::<&str>() {
		(*s).to_string()
	} else if let Some(s) = panic.downcast_ref::<String>() {
		s.clone()
	} else {
		"non-string panic payload".to_string()
	}
}

mod event;
// The applied-event types are part of the public API (the `SyncRootCallback` receives `&CacheEvent`).
pub use event::{CacheEvent, CacheEventType, DirEvent, FileEvent, GlobalEvent};
// Internal only: the channel-carried wrapper never reaches a callback.
pub(crate) use event::CacheEventMaybeDecrypted;

#[cfg(test)]
mod tests;
