use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	sync::{
		Arc, LazyLock, Weak,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::timeout as ack_timeout;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::tokio::timeout as ack_timeout;

use crate::{
	Error, ErrorKind,
	auth::Client,
	fs::HasUUID,
	io::{RemoteDirectory, RemoteFile},
	socket::ListenerHandle,
};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use crate::cache::{
	CacheControlMessage, CacheError, CacheState, SyncRootCallback,
	search::ReadTask,
	state::{CacheThreadEvent, ManualEvent},
};

/// How long [`spawn_cache_worker`] waits for the new worker's init ack before presuming it dead.
/// Init is local-only (open the DB, build the schema — no network), so this is generous; it
/// exists for the wasm failure modes where a worker dies WITHOUT dropping its channels.
const CACHE_INIT_ACK_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum CacheMessage {
	/// Non-fatal errors surfaced by the worker. The worker keeps running after emitting these — they are
	/// informational, not a shutdown signal — though repeated errors may warrant the app restarting it.
	Error(Vec<CacheError>),
	/// One or more configured sync roots were deleted server-side (a `Removed` of the root node, or a
	/// cascade when an ancestor was deleted or moved out). They have been dropped from the active set —
	/// their [`SyncRootHandle`]s are inert from here on — and the app must re-issue
	/// [`add_sync_root`](Client::add_sync_root) to resume syncing them.
	SyncRootsDeleted(Vec<Uuid>),
	/// Progress of a convergence resync (see [`ResyncProgress`] for the lifecycle and its
	/// attribution caveats). Lossy like every status message — a tick can be dropped under load —
	/// so treat each one as a fresh snapshot, never accumulate deltas.
	ResyncProgress(ResyncProgress),
}

/// One step of a convergence resync's lifecycle: the whole-subtree `dir/download` listings the
/// worker runs when a NEW sync root is added (e.g. by starting a
/// [`Search`](crate::cache::Search) on an uncovered directory) or when it heals a detected event
/// gap. Delivered via [`CacheMessage::ResyncProgress`] on the
/// [`configure_cache`](Client::configure_cache) status callback.
///
/// A resync is WORKER-GLOBAL: it relists EVERY registered root under one drive lock — quick
/// successive [`add_sync_root`](Client::add_sync_root) calls coalesce into ONE resync, and
/// gap-healing resyncs run with no add in flight at all — so progress is keyed by ROOT uuid
/// rather than by caller; correlate with [`SyncRootHandle::uuid`] /
/// [`Search::root_uuid`](crate::cache::Search::root_uuid). A COVERED add (a uuid already cached
/// under an active root) triggers no resync and therefore no progress messages at all.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResyncProgress {
	/// A resync attempt began. It first waits (bounded) on the drive lock, which another device
	/// may hold — a contended attempt ends in [`Finished { converged: false }`](Self::Finished)
	/// within seconds and is retried shortly, each retry emitting its own `Started`. `roots` is
	/// the full set being converged, in listing order — possibly empty (a watermark-only
	/// catch-up with no registered roots).
	Started { roots: Vec<Uuid> },
	/// Byte progress of one root's whole-subtree listing download (`root_index` indexes
	/// [`Started`](Self::Started)'s `roots`). `bytes_downloaded` is CUMULATIVE within one HTTP
	/// attempt (an internal retry restarts it from 0); `total_bytes` is present only when the
	/// server reports a length; ticks arrive at most every ~200 ms. After the last byte the
	/// listing is still decrypted before the next root starts, so expect a pause at 100%.
	Listing {
		root: Uuid,
		root_index: usize,
		root_count: usize,
		bytes_downloaded: u64,
		total_bytes: Option<u64>,
	},
	/// Every root listed (or was skipped); the worker is diffing and committing the listings into
	/// the cache. No finer-grained progress — on very large listings this phase can take a while.
	Applying,
	/// The attempt ended — always fired once per [`Started`](Self::Started), so a consumer's
	/// spinner can never hang. `converged: true` means the convergence committed with nothing
	/// left pending: cache truth now holds a complete listing of every registered root's subtree
	/// (for a search, the result set is as complete as the server was at the snapshot). `false`
	/// means the attempt failed or partially failed — errors arrive separately as
	/// [`CacheMessage::Error`] — and the worker retries on a later cycle (which emits its own
	/// `Started`).
	Finished { converged: bool },
}

/// One-time cache configuration stored on the [`Client`]: the SQLite DB path and the global
/// status callback. Survives worker restarts — every (re)spawn clones it.
pub(crate) struct CacheConfig {
	path: PathBuf,
	/// `Arc` so each respawned worker's status-bridge task can reuse the same app callback.
	status_callback: Arc<dyn Fn(Vec<CacheMessage>) + Send + Sync + 'static>,
}

