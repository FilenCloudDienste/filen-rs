use std::{
	collections::HashMap,
	iter::once,
	path::Path,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
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
};
use crossbeam::channel::{Receiver, Sender};
use filen_types::traits::CowHelpers;
use uuid::Uuid;

use crate::cache::{CacheError, handle::CacheMessage};
/// How many durable `events` rows the drain loads and applies per iteration.
const BATCH_SIZE: usize = 256;
/// Bounded-memory backstop for the single UNBOUNDED event channel. The socket router runs on
/// the SDK runtime and cannot block or persist, so it pushes to an unbounded channel; if the worker
/// cannot drain fast enough and the channel reaches this many buffered events, the router SHEDS further
/// events (drops them) and latches a `shed` flag. The worker then records a durable resync, which
/// re-lists and recovers whatever was dropped — so a shed costs a resync, never a silent loss.
/// crossbeam frees consumed blocks, so the channel shrinks back toward empty as the worker drains a
/// burst; this cap only bounds the worst-case PEAK (each event is on the order of a few hundred bytes,
/// so tens of MiB here) — important on the mobile (UniFFI) target.
const EVENT_SHED_CAP: usize = 50_000;

/// Dependencies the worker needs to run a write-locked resync: an owned
/// handle to the SDK [`Client`] and the Tokio runtime [`Handle`](tokio::runtime::Handle) the worker
/// `block_on`s to drive the async listing from its `std::thread`. Captured once at construction
/// ([`Client::add_sync_root`](crate::auth::Client::add_sync_root) spawns the worker from inside the
/// app's runtime, so `Handle::current()` is valid there). `None` under unit-test construction (no live
/// client/runtime); the resync path logs and no-ops when it is absent.
#[derive(Clone)]
pub(crate) struct ResyncDeps {
	pub(crate) client: Arc<Client>,
	pub(crate) rt_handle: tokio::runtime::Handle,
}

pub(crate) struct CacheState {
	pub(crate) db: rusqlite::Connection,
	/// The single UNBOUNDED event channel (Socket events from the callback + Manual events). Unbounded
	/// so the socket router never blocks; its worst-case memory is bounded by [`EVENT_SHED_CAP`] (the
	/// router sheds + the worker resyncs past that), and the worker draining it shrinks it back toward
	/// empty (crossbeam frees consumed blocks).
	event_receiver: Receiver<CacheThreadEvent>,
	control_receiver: Receiver<CacheControlMessage>,
	msg_sender: tokio::sync::mpsc::Sender<Vec<CacheMessage>>,
	/// Shed latch: set by the socket router when the channel hit [`EVENT_SHED_CAP`] and it had
	/// to drop events. The worker observes it once per drain and records a durable resync to recover the
	/// dropped events, then clears it.
	shed: Arc<AtomicBool>,
	/// The account-root uuid — the single `roots` row, used for upsert `?4` (root_id resolution) and DB
	/// init. NOT necessarily a sync root (see `sync_roots`).
	pub(crate) root_uuid: Uuid,
	/// Configured sync roots → their live registrations. An item is cached iff it
	/// descends from one of these roots (the membership gate, in `sql/membership.rs`); EMPTY ⇒
	/// nothing is cached. The production worker starts EMPTY — registrations arrive via
	/// [`CacheControlMessage::AddSyncRoot`], and a uuid stops being a sync root when its LAST
	/// registration is removed. Test constructors default this to `{account_root → no-op}`
	/// (whole-account sync) so existing tests still exercise the machinery.
	sync_roots: HashMap<Uuid, RootRegistrations>,
	/// Client + runtime handle for the write-locked resync island. `None` in unit tests.
	resync: Option<ResyncDeps>,
}

#[cfg(test)]
impl CacheState {
	/// Create a CacheState with an in-memory DB for unit testing.
	pub(crate) fn new_in_memory() -> Self {
		let root_uuid = Uuid::new_v4();
		let (event_sender, event_receiver) = crossbeam::channel::unbounded();
		let (control_sender, control_receiver) = crossbeam::channel::unbounded();
		drop(event_sender);
		drop(control_sender);
		let (msg_sender, msg_receiver) = tokio::sync::mpsc::channel(1);
		drop(msg_receiver);

		let mut state = Self {
			db: rusqlite::Connection::open_in_memory().unwrap(),
			event_receiver,
			control_receiver,
			msg_sender,
			shed: Arc::new(AtomicBool::new(false)),
			root_uuid,
			sync_roots: whole_account_sync_roots(root_uuid),
			resync: None,
		};
		state.init_db().unwrap();
		state
	}