/// The per-[`Client`] cache slot: the stored configuration plus a WEAK reference to the live
/// worker. The [`SyncRootHandle`]s hold the strong references — when the last one drops, the
/// shared state drops, the worker's control channel disconnects, and the worker drains and exits
/// (the socket-listener lifecycle).
#[derive(Default)]
pub(crate) struct CacheSlot {
	config: Option<Arc<CacheConfig>>,
	worker: Weak<CacheWorkerShared>,
	/// Resolves to `true` when the most recently spawned worker has fully exited (its SQLite
	/// connection is already closed by then). Deposited here IMMEDIATELY after the spawn (before
	/// any await) and awaited via [`wait_for_worker_exit`] before any respawn — and by
	/// [`Client::flush_cache`] — so even a CANCELLED spawn/flush future cannot leave a detached
	/// worker overlapping a successor on the DB file. The signal fires on every NATIVE exit
	/// path including panics (the unwind runs `SignalOnDrop`, which sends — and logs — the
	/// panic). On wasm, panics ABORT the
	/// worker without unwinding, leaking the sender: a post-init trap leaves this signal
	/// permanently pending, so the cache is unrecoverable without a page reload (the in-memory
	/// DB died with the worker anyway); init-time failures are bounded by the spawn ack timeout.
	finished: Option<tokio::sync::watch::Receiver<bool>>,
}

impl CacheSlot {
	/// The configured cache DB path, if [`Client::configure_cache`] has run. Stable while a
	/// worker is live (reconfiguration is rejected then) — the search engine opens its own
	/// READ-ONLY connection on it.
	// Native-only: the wasm cache routes searches through the worker (`ReadConn::Worker`), not a
	// second connection opened on this path.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub(crate) fn db_path(&self) -> Option<PathBuf> {
		self.config.as_ref().map(|config| config.path.clone())
	}
}

/// Worker-side senders shared by every [`SyncRootHandle`] (strongly) and the [`Client`]'s slot
/// (weakly). Dropping the last strong reference drops `control_sender`, which the worker's run
/// loop treats as a clean shutdown (drain, close the DB, exit); dropping `listener_handle` then
/// unregisters the socket listener.
pub(crate) struct CacheWorkerShared {
	/// This worker's key in [`LIVE_WORKERS`] — the canonicalized path of the SQLite DB it owns.
	db_path: PathBuf,
	control_sender: UnboundedSender<CacheControlMessage>,
	manual_event_sender: tokio::sync::mpsc::Sender<CacheThreadEvent>,
	/// Ships search read queries to the worker's connection — the WASM read path (no WAL, no
	/// second connection there). Native searches read via their own connection instead.
	read_task_sender: UnboundedSender<ReadTask>,
	next_registration_id: AtomicU64,
	/// `Some` until either the shared state drops (last handle gone) or [`Client::flush_cache`]
	/// takes it — inert handles outliving a flush must not keep the websocket subscribed (and
	/// decrypting every drive event) for a dead worker.
	listener_handle: std::sync::Mutex<Option<ListenerHandle>>,
}

/// Every live cache worker in this PROCESS, keyed by the DB it owns. Weak on purpose: the registry
/// must never extend a worker's life — the [`SyncRootHandle`]s stay its only owners.
///
/// Each [`Client`] has its own [`CacheSlot`], so without this two `Client`s configured on the SAME
/// database (two file-provider instances overlapping across a replacement, or several domains
/// hosted in one process) would each spawn a worker: two socket consumers and two writers on one
/// `events` table, each advancing the one drive watermark past events the other never applied.
/// Acquisition JOINs instead — the engine already holds many `(registration_id, callback)` pairs
/// per root, so sharing costs the joiner nothing.
///
/// A joined worker keeps its FIRST creator's `Client`. Fine today: every instance authenticates
/// the same account from one shared auth file — revisit if per-instance auth can ever diverge.
/// Multi-account is a per-domain `db_dir` away, and needs nothing here: distinct paths key distinct
/// entries, so distinct domains get distinct workers for free.
///
/// Lock discipline: a LEAF lock, never held across an await or a call into a worker.
static LIVE_WORKERS: LazyLock<std::sync::Mutex<HashMap<PathBuf, Weak<CacheWorkerShared>>>> =
	LazyLock::new(Default::default);