	/// Like [`new_in_memory`](Self::new_in_memory) but on a real file `path` with a caller-chosen
	/// `root_uuid`, so a test can open the SAME DB twice and assert that state survives a reopen (the
	/// app close/resume path). `init_db` only wipes when the `user_version` mismatches, so a second open
	/// with the matching version preserves the data.
	pub(crate) fn new_on_path(path: &Path, root_uuid: Uuid) -> Self {
		let (event_sender, event_receiver) = crossbeam::channel::unbounded();
		let (control_sender, control_receiver) = crossbeam::channel::unbounded();
		drop(event_sender);
		drop(control_sender);
		let (msg_sender, msg_receiver) = tokio::sync::mpsc::channel(1);
		drop(msg_receiver);

		let mut state = Self {
			db: rusqlite::Connection::open(path).unwrap(),
			event_receiver,
			control_receiver,
			msg_sender,
			shed: Arc::new(AtomicBool::new(false)),
			root_uuid,
			sync_roots: whole_account_sync_roots(root_uuid),
			resync: None,
		};
		state.init_db().unwrap();
		state
	}

	/// Like [`new_in_memory`](Self::new_in_memory) but RETAINS the producer side (the event sender, the
	/// control sender, and the shed latch) so a test can flood the worker the way the real callback does
	/// and then drive the drain.
	pub(crate) fn new_in_memory_with_producer() -> (Self, TestProducer) {
		let root_uuid = Uuid::new_v4();
		let (event_sender, event_receiver) = crossbeam::channel::unbounded();
		let (control_sender, control_receiver) = crossbeam::channel::unbounded();
		let (msg_sender, msg_receiver) = tokio::sync::mpsc::channel(100);
		drop(msg_receiver);
		let shed = Arc::new(AtomicBool::new(false));

		let mut state = Self {
			db: rusqlite::Connection::open_in_memory().unwrap(),
			event_receiver,
			control_receiver,
			msg_sender,
			shed: shed.clone(),
			root_uuid,
			sync_roots: whole_account_sync_roots(root_uuid),
			resync: None,
		};
		state.init_db().unwrap();

		let producer = TestProducer {
			events: event_sender,
			control: control_sender,
			shed,
		};
		(state, producer)
	}

	/// Test setter: install `map` as the sync roots, wrapping each callback as that root's single
	/// registration (id 0).
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
	pub(crate) events: Sender<CacheThreadEvent>,
	pub(crate) control: Sender<CacheControlMessage>,
	pub(crate) shed: Arc<AtomicBool>,
}

#[derive(Debug)]
pub(crate) enum ManualEvent {
	/// A directly-injected recursive directory listing (not from the socket). Upsert-only: it adds/
	/// refreshes the listed dirs and files but never deletes, and does not touch the drain watermark.
	ListDirRecursive(Vec<RemoteDirectory>, Vec<RemoteFile>),
}

/// A message delivered to the cache worker thread: either an event derived from the WebSocket
/// (which may have failed to convert into a cacheable form) or a manually-injected event such as
/// a recursive directory listing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum CacheThreadEvent {
	Socket(CacheEventMaybeDecrypted<'static>),
	Manual(ManualEvent),
}