fn live_workers() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Weak<CacheWorkerShared>>> {
	LIVE_WORKERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The registry key for a configured cache path: the DB's file name under its CANONICALIZED PARENT.
/// Canonicalizing the parent rather than the file itself is what makes the key stable before the DB
/// exists — the first spawn runs on a path with no file yet, and its key must equal the one a later
/// joiner computes. An unresolvable parent falls back to the path as given (such spellings then
/// simply don't join, i.e. today's behavior).
fn registry_key(path: &Path) -> PathBuf {
	// wasm has no filesystem: the path names the wasm VFS's store, already canonical.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
		&& let Ok(dir) = std::fs::canonicalize(parent)
	{
		return dir.join(name);
	}
	path.to_path_buf()
}

/// The live worker on `key`, if there is one. A dead entry is REMOVED on the way, never handed
/// back — the caller spawns a fresh worker and registers it.
fn join_live_worker(key: &Path) -> Option<Arc<CacheWorkerShared>> {
	let mut workers = live_workers();
	match workers.get(key).and_then(Weak::upgrade) {
		Some(shared) => Some(shared),
		None => {
			workers.remove(key);
			None
		}
	}
}

fn register_worker(shared: &Arc<CacheWorkerShared>) {
	live_workers().insert(shared.db_path.clone(), Arc::downgrade(shared));
}

/// Forget a worker that has been told to stop ([`Client::flush_cache`]) or found dead
/// ([`Client::mark_worker_stale`]) so no other `Client` can join it: inert handles may still hold
/// strong references, which would keep its `Weak` looking alive. A NEWER entry for the same path (a
/// concurrent respawn) is left intact.
fn unregister_worker(shared: &Arc<CacheWorkerShared>) {
	let mut workers = live_workers();
	if workers
		.get(&shared.db_path)
		.is_some_and(|worker| worker.ptr_eq(&Arc::downgrade(shared)))
	{
		workers.remove(&shared.db_path);
	}
}

/// Sends `true` on the paired watch channel when dropped — the worker's exit signal, guaranteed
/// to fire on every NATIVE exit path including a panic (the unwind drops the run future's
/// locals; on wasm a panic aborts the worker without unwinding and the signal is lost — see
/// [`CacheSlot::finished`]). Declared FIRST in the worker future so it drops LAST, i.e. after
/// the `CacheState` (and its SQLite connection) is gone.
struct SignalOnDrop(tokio::sync::watch::Sender<bool>);

impl Drop for SignalOnDrop {
	fn drop(&mut self) {
		// Unwinding still SENDS the signal (the worker is gone either way), so this branch is
		// the only platform-visible record of a native worker panic — mobile's oslog/logcat
		// never see std's stderr panic message. (Irrelevant on wasm: panics abort, no unwind.)
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		if std::thread::panicking() {
			tracing::error!("cache worker panicked; shutting down");
		}
		let _ = self.0.send(true);
	}
}

/// Wait (cancel-safely) for the slot's current worker to exit. The exit signal stays IN the slot
/// until the worker has actually finished, so a caller cancelled mid-await leaves it behind for
/// the next add/flush to await — a detached worker can never overlap a successor on the DB file.
/// No-op when nothing was spawned. (The worker is not joinable: on native it is a detached
/// `runtime::spawn_async` thread, on wasm a self-closing web worker — the watch signal, sent as
/// the worker's last action after its DB connection closes, IS the deterministic exit wait.)
async fn wait_for_worker_exit(slot: &mut CacheSlot) {
	if let Some(finished) = slot.finished.as_mut() {
		// `Err` means the sender dropped WITHOUT ever signalling. A panicking worker still
		// signals (its unwind runs `SignalOnDrop`, which also logs the panic), so this only
		// fires when the worker future was dropped un-run — e.g. the host thread failed to
		// build its runtime, or an abandoned late starter exited at its entry check.
		if finished.wait_for(|done| *done).await.is_err() {
			tracing::error!("cache worker was torn down before it ever ran");
		}
	}
	slot.finished = None;
}

/// RAII registration of one sync root, returned by [`Client::add_sync_root`].
///
/// Dropping the handle stops this registration NON-destructively (the cached subtree stays; use
/// [`evict`](SyncRootHandle::evict) to also delete it). Multiple live handles may target the same
/// uuid — each holds its own registration and the uuid stops being synced only when the last one
/// goes. Dropping the last handle overall shuts the cache worker down.
pub struct SyncRootHandle {
	uuid: Uuid,
	registration_id: u64,
	/// Set when the registration was already consumed (`evict`) or never became live (a rejected
	/// add), so `Drop` does not send a removal.
	disarmed: bool,
	shared: Arc<CacheWorkerShared>,
}

impl Client {
	/// One-time cache configuration: the SQLite DB `cache_path` and the global `status_callback`
	/// receiving worker status messages ([`CacheMessage::Error`] /
	/// [`CacheMessage::SyncRootsDeleted`]). Pure storage — the DB is opened lazily by the first
	/// [`add_sync_root`](Client::add_sync_root) — and the config survives worker restarts.
	/// Reconfiguring is allowed while NO worker is live (before the first sync root, or after
	/// [`flush_cache`](Client::flush_cache) / dropping every handle); it errors while one is.
	pub async fn configure_cache(
		&self,
		cache_path: PathBuf,
		status_callback: impl Fn(Vec<CacheMessage>) + Send + Sync + 'static,
	) -> Result<(), Error> {
		let mut slot = self.cache_slot.lock().await;
		if slot.worker.upgrade().is_some() {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				"cannot reconfigure the cache while it is running; drop all sync-root handles or call flush_cache first",
			));
		}
		slot.config = Some(Arc::new(CacheConfig {
			path: cache_path,
			status_callback: Arc::new(status_callback),
		}));
		Ok(())
	}

	/// Register `uuid` as a sync root with its notification `callback`, returning an RAII
	/// [`SyncRootHandle`] — the cache analog of
	/// [`add_event_listener`](Client::add_event_listener). The first registration lazily opens the
	/// configured DB and starts the cache worker; dropping the last handle shuts it down again
	/// (drain + DB close — the next add respawns it, and the add-triggered convergence resync
	/// populates whatever it registers, retried durably until it succeeds).
	///
	/// Multiple live handles may target the same `uuid`: each gets its own registration whose
	/// callback is notified independently, and the uuid stops being synced only when the last one
	/// is dropped — so independent consumers never have to coordinate. One caveat: a server-side
	/// deletion of the root is announced ONLY on the global
	/// [`configure_cache`](Client::configure_cache) status callback
	/// ([`CacheMessage::SyncRootsDeleted`]); the other registrations' handles just go silently
	/// inert, so consumers that don't own the global callback must learn it from whoever does.
	///
	/// Errors if [`configure_cache`](Client::configure_cache) was never called, or if validation
	/// rejects `uuid` (it runs on the worker — the future resolves once the registration is live,
	/// which can wait on an in-flight resync). Downcast the error to [`CacheError`] to branch:
	/// [`CacheError::InvalidSyncRoot`] means the directory definitively no longer exists (any stale
	/// subtree a prior session cached under it has been wiped — do not retry);
	/// [`CacheError::SyncRootUnavailable`] means the validation itself failed (network/server —
	/// retry the same uuid). Must be called from within the app's Tokio runtime.
	///
	/// Do NOT move the returned handle (or anything that owns it) into a [`SyncRootCallback`]: the
	/// worker owns the callbacks, so a captured handle keeps the worker's control channel open from
	/// inside the worker itself — the drop-the-last-handle shutdown can then never fire, and the
	/// worker (with its DB connection and socket listener) lives until
	/// [`flush_cache`](Client::flush_cache).
	pub async fn add_sync_root(
		self: Arc<Self>,
		uuid: Uuid,
		callback: SyncRootCallback,
	) -> Result<SyncRootHandle, Error> {
		let mut callback = callback;
		// One respawn retry: the worker can exit between the slot's weak upgrade and the send
		// (e.g. a concurrent `flush_cache`, or it panicked while other handles kept it upgradable).
		// A failed SEND returns the message, so the callback is recovered for the retry.
		for _ in 0..2 {
			let shared = Client::get_or_spawn_worker(&self).await?;
			let registration_id = shared.next_registration_id.fetch_add(1, Ordering::Relaxed);
			let (ack_sender, ack_receiver) = tokio::sync::oneshot::channel();
			// Construct the handle BEFORE sending so that a caller dropping this future mid-await
			// still removes the registration: the handle's Drop queues a `RemoveRegistration` on
			// the same FIFO control channel, guaranteed to be processed after the `AddSyncRoot`.
			let mut handle = SyncRootHandle {
				uuid,
				registration_id,
				disarmed: false,
				shared: shared.clone(),
			};
			match shared
				.control_sender
				.send(CacheControlMessage::AddSyncRoot {
					uuid,
					registration_id,
					callback,
					ack: ack_sender,
				}) {
				Ok(()) => {}
				Err(tokio::sync::mpsc::error::SendError(CacheControlMessage::AddSyncRoot {
					callback: recovered,
					..
				})) => {
					handle.disarmed = true;
					callback = recovered;
					self.mark_worker_stale(&shared).await;
					continue;
				}
				Err(_) => unreachable!("send returns the message it was given"),
			}
			return match ack_receiver.await {
				Ok(Ok(())) => Ok(handle),
				Ok(Err(e)) => {
					// Rejected by validation — never registered, so disarm the handle (its Drop
					// removal would only be a logged no-op on the worker).
					handle.disarmed = true;
					Err(Error::custom_with_source(
						ErrorKind::InvalidState,
						*e,
						Some(format!("registering sync root {uuid}")),
					))
				}
				Err(_) => {
					// The worker shut down before processing the queued registration (e.g. a
					// concurrent `flush_cache` raced the send), dropping the message — and the
					// callback with it, so a transparent retry is impossible. The caller retries
					// with a fresh callback; the stale-marked slot respawns the worker then.
					handle.disarmed = true;
					self.mark_worker_stale(&shared).await;
					Err(Error::custom(
						ErrorKind::Internal,
						"cache worker shut down before the sync-root registration completed; retry",
					))
				}
			};
		}
		Err(Error::custom(
			ErrorKind::Internal,
			"cache worker repeatedly unavailable while registering a sync root",
		))
	}

	/// Deterministically stop the cache worker: signal shutdown, unregister its socket listener,
	/// then wait until the worker has drained its buffered events into the durable `events` store,
	/// applied them, and CLOSED the SQLite connection. Call on app close/suspend so the DB is fully
	/// flushed and nothing keeps decrypting socket events. The stored configuration is retained and
	/// existing [`SyncRootHandle`]s become INERT (their drops are no-ops); the next
	/// [`add_sync_root`](Client::add_sync_root) respawns the worker, and the add-triggered
	/// convergence resync populates whatever it registers.
	///
	/// NOT required for correctness: an un-joined drop (or an outright process kill) is recovered
	/// on the next startup by the gap-check — the watermark was never advanced for any un-drained
	/// event, so the remote drive id reads ahead of it and triggers a catch-up resync. This only
	/// makes shutdown deterministic. No-op when nothing is running.
	pub async fn flush_cache(&self) {
		let mut slot = self.cache_slot.lock().await;
		if let Some(shared) = slot.worker.upgrade() {
			// Signal shutdown synchronously (the control channel is unbounded; `send` never
			// blocks). If the worker already exited (e.g. every handle was dropped) the send
			// errors harmlessly. Also take + drop the socket listener registration NOW: inert
			// handles may outlive this flush, and they must not keep the websocket subscribed
			// (and decrypting every drive event) for a dead worker.
			let _ = shared.control_sender.send(CacheControlMessage::Shutdown);
			drop(
				shared
					.listener_handle
					.lock()
					.unwrap_or_else(|e| e.into_inner())
					.take(),
			);
			// And take it out of the process registry: no other `Client` may join a worker we just
			// told to stop, even while inert handles still hold strong references to it.
			unregister_worker(&shared);
		}
		slot.worker = Weak::new();
		// Cancel-safe deterministic wait under the slot lock: a concurrent `add_sync_root` cannot
		// spawn a second worker onto the same DB file mid-shutdown, and a CANCELLED flush leaves
		// the JoinHandle + exit signal in the slot for the next add/flush to reap.
		wait_for_worker_exit(&mut slot).await;
	}

	/// Return the live worker, or (re)spawn one from the stored config. The slot lock is held
	/// across the whole spawn — including waiting out a previous worker's exit — so concurrent
	/// calls cannot double-spawn and two workers can never write the same DB file.
	async fn get_or_spawn_worker(client: &Arc<Client>) -> Result<Arc<CacheWorkerShared>, Error> {
		let mut slot = client.cache_slot.lock().await;
		let Some(config) = slot.config.clone() else {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				"cache is not configured; call configure_cache first",
			));
		};
		if let Some(shared) = slot.worker.upgrade() {
			return Ok(shared);
		}
		// Another `Client` in this process may already run a worker on this DB — join it rather
		// than spawn a second one onto the same database (see [`LIVE_WORKERS`]).
		let key = registry_key(&config.path);
		if let Some(shared) = join_live_worker(&key) {
			slot.worker = Arc::downgrade(&shared);
			return Ok(shared);
		}
		// The previous worker (if any) is gone or on its way out — its senders are dropped or
		// stale. Wait for it to fully exit and reap it, so the SQLite file is guaranteed closed
		// before the new worker reopens it. (Only THIS `Client`'s previous worker is waited out:
		// another `Client`'s worker that just lost its last handle drops out of the registry
		// before it has finished closing the DB, so a spawn racing that teardown can still
		// briefly overlap it — the same window a single `Client` closed for itself with the slot's
		// exit signal.)
		wait_for_worker_exit(&mut slot).await;
		let shared = spawn_cache_worker(client.clone(), &config, key, &mut slot).await?;
		slot.worker = Arc::downgrade(&shared);
		register_worker(&shared);
		Ok(shared)
	}

	/// Clear the slot's weak worker pointer if it still references `shared`, so the next
	/// [`add_sync_root`](Client::add_sync_root) respawns instead of re-targeting a dead worker.
	/// The pointer comparison keeps a NEWER worker (spawned by a concurrent caller) intact.
	async fn mark_worker_stale(&self, shared: &Arc<CacheWorkerShared>) {
		// Registry first, so a dead worker stops being joinable by other `Client`s too.
		unregister_worker(shared);
		let mut slot = self.cache_slot.lock().await;
		if slot.worker.ptr_eq(&Arc::downgrade(shared)) {
			slot.worker = Weak::new();
		}
	}
}

/// Spawn the cache worker (a `runtime::spawn_async` host: a dedicated thread's current-thread
/// runtime on native, a web worker on wasm) plus its status-bridge task, and register the socket
/// listener. The exit signal is deposited into `slot` IMMEDIATELY after the spawn — before the
/// first await — so even if the caller's future is cancelled mid-spawn, the next add/flush waits
/// the (then channel-disconnected, self-exiting) worker out before touching the DB file. Failure
/// paths likewise just drop the worker's senders and leave the exit-wait to the slot.
async fn spawn_cache_worker(
	client: Arc<Client>,
	config: &CacheConfig,
	db_path: PathBuf,
	slot: &mut CacheSlot,
) -> Result<Arc<CacheWorkerShared>, Error> {
	let (res_sender, res_receiver) = tokio::sync::oneshot::channel();
	let (msg_sender, mut msg_receiver) = tokio::sync::mpsc::channel(100);
	let (finished_sender, finished_receiver) = tokio::sync::watch::channel(false);

	let root_uuid = client.root().uuid();
	let cache_path = config.path.clone();
	// Set when the spawner gives up on this worker (init-ack timeout): a LATE-starting worker
	// (slow worker-script fetch on wasm; a thread unfrozen after a long stall on native) checks
	// it at entry and exits WITHOUT touching the DB — otherwise it could run its whole init
	// concurrently with a freshly spawned successor on the same database (destructive on a
	// version-mismatch wipe; undefined behavior on the single-threaded wasm SQLite build).
	let abandoned = Arc::new(std::sync::atomic::AtomicBool::new(false));
	let abandoned_for_worker = abandoned.clone();
	// The worker owns its own `Arc<Client>` clone; the original stays on this task for
	// `add_event_listener` below. The worker loop is async, so the resync listings are awaited in
	// place on the host runtime — no captured runtime handle, no `block_on`.
	let worker_client = client.clone();
	crate::runtime::spawn_async(move || async move {
		// Declared first so it drops LAST — the exit signal fires only after `CacheState` (and its
		// SQLite connection) is gone, on every exit path including a native panic's unwind.
		let _exit_signal = SignalOnDrop(finished_sender);
		// BEFORE opening the DB: a worker the spawner has already abandoned must not init (see
		// the flag's declaration). Its channel senders just drop, which nobody awaits anymore.
		if abandoned_for_worker.load(Ordering::Acquire) {
			tracing::warn!(
				"cache worker started after its spawner gave up on it; exiting untouched"
			);
			return;
		}
		let state = match CacheState::new(&cache_path, root_uuid, msg_sender, worker_client) {
			Ok((state, callback, control_sender, event_sender, read_task_sender)) => {
				if res_sender
					.send(Ok((
						callback,
						control_sender,
						event_sender,
						read_task_sender,
					)))
					.is_err()
				{
					// The spawning future was dropped (e.g. cancelled) before it received the
					// init result, so nobody is waiting. Exit the worker cleanly instead of
					// panicking.
					tracing::debug!(
						"cache init result receiver dropped before init completed; worker exiting"
					);
					return;
				}
				state
			}
			Err(e) => {
				if res_sender.send(Err(e)).is_err() {
					tracing::debug!(
						"cache init result receiver dropped before init failed; worker exiting"
					);
				}
				return;
			}
		};

		state.run().await;
	});
	slot.finished = Some(finished_receiver);

	// Bridge the worker's status channel to the app callback. The bridge handle is intentionally
	// dropped (detached): the loop ends on its own when the worker drops `msg_sender` — on
	// shutdown or if the worker dies — at which point `recv()` returns `None`.
	let status_callback = config.status_callback.clone();
	drop(crate::runtime::spawn_task_maybe_send(async move {
		while let Some(msg) = msg_receiver.recv().await {
			status_callback(msg);
		}
	}));

	// BOUNDED wait for the init ack. Init is local-only work (DB open + schema), so a silent
	// worker past the deadline is presumed dead — on wasm a worker that never started (or
	// trapped during init: panic=abort leaks the ack sender instead of dropping it) would
	// otherwise hang this await forever WITH THE SLOT LOCK HELD, wedging every cache call for
	// the session. The `abandoned` flag (set in the timeout arm) stops a LATE starter at entry,
	// and one that got past the check exits at its failed ack send — but a straggler frozen
	// MID-init can still briefly overlap a successor; native therefore KEEPS the exit signal
	// (see the timeout arm) so respawns wait stragglers out.
	let (callback, control_sender, manual_event_sender, read_task_sender) = match ack_timeout(
		CACHE_INIT_ACK_TIMEOUT,
		res_receiver,
	)
	.await
	{
		Ok(Ok(Ok(parts))) => parts,
		// `CacheState::new` failed (or the worker died before reporting); the thread is
		// already exiting on its own and stays reapable via the slot.
		Ok(Ok(Err(e))) => return Err(e),
		Ok(Err(_)) => {
			return Err(Error::custom(
				ErrorKind::Internal,
				"cache worker thread exited before initialization completed",
			));
		}
		Err(_) => {
			tracing::error!(
				"cache worker did not acknowledge initialization within {CACHE_INIT_ACK_TIMEOUT:?}; presuming it dead"
			);
			// Tell a late starter to exit at entry instead of initializing onto a DB a
			// successor may be using (stored BEFORE any successor can spawn — the slot lock is
			// still held).
			abandoned.store(true, Ordering::Release);
			// NATIVE: keep the exit signal — a stalled-but-alive worker that got PAST the
			// abandoned check still owns the DB file and always fires the signal eventually
			// (unwind included), so the next add/flush waits it out rather than overlapping
			// it. WASM: a dead worker can never fire the signal (panic=abort leaks it, no
			// unwinding) and a late starter exits at the abandoned check, so clearing it here
			// is what keeps the cache recoverable.
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				slot.finished = None;
			}
			return Err(Error::custom(
				ErrorKind::Internal,
				"cache worker did not initialize in time (worker startup failure?)",
			));
		}
	};

	// Register the socket listener inline via `add_event_listener_sync`, which inserts the callback
	// into the routing table and returns its handle WITHOUT awaiting a connect ack. `add_event_listener`
	// (the ack-awaiting variant) only resolves once the socket connects or auth fails, so awaiting it
	// here — under the cache-slot lock the caller holds — wedges every cache/sync API when the app
	// starts offline. The sync path avoids that wedge while KEEPING the delivery guarantee: on return
	// the callback is live in the table, so every event the socket subsequently receives is delivered
	// to the worker (we subscribe to ALL event types with `None` so it advances its watermark
	// correctly). A registration error still surfaces synchronously and aborts the spawn.
	match client.add_event_listener_sync(callback, None).await {
		Ok(listener_handle) => Ok(Arc::new(CacheWorkerShared {
			db_path,
			control_sender,
			manual_event_sender,
			read_task_sender,
			next_registration_id: AtomicU64::new(0),
			listener_handle: std::sync::Mutex::new(Some(listener_handle)),
		})),
		Err(e) => {
			// Listener registration failed — dropping the just-spawned worker's senders
			// disconnects its control channel, which it treats as a clean shutdown; the slot
			// retains the JoinHandle for the next caller to reap.
			drop(control_sender);
			drop(manual_event_sender);
			Err(e)
		}
	}
}