/// A per-sync-root notification callback. Invoked POST-COMMIT on the worker thread with a
/// borrowing iterator over the events applied to that root's subtree, each call wrapped in
/// `catch_unwind`. `Send` so it can be moved to the worker thread; the iterator borrows a local owned
/// `Vec`, never the rusqlite transaction. The cache is consumed only by an in-process Rust intermediary
/// (the search / sync engine, which exposes its OWN FFI), so the borrowing shape needs no marshalling.
pub type SyncRootCallback = Box<dyn Fn(&mut dyn Iterator<Item = &CacheEvent<'_>>) + Send + 'static>;

/// The live registrations for one sync root: `(registration_id, callback)` pairs. Multiple
/// [`SyncRootHandle`](crate::cache::SyncRootHandle)s may target the same uuid; each holds its own
/// registration, every callback is notified on dispatch, and the uuid stops being a sync root only
/// when its last registration is removed.
pub(crate) type RootRegistrations = Vec<(u64, SyncRootCallback)>;

/// Build the default `{account_root → no-op}` sync-root map: whole-account sync with no notification.
/// Used by the test constructors so the existing apply/drain/resync tests keep exercising the machinery
/// (production registrations arrive via [`Client::add_sync_root`](crate::auth::Client::add_sync_root)).
#[cfg(test)]
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
	events: Sender<CacheThreadEvent>,
	shed: Arc<AtomicBool>,
) -> impl Fn(&DecryptedSocketEvent<'_>) + Send + 'static {
	move |event| {
		if let Some(event) = CacheEventMaybeDecrypted::from_decrypted_event(event) {
			route_thread_event(
				CacheThreadEvent::Socket(event.into_owned_cow()),
				&events,
				&shed,
				EVENT_SHED_CAP,
			);
		}
	}
}

/// Route one event onto the worker's single UNBOUNDED event channel, UNLESS it already
/// holds `cap` messages — then SHED it (drop it) and latch `shed`. This runs on the SDK socket
/// runtime, which must not block or touch the DB, so an unbounded non-blocking `send` is the only
/// option; `cap` bounds the worst-case PEAK memory, and a shed is recovered by the resync the worker
/// triggers when it sees the latch. crossbeam frees consumed blocks, so the channel shrinks back toward
/// empty once the worker drains a burst. Because a single FIFO channel preserves order on its own, the
/// cap above is the ONLY backpressure — there is no second "overflow" channel or spill latch to manage.
fn route_thread_event(
	event: CacheThreadEvent,
	events: &Sender<CacheThreadEvent>,
	shed: &AtomicBool,
	cap: usize,
) {
	if events.len() >= cap {
		if !shed.swap(true, Ordering::AcqRel) {
			log::warn!(
				"cache event channel reached its {cap}-event cap; shedding events under sustained load — \
				 a resync will recover the gap"
			);
		}
		// `event` intentionally dropped.
	} else {
		// Unbounded, capped above; `send` never blocks and only errors if the worker has shut down.
		let _ = events.send(event);
	}
}

type InitResult = (
	CacheState,
	Box<dyn Fn(&DecryptedSocketEvent<'_>) + Send + 'static>,
	Sender<CacheControlMessage>,
	Sender<CacheThreadEvent>,
);

impl CacheState {
	pub(crate) fn new(
		cache_path: &Path,
		root_uuid: Uuid,
		msg_sender: tokio::sync::mpsc::Sender<Vec<CacheMessage>>,
		client: Arc<Client>,
		rt_handle: tokio::runtime::Handle,
	) -> Result<InitResult, crate::Error> {
		let connection = rusqlite::Connection::open(cache_path).map_err(|e| {
			crate::Error::custom_with_source(
				ErrorKind::Internal,
				e,
				Some("Failed to open SQLite database"),
			)
		})?;

		let (event_sender, event_receiver) = crossbeam::channel::unbounded();
		let (control_sender, control_receiver) = crossbeam::channel::unbounded();
		let shed = Arc::new(AtomicBool::new(false));

		let mut cache_state = CacheState {
			db: connection,
			event_receiver,
			control_receiver,
			msg_sender,
			shed: shed.clone(),
			root_uuid,
			// Starts EMPTY (nothing cached); registrations arrive via `AddSyncRoot` control messages.
			sync_roots: HashMap::new(),
			resync: Some(ResyncDeps { client, rt_handle }),
		};

		cache_state.init_db().map_err(|e| {
			crate::Error::custom_with_source(
				ErrorKind::Internal,
				e,
				Some("Failed to set up SQLite database"),
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
		))
	}

	pub(crate) fn run(mut self) {
		// Startup / app-resume recovery, in order:
		// 1. Apply anything a prior session persisted to `events` but did not drain (e.g. an abrupt
		//    close) so the watermark reflects it BEFORE the gap-check — this lets a clean local catch-up
		//    avoid a needless network resync.
		// 2. Catch up on changes that landed while the cache was entirely offline (a durably-flagged
		//    hole, or the remote drive id having advanced past our watermark).
		self.drain_pending(None);
		self.maybe_run_startup_resync();
		loop {
			// `select_biased!` checks arms top-down: control (shutdown) first, then the single event
			// channel. A lone FIFO channel needs no spill latch or first-class overflow arm — order is
			// intrinsic, so the previous two-channel TOCTOU dance is gone.
			crossbeam::channel::select_biased! {
				recv(self.control_receiver) -> control_event => {
					// `select_biased!` selects a pending control message ahead of a non-empty event arm.
					// A `Shutdown` — or every control sender having been dropped WITHOUT one (the
					// defensive `Err(_)` case: the last `SyncRootHandle` and the worker references are
					// gone) — is the NORMAL clean-shutdown path. Either way, drain everything currently
					// buffered into the durable store before exiting so it is not lost.
					let shutdown = match control_event {
						Ok(first) => self.process_control_burst(first),
						Err(_) => true,
					};
					if shutdown {
						log::info!("Cache shutting down; draining buffered events first...");
						self.drain_pending(None);
						return;
					}
				},
				recv(self.event_receiver) -> event => {
					let Ok(event) = event else {
						log::warn!("Event channel closed, draining and shutting down cache...");
						self.drain_pending(None); // don't drop buffered events on disconnect
						return;
					};
					self.drain_pending(Some(event));
					// A drain that observed a hole/corrupt row/failed apply set needs_resync; heal it now.
					self.maybe_run_resync();
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
			self.surface_errors(vec![*e]);
			if let Err(e) = self.mark_needs_resync() {
				self.surface_errors(vec![*e]);
			}
		}

		// Apply the ordered, persisted socket events first. The drain records `needs_resync` ATOMICALLY
		// (inside `commit_drain_batch`) when it observes a hole, so there is no separate best-effort flag
		// write here; a hole detected here is healed by the subsequent `maybe_run_resync`
		// in `run`, or by the startup gap-check on the next boot.
		if let Err(e) = self.drain_persisted() {
			self.surface_errors(vec![*e]);
		}

		// if the socket router shed events under sustained over-rate (the channel hit its cap),
		// record a durable resync so the dropped events are recovered — the subsequent `maybe_run_resync`
		// in `run` (or the startup gap-check) re-lists and converges. Consume the latch once; any shed
		// that happens after this re-sets it for the next cycle, so the signal is never lost.
		if self.shed.swap(false, Ordering::AcqRel) {
			log::warn!("cache shed events under load; recording a resync to recover them");
			if let Err(e) = self.mark_needs_resync() {
				// The durable flag write failed — RE-ARM the shed latch so the next drain
				// retries the mark instead of silently losing the resync signal. (The startup gap-check
				// remains a backstop, but it only fires on the next boot.)
				self.shed.store(true, Ordering::Release);
				self.surface_errors(vec![*e]);
			}
		}

		// Then apply deferred Manual events on top of the freshly-applied socket state.
		for manual in deferred {
			if let Err(errors) = self.handle_manual_event(manual) {
				self.surface_errors(errors);
			}
		}
	}

	/// Best-effort surface of cache errors to the main thread.
	// TODO: distinguish a Full status channel (keep running) from a Closed one (main thread
	// gone → shut down). For now errors are surfaced best-effort.
	fn surface_errors(&self, errors: Vec<CacheError>) {
		let _ = self.msg_sender.try_send(vec![CacheMessage::Error(errors)]);
	}

	/// Apply one decoded event to the cache. Every branch must be idempotent (the drain may re-apply
	/// a hole-held / crash-replayed event — see [`CacheState::drain_persisted`]).
	fn apply_event(&mut self, event: CacheEventType<'_>) -> Result<(), Vec<CacheError>> {
		match event {
			CacheEventType::File(file_event) => self.handle_file_event(file_event),
			CacheEventType::Dir(dir_event) => self.handle_dir_event(dir_event),
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
		let mut resync_needed = false;
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
		let mut frontier = watermark;
		let mut frontier_broken = false;

		loop {
			let (batch, corrupt_seqs) = self.load_event_batch(BATCH_SIZE)?;
			if batch.is_empty() && corrupt_seqs.is_empty() {
				break;
			}
			if !corrupt_seqs.is_empty() {
				// A corrupt row is a lost event of (conservatively) unknown id — break the prefix and
				// force a resync rather than risk advancing the watermark past it.
				resync_needed = true;
				frontier_broken = true;
			}

			let frontier_before = frontier;
			let mut to_delete = corrupt_seqs; // corrupt rows are quarantined (deleted) with this batch
			// Per-root dispatch: collect (applied event, owning roots) for delivery AFTER commit.
			// The event is held in an `Arc` so fan-out to multiple owning roots shares one allocation
			// instead of deep-cloning the (string-heavy) payload per root.
			let mut dispatch_buffer: Vec<(Arc<CacheEvent<'static>>, Vec<Uuid>)> = Vec::new();

			for pe in batch {
				to_delete.push(pe.seq);
				// snapshot the pre-delete/pre-move parent BEFORE applying, and keep a clone of the
				// event for the post-commit callback (the original's payload is moved into `apply_event`).
				let pre_parent = self.pre_parent_snapshot(&pe.event.event);
				let event_for_dispatch = Arc::new(pe.event.clone());
				let apply_result = match pe.id {
					// Synthetic diff events (from the resync diff) always apply (no watermark interaction).
					// The apply commits in its own tx, separate from the
					// batched watermark+delete (`commit_drain_batch`). A crash in between re-loads the
					// synthetic and re-applies it — which is SAFE because every synthetic is idempotent
					// (New/Move/Changed = upsert, Removed = delete-of-missing) and the resync watermark
					// jump was already committed by `commit_resync_synthetics`. So the cache is
					// eventually-consistent across a crash; wrapping apply+delete in one tx would only
					// remove the transient re-apply, not fix a divergence. Left as a documented residual.
					None => self.apply_event(pe.event.event),
					// Already applied in a prior (possibly crashed) drain: skip, but still delete.
					Some(id) if watermark.is_some_and(|w| id <= w) => continue,
					Some(id) => {
						let result = self.apply_event(pe.event.event);
						if result.is_ok() {
							// Advance the frontier ONLY along an unbroken, gap-free run from `watermark`.
							// `checked_add` avoids a u64 overflow if a malformed id reached the store.
							if !frontier_broken
								&& frontier.is_none_or(|f| f.checked_add(1) == Some(id))
							{
								frontier = Some(id);
							} else {
								// a hole below `id` (an SDK-dropped event), or a
								// previously-broken prefix — never advance the watermark past it.
								frontier_broken = true;
								resync_needed = true;
							}
						}
						result
					}
				};
				match apply_result {
					Err(errors) => {
						// Quarantine the failing event (already queued for deletion), surface the error,
						// break the prefix, and keep draining — do NOT abort the loop.
						self.surface_errors(errors);
						frontier_broken = true;
						resync_needed = true;
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
			// the drain started from). Watermark-before-delete: the applies already committed in their
			// own transactions; this advances the watermark and deletes the consumed rows together.
			let advanced = match frontier {
				Some(f) if frontier != frontier_before && watermark.is_none_or(|w| f > w) => {
					Some(f)
				}
				_ => None,
			};
			// Record `needs_resync` ATOMICALLY with this batch's watermark advance + row deletes when
			// the contiguous frontier is broken, so the "resync needed" signal can never be lost to a
			// failed best-effort write after the hole's evidence is already deleted. The
			// flag write is idempotent across batches.
			self.commit_drain_batch(advanced, &to_delete, resync_needed)?;

			// POST-COMMIT: notify each sync root's callback of the events that touched its subtree.
			self.dispatch_batch(dispatch_buffer);
		}
		Ok(resync_needed)
	}

	/// Resync core, independent of the async island so it is unit-testable. Given EACH sync
	/// root's freshly-listed subtree (as owned cacheables, paired with that root's anchor uuid) and the
	/// snapshot's drive message id, converge the cache: per root, stage its listing into `diff_incoming`
	/// and compute its diff synthetics (anchored at that root); then atomically persist ALL roots'
	/// synthetics with the advanced watermark + cleared `needs_resync`, and drain so they apply.
	///
	/// The whole thing is crash-idempotent: synthetics are upserts / delete-of-missing, and the
	/// watermark + flag commit atomically with the synthetic rows (see [`commit_resync_synthetics`]).
	/// All roots commit TOGETHER (not per-root) so a crash mid-loop cannot clear `needs_resync` after one
	/// root and leave another un-converged. If the post-commit drain observes a *fresh* gap (a real
	/// buffered event above the snapshot id), `needs_resync` is re-set so the next worker cycle resyncs.
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
		// `per_root` only holds roots that listed successfully. `finalize_resync` already handled the rest:
		// a server-deleted root was dropped, and an all-transient-failure resync returned before reaching
		// here. So an empty `per_root` means an empty config or an all-roots-deleted resync — nothing to
		// diff and no deletes to fear, so the empty-listing guard below must not fire for it.
		let had_listings = !per_root.is_empty();
		let mut all_synthetics = Vec::new();
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
			all_synthetics.extend(synthetics);
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
				log::warn!(
					"resync: every sync root listed EMPTY but the cache holds {cached_non_root} \
					 item(s); converging will delete them. Proceeding — expected only if the drive was \
					 genuinely emptied."
				);
			}
		}

		self.commit_resync_synthetics(&all_synthetics, remote_under_lock, mark_resync)?;

		// Apply the synthetics. If the drain observes a FRESH hole (a real buffered event above the
		// snapshot id broke the frontier), `commit_drain_batch` re-sets `needs_resync` atomically — even
		// though `commit_resync_synthetics` just cleared it — so the next worker cycle resyncs again
		// (no separate best-effort re-mark to lose).
		self.drain_persisted()?;
		Ok(())
	}

	/// Run the write-locked resync, surfacing any error to the main thread.
	fn run_resync_surfacing_errors(&mut self) {
		if let Err(e) = self.run_resync() {
			self.surface_errors(vec![*e]);
		}
	}

	/// Process `first` plus every control message already queued behind it, then run AT MOST ONE
	/// convergence resync if the burst added any new sync root — so a multi-root startup (N
	/// `add_sync_root` calls in quick succession) converges with a single listing pass instead of N.
	/// Returns `true` if a `Shutdown` was encountered (the caller drains and exits; messages queued
	/// behind the Shutdown are intentionally not processed).
	fn process_control_burst(&mut self, first: CacheControlMessage) -> bool {
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
					added_new_root |=
						self.handle_add_sync_root(uuid, registration_id, callback, ack);
				}
				CacheControlMessage::RemoveRegistration {
					uuid,
					registration_id,
					evict,
					ack,
				} => {
					self.handle_remove_registration(uuid, registration_id, evict, ack);
				}
			}
			message = self.control_receiver.try_recv().ok();
		}
		if added_new_root {
			// Durably schedule the convergence FIRST: the adds were already acked Ok, and a
			// transient resync failure returns Ok after only a log — without the flag the new
			// root(s) would silently stay unpopulated for the session (live events for them are
			// membership-gated out while the watermark keeps advancing, so no hole ever re-flags).
			// A fully successful resync clears the flag atomically in the watermark commit.
			if let Err(e) = self.mark_needs_resync() {
				self.surface_errors(vec![*e]);
			}
			// Resync ALL roots so the new root(s) are populated AND the watermark stays accurate for
			// every root. Resyncing only the new roots could advance the watermark past a pending gap
			// in an existing root and clear `needs_resync`, masking it. Redundant listings of
			// already-current roots are accepted in v1; a per-root-bookmark skip is a future
			// optimization.
			log::info!("sync root(s) added; resyncing to populate");
			self.run_resync_surfacing_errors();
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
	fn handle_add_sync_root(
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
			log::info!("AddSyncRoot {uuid}: covered by an active sync root; registering directly");
			self.sync_roots
				.insert(uuid, vec![(registration_id, callback)]);
			let _ = ack.send(Ok(()));
			return false;
		}
		if uuid != self.root_uuid
			&& let Some(deps) = self.resync.clone()
			&& let Err(e) = deps.rt_handle.block_on(deps.client.get_dir((&uuid).into()))
		{
			let error = if matches!(
				e.kind(),
				ErrorKind::FolderNotFound | ErrorKind::FileNotFound
			) {
				log::warn!("AddSyncRoot {uuid}: directory no longer exists ({e}); rejecting");
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
				log::warn!("AddSyncRoot {uuid}: validation failed transiently ({e}); rejecting");
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
	fn handle_remove_registration(
		&mut self,
		uuid: Uuid,
		registration_id: u64,
		evict: bool,
		ack: Option<RemoveRegistrationAck>,
	) {
		let result = self.remove_registration(uuid, registration_id, evict);
		match (ack, result) {
			(Some(ack), result) => {
				let _ = ack.send(result);
			}
			(None, Err(e)) => self.surface_errors(vec![*e]),
			(None, Ok(_)) => {}
		}
	}

	/// Drop one `(uuid, registration_id)` registration. The uuid stays an active sync root while
	/// other registrations remain — `evict` is then SKIPPED too (deleting the subtree out from under a
	/// still-active root would fight the membership gate). Removing the last registration stops
	/// syncing `uuid`, and with `evict` also deletes its cached subtree. An unknown uuid/registration
	/// (e.g. the root was already dropped server-side) is a harmless no-op — a stale handle's Drop
	/// must never error. Returns `Ok(true)` iff the subtree was evicted.
	fn remove_registration(
		&mut self,
		uuid: Uuid,
		registration_id: u64,
		evict: bool,
	) -> Result<bool, Box<CacheError>> {
		let Some(registrations) = self.sync_roots.get_mut(&uuid) else {
			log::warn!("RemoveRegistration: {uuid} is not an active sync root; ignoring");
			return Ok(false);
		};
		let before = registrations.len();
		registrations.retain(|(id, _)| *id != registration_id);
		if registrations.len() == before {
			log::warn!(
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
		self.evict_removed_root(uuid)?;
		Ok(true)
	}

	/// Delete the cached subtree of a JUST-removed sync root (`uuid` must already be out of
	/// `sync_roots`), protecting any still-active nested root.
	fn evict_removed_root(&mut self, uuid: Uuid) -> Result<(), Box<CacheError>> {
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
				self.run_resync_surfacing_errors();
			}
			return Ok(());
		}
		// Protect the remaining active roots (subtrees + ancestor paths) from the eviction + cascade.
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
			log::warn!("sync root {root} was deleted server-side; dropped from the active set");
		}
		// The app MUST learn these roots are gone (it has to re-issue `add_sync_root` to resume them —
		// see `CacheMessage::SyncRootsDeleted`). If the status channel is full the notification is lost
		// and the roots stay silently unsynced until restart, so at least make that visible in the log.
		if self
			.msg_sender
			.try_send(vec![CacheMessage::SyncRootsDeleted(deleted_roots.clone())])
			.is_err()
		{
			log::error!(
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

	/// Add the sync roots that own `uuid` (it, or any ancestor, is a sync-root key) to `owners`.
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
	fn dispatch_batch(&self, dispatch: Vec<(Arc<CacheEvent<'static>>, Vec<Uuid>)>) {
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
					log::error!(
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
	fn maybe_run_resync(&mut self) {
		match self.needs_resync() {
			Ok(true) => {
				log::info!("resync pending (hole flagged during live operation); resyncing");
				self.run_resync_surfacing_errors();
			}
			Ok(false) => {}
			Err(e) => self.surface_errors(vec![*e]),
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
	fn maybe_run_startup_resync(&mut self) {
		let Some(deps) = self.resync.clone() else {
			self.maybe_run_resync();
			return;
		};
		let remote = match deps.rt_handle.block_on(deps.client.get_last_event_ids()) {
			Ok(ids) => ids.drive,
			Err(e) => {
				log::warn!(
					"startup gap-check: could not read the remote drive id ({e}); falling back to \
					 the durable resync flag"
				);
				self.maybe_run_resync();
				return;
			}
		};
		match self.startup_should_resync(remote) {
			Ok(true) => {
				log::info!(
					"startup resync: remote drive id {remote} is ahead of watermark {:?} (or a hole \
					 was flagged); catching up",
					self.watermark().ok().flatten()
				);
				self.run_resync_surfacing_errors();
			}
			Ok(false) => log::debug!(
				"startup: cache is up to date (remote drive id {remote} == watermark); no resync"
			),
			// FAIL OPEN: if the decision itself errors (a `cache_meta` read failed), resync
			// rather than skip — a needless full listing is far cheaper than silently missing a gap.
			Err(e) => {
				log::warn!("startup gap-check failed to read cache state; resyncing to be safe");
				self.surface_errors(vec![*e]);
				self.run_resync_surfacing_errors();
			}
		}
	}

	/// The write-locked resync island: on the worker thread, `block_on` an async listing of
	/// the whole subtree under the drive lock, read the snapshot drive message id under the SAME lock
	/// (so listing + watermark are consistent), then converge the cache via [`apply_resync`].
	///
	/// The `block_on` parks the worker for the listing's duration, but the socket callback keeps pushing
	/// events onto the unbounded channel during that window (the worker persists + applies them when it
	/// resumes), so nothing is lost. On a listing/lock failure the flag is left set and nothing is
	/// committed, so a later cycle retries.
	///
	/// CONTRACT: `list_dir_recursive` is a single `dir/download` of the whole subtree, so
	/// the returned tree is ANCESTOR-CLOSED — every returned item's parent chain up to the sync root is
	/// also returned. The diff passes (orphan sweep, cascade-on-delete) rely on this: an item absent
	/// from the listing cannot have a listed descendant. MEMORY: the whole subtree plus
	/// its synthetic events are held in RAM, like `list_dir_recursive` itself (which documents a >1GiB
	/// footprint for very large trees); bounding this is deferred.
	fn run_resync(&mut self) -> Result<(), Box<CacheError>> {
		let Some(deps) = self.resync.clone() else {
			log::warn!(
				"resync requested but client/runtime deps are absent (test construction?); skipping"
			);
			return Ok(());
		};

		let account_root = self.root_uuid;
		let sync_roots: Vec<Uuid> = self.sync_roots.keys().copied().collect();

		// List EACH sync root's subtree under ONE drive lock, reading the snapshot id under the same lock
		// so every root's listing is consistent at `remote_under_lock`. A subdir root is resolved via
		// `get_dir` (which also yields the node to materialize so the diff has an anchor row); the account
		// root uses its already-materialized `roots` row. (Empty `sync_roots` ⇒ no listings, and
		// `finalize_resync` still advances the watermark + clears the flag so the gap-check does not loop.)
		#[allow(clippy::type_complexity)]
		let listing: Result<_, crate::Error> = deps.rt_handle.block_on(async {
			let _lock = deps.client.lock_drive().await?;
			let remote_under_lock = deps.client.get_last_event_ids().await?.drive;
			let mut per_root_raw: Vec<(
				Uuid,
				Option<RemoteDirectory>,
				Vec<RemoteDirectory>,
				Vec<RemoteFile>,
			)> = Vec::with_capacity(sync_roots.len());
			// Roots the server reported GONE (a definitive not-found): `finalize_resync` deletes their
			// cached subtrees, drops them from the active set, and notifies the app. Kept distinct from a
			// transient skip so a deleted root is removed rather than re-listed (and re-failed) forever.
			let mut deleted_roots: Vec<Uuid> = Vec::new();
			// Set when a root fails with a NON-not-found (network/server) error. `finalize_resync` uses it
			// to keep an all-transient resync from advancing the watermark past a gap it never reconciled.
			let mut any_transient = false;
			for root in &sync_roots {
				let root_node: Option<RemoteDirectory> = if *root == account_root {
					// The account root always exists and resyncs via `client.root()`, not `get_dir`.
					None
				} else {
					match deps.client.get_dir(root.into()).await {
						Ok(node) => Some(node),
						Err(e)
							if matches!(
								e.kind(),
								ErrorKind::FolderNotFound | ErrorKind::FileNotFound
							) =>
						{
							// Gone server-side (deleted while offline, or a cascade we missed). A not-found
							// is definitive, so drop it rather than skip-and-retry.
							log::warn!(
								"resync: sync root {root} no longer exists ({e}); dropping it"
							);
							deleted_roots.push(*root);
							continue;
						}
						Err(e) => {
							// Transient: skip and retry on a later resync (a single unreachable root must
							// not stall the others). The lock + snapshot-id calls above stay fatal.
							log::warn!("resync: skipping sync root {root} (get_dir failed: {e})");
							any_transient = true;
							continue;
						}
					}
				};
				let dir_type: DirType<'_, Normal> = match &root_node {
					Some(node) => node.into(),
					None => deps.client.root().into(),
				};
				let listed = deps
					.client
					.list_dir_recursive::<Normal, fn(u64, Option<u64>)>(&dir_type, None, ())
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
						log::warn!(
							"resync: sync root {root} vanished during listing ({e}); dropping it"
						);
						deleted_roots.push(*root);
					}
					Err(e) => {
						log::warn!("resync: skipping sync root {root} (listing failed: {e})");
						any_transient = true;
					}
				}
			}
			Ok((
				per_root_raw,
				deleted_roots,
				any_transient,
				remote_under_lock,
			))
		});

		let (per_root_raw, deleted_roots, any_transient, remote_under_lock) = match listing {
			Ok(listing) => listing,
			Err(e) => {
				// lock-loss / snapshot-id failure — leave needs_resync set (untouched) and commit
				// nothing, so a later worker cycle retries against a fresh snapshot.
				log::warn!("resync listing failed ({e}); leaving needs_resync set for retry");
				return Ok(());
			}
		};

		self.finalize_resync(
			per_root_raw,
			deleted_roots,
			any_transient,
			remote_under_lock,
		)
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
	#[allow(clippy::type_complexity)]
	fn finalize_resync(
		&mut self,
		per_root_raw: Vec<(
			Uuid,
			Option<RemoteDirectory>,
			Vec<RemoteDirectory>,
			Vec<RemoteFile>,
		)>,
		deleted_roots: Vec<Uuid>,
		any_transient: bool,
		remote_under_lock: u64,
	) -> Result<(), Box<CacheError>> {
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
					if let Err(e) = self.mark_needs_resync() {
						self.surface_errors(vec![*e]);
					}
				}
			}
		}

		// See the doc comment: an all-transient resync must not advance the watermark / clear the flag.
		if per_root_raw.is_empty() && any_transient {
			log::warn!(
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

	fn handle_file_event(&mut self, event: FileEvent) -> Result<(), Vec<CacheError>> {
		match event {
			FileEvent::New(file) | FileEvent::Changed(file) => {
				// skip the upsert for an out-of-root file, but still advance the watermark. (A
				// New/Changed has no prior in-root row to leave stale, unlike a Move — see below.)
				if !self.parent_in_sync_root(file.parent)? {
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
				if self.parent_in_sync_root(file.parent)? {
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

	fn handle_dir_event(&mut self, event: DirEvent) -> Result<(), Vec<CacheError>> {
		match event {
			DirEvent::New(dir) | DirEvent::Changed(dir) => {
				// Skip the upsert for an out-of-root dir, but still advance the watermark. A dir that IS
				// itself a sync root is the exception (same as the Move arm): a root whose own parent is
				// out-of-root must still apply its own New/Changed, else its metadata goes stale.
				if !self.sync_roots.contains_key(&dir.uuid)
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
				if self.sync_roots.contains_key(&dir.uuid)
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

/// Wrap a rusqlite error as the single-element `Vec<CacheError>` the apply handlers return —
/// centralises the `vec![CacheError::db(e, ...)]` pattern that recurs across every event handler.
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

/// Best-effort string for a caught panic payload.
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