impl std::fmt::Debug for SyncRootHandle {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("SyncRootHandle")
			.field("uuid", &self.uuid)
			.field("registration_id", &self.registration_id)
			.finish_non_exhaustive()
	}
}

impl SyncRootHandle {
	/// The sync root this handle registers.
	pub fn uuid(&self) -> Uuid {
		self.uuid
	}

	/// A sender for search read queries served by the worker's connection — the wasm read path
	/// (unused on native, where searches open their own connection).
	#[cfg_attr(
		not(all(target_family = "wasm", target_os = "unknown")),
		allow(dead_code)
	)]
	pub(crate) fn read_task_sender(&self) -> UnboundedSender<ReadTask> {
		self.shared.read_task_sender.clone()
	}

	/// Consume the handle, removing its registration AND — when it was the last registration for
	/// this uuid — deleting the root's cached subtree (protecting any still-active nested root).
	/// Returns `Ok(true)` iff the subtree was evicted; `Ok(false)` when other live registrations
	/// keep the root active (eviction is skipped — it would fight the membership gate), or when
	/// the registration was already gone (e.g. the root was deleted server-side).
	pub async fn evict(mut self) -> Result<bool, Error> {
		self.disarmed = true;
		let (ack_sender, ack_receiver) = tokio::sync::oneshot::channel();
		self.shared
			.control_sender
			.send(CacheControlMessage::RemoveRegistration {
				uuid: self.uuid,
				registration_id: self.registration_id,
				evict: true,
				ack: Some(ack_sender),
			})
			.map_err(|_| {
				Error::custom(
					ErrorKind::Internal,
					"cache control channel closed (evict); worker has shut down",
				)
			})?;
		match ack_receiver.await {
			Ok(Ok(evicted)) => Ok(evicted),
			Ok(Err(e)) => Err(Error::custom_with_source(
				ErrorKind::Internal,
				*e,
				Some(format!("evicting sync root {}", self.uuid)),
			)),
			Err(_) => Err(Error::custom(
				ErrorKind::Internal,
				"cache worker exited before acknowledging the eviction",
			)),
		}
	}

	/// Inject a recursive directory listing into the cache.
	///
	/// LEGACY initial-population path: it is upsert-only (it never deletes vanished items) and is
	/// applied WITHOUT watermark gating, so using it as a *live* refresh can resurrect items that
	/// socket events already deleted. Use only for initial population, not as a live refresh.
	///
	/// Despite living on a per-root handle, the injection is ACCOUNT-GLOBAL and unvalidated: the
	/// listed items are upserted regardless of this handle's uuid, membership gating, or whether
	/// the registration is even still live — the handle only provides the channel to the worker.
	pub async fn update_list_dir_recursive(
		&self,
		dirs: Vec<RemoteDirectory>,
		files: Vec<RemoteFile>,
	) -> Result<(), Error> {
		let event = CacheThreadEvent::Manual(ManualEvent::ListDirRecursive(dirs, files));
		// The worker's event channel is BOUNDED (the shed cap): socket events past the cap are
		// shed, but a Manual injection must never be — so this `send` AWAITS capacity instead.
		// It only waits while a 50k-event flood is in flight, but then possibly for a LONG time
		// (the worker frees no capacity while parked in a resync) — never call this from a
		// `SyncRootCallback` or anywhere that gates the worker's own progress. `map_err` drops
		// the (large) un-sent event held by `SendError` so the `Err` stays small.
		self.shared
			.manual_event_sender
			.send(event)
			.await
			.map_err(|_| {
				Error::custom(
					ErrorKind::Internal,
					"Failed to send manual event to cache thread (channel closed)",
				)
			})
	}
}

impl Drop for SyncRootHandle {
	fn drop(&mut self) {
		if self.disarmed {
			return;
		}
		// Best-effort, NON-destructive untrack (`Drop` is sync and must not block; the control
		// channel is unbounded so `send` never blocks). A failed send means the worker already
		// exited (e.g. after `flush_cache`) — nothing left to untrack. If this handle held the LAST
		// strong `Arc<CacheWorkerShared>`, the message is moot anyway: the shared state drops right
		// after, disconnecting the control channel, and the worker shuts down cleanly.
		let _ = self
			.shared
			.control_sender
			.send(CacheControlMessage::RemoveRegistration {
				uuid: self.uuid,
				registration_id: self.registration_id,
				evict: false,
				ack: None,
			});
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	impl CacheWorkerShared {
		/// A worker handle with disconnected channels: the registry only ever stores and compares
		/// IDENTITY, so nothing here needs a real worker behind it.
		fn fake(db_path: PathBuf) -> Arc<Self> {
			let (control_sender, _) = tokio::sync::mpsc::unbounded_channel();
			let (manual_event_sender, _) = tokio::sync::mpsc::channel(1);
			let (read_task_sender, _) = tokio::sync::mpsc::unbounded_channel();
			Arc::new(Self {
				db_path,
				control_sender,
				manual_event_sender,
				read_task_sender,
				next_registration_id: AtomicU64::new(0),
				listener_handle: std::sync::Mutex::new(None),
			})
		}
	}

	/// A unique DB path under the temp dir — these tests share the one process-global registry, so
	/// they must not collide with each other.
	fn temp_db_path(tag: &str) -> PathBuf {
		registry_key(
			&std::env::temp_dir().join(format!("filen-cache-registry-{tag}-{}.db", Uuid::new_v4())),
		)
	}

	#[test]
	fn a_second_client_joins_the_worker_already_on_the_path() {
		let key = temp_db_path("join");
		let first = CacheWorkerShared::fake(key.clone());
		register_worker(&first);
		let joined = join_live_worker(&key).expect("a live worker on this path is joinable");
		assert!(Arc::ptr_eq(&first, &joined));
	}

	#[test]
	fn distinct_paths_get_distinct_workers() {
		let (a, b) = (temp_db_path("distinct-a"), temp_db_path("distinct-b"));
		let (first, second) = (
			CacheWorkerShared::fake(a.clone()),
			CacheWorkerShared::fake(b.clone()),
		);
		register_worker(&first);
		register_worker(&second);
		assert!(Arc::ptr_eq(&first, &join_live_worker(&a).unwrap()));
		assert!(Arc::ptr_eq(&second, &join_live_worker(&b).unwrap()));
		assert!(!Arc::ptr_eq(&first, &second));
	}

	#[test]
	fn dropping_the_last_user_frees_the_path_and_a_reacquisition_is_fresh() {
		let key = temp_db_path("respawn");
		let first = CacheWorkerShared::fake(key.clone());
		register_worker(&first);
		let dead = Arc::downgrade(&first);
		// The last `SyncRootHandle` going away: the worker shuts down.
		drop(first);
		assert!(dead.upgrade().is_none());
		// The dead entry is dropped on the way, not handed back.
		assert!(join_live_worker(&key).is_none());
		assert!(!live_workers().contains_key(&key));
		let second = CacheWorkerShared::fake(key.clone());
		register_worker(&second);
		assert!(Arc::ptr_eq(&second, &join_live_worker(&key).unwrap()));
	}

	#[test]
	fn unregistering_a_stale_worker_keeps_its_replacement() {
		let key = temp_db_path("stale");
		let stale = CacheWorkerShared::fake(key.clone());
		register_worker(&stale);
		// A concurrent caller respawned onto the same path.
		let replacement = CacheWorkerShared::fake(key.clone());
		register_worker(&replacement);
		unregister_worker(&stale);
		assert!(Arc::ptr_eq(&replacement, &join_live_worker(&key).unwrap()));
		unregister_worker(&replacement);
		assert!(join_live_worker(&key).is_none());
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[test]
	fn two_spellings_of_one_database_share_a_key() {
		let dir = std::env::temp_dir();
		assert_eq!(
			registry_key(&dir.join("cache.db")),
			registry_key(&dir.join(".").join("cache.db"))
		);
	}
}
