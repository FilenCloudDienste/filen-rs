//! Keeps the working set fresh through the SDK cache engine's file sync roots.
//!
//! The engine owns the only socket consumer worth having (durable event table, drive watermark,
//! resync healing), and since commit "files as sync roots" it can follow a FILE across the uuid
//! re-mint an edit causes — keyed by the lineage's whole-life [`StableUuid`]. This module is the
//! bridge: it registers the working set's files as file roots, and pushes each post-commit
//! notification into `native_cache.db` through the same upsert a server refresh uses, so the
//! change-sequence and tombstone triggers fire exactly as they do for anything else.
//!
//! It runs on the SDK cache DB the live search already uses ([`AuthCacheState::sdk_cache_path`]) —
//! the same [`Client`](filen_sdk_rs::auth::Client) means the same cache slot, so registering here
//! joins search's worker rather than starting a second one.
//!
//! LOCK ORDERING, the one rule this module lives by: the `CacheState` guard is taken for the
//! CHEAP, local work only — a membership query, a set difference, installing handles — and is
//! ALWAYS released before any call into the engine. Tokio's `RwLock` is fair, so a queued
//! auth-refresh writer parks every later reader behind whoever holds the read guard; holding one
//! across an engine round trip (a registration ack, a lineage listing) would stall the whole
//! native cache for its duration, `invalidate()` included.

use std::{
	collections::{HashMap, HashSet},
	path::PathBuf,
	sync::{Arc, Mutex, Weak},
};

use filen_sdk_rs::{
	auth::Client,
	cache::{CacheEvent, CacheEventType, FileEvent, SyncRootCallback, SyncRootHandle},
	fs::file::{RemoteFile, meta::FileMeta},
};
use filen_types::{
	fs::{ParentUuid, StableUuid, Uuid},
	traits::{CowHelpers, CowHelpersExt},
};
use rusqlite::{Connection, OptionalExtension};
use tokio::sync::{
	RwLock,
	mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};

use crate::{
	CacheError,
	auth::{AuthCacheState, AuthStatus, CacheState, FilenMobileCacheState},
	sql::{
		self,
		error::OptionalExtensionSQL,
		file::DBFile,
		object::{DBNonRootObject, DBObject},
	},
	traits::WorkingSetUpdateListener,
};

/// One post-commit notification from the engine, deep-copied out of the worker's own buffer.
type Batch = Vec<CacheEvent<'static>>;

/// Everything tracking owns: the live file-root registrations (dropping a handle unregisters it
/// non-destructively) and the single ordered delivery pipeline they all share.
pub(crate) type TrackedFiles = Mutex<Tracking>;

#[derive(Default)]
pub(crate) struct Tracking {
	handles: HashMap<StableUuid, SyncRootHandle>,
	/// `(sender, drainer)`, created with the FIRST registration. ONE channel and ONE consumer for
	/// every tracked lineage — that is what makes the deliveries ordered.
	pipeline: Option<(UnboundedSender<Batch>, tokio::task::JoinHandle<()>)>,
}

impl Tracking {
	/// The sender every callback sends into, spawning the drainer on first use.
	fn sender(&mut self, state: &Weak<RwLock<CacheState>>) -> UnboundedSender<Batch> {
		let (sender, _) = self.pipeline.get_or_insert_with(|| {
			let (sender, receiver) = unbounded_channel();
			let drainer =
				crate::env::get_runtime().spawn(drain_tracked_events(state.clone(), receiver));
			(sender, drainer)
		});
		sender.clone()
	}

	/// Stop tracking: drop every registration and the pipeline with them. Never awaits — handle
	/// `Drop` queues its removal on the engine's control channel without waiting for an ack — so
	/// this is safe to call from a teardown path that must not block.
	fn stop(&mut self) {
		self.handles.clear();
		if let Some((_, drainer)) = self.pipeline.take() {
			drainer.abort();
		}
	}
}

impl Drop for Tracking {
	/// Deauth drops the whole [`AuthCacheState`], and the pipeline belongs to it.
	fn drop(&mut self) {
		if let Some((_, drainer)) = self.pipeline.take() {
			drainer.abort();
		}
	}
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	mutex
		.lock()
		.unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the working set says should be tracked, against what currently is: `(to register, to
/// drop)`.
///
/// FILES ONLY. A favourited DIRECTORY is a working-set member too, but tracking it would mean a
/// dir sync root, whose subtree — every child, none of which the device has a stake in — the
/// engine would then cache and keep converged. Dirs stay reconciled-when-presented in v1.
fn tracking_plan(
	conn: &Connection,
	registered: &HashSet<StableUuid>,
) -> Result<(Vec<StableUuid>, Vec<StableUuid>), CacheError> {
	let desired: HashSet<StableUuid> = sql::select_working_set(conn)?
		.into_iter()
		.filter_map(|obj| match obj {
			DBNonRootObject::File(file) => Some(file.stable_uuid),
			DBNonRootObject::Dir(_) => None,
		})
		.collect();
	Ok((
		desired.difference(registered).copied().collect(),
		registered.difference(&desired).copied().collect(),
	))
}

/// Reconcile tracking off the caller's path. Fire-and-forget: over-calling is cheap (one query
/// plus a set difference when nothing moved) and a failure only means the next call retries.
pub(crate) fn schedule_refresh(state: &Arc<RwLock<CacheState>>) {
	let state = state.clone();
	crate::env::get_runtime().spawn(async move {
		let weak = Arc::downgrade(&state);
		// Phase 1, GUARDED and local-only.
		let planned = {
			let guard = state.read().await;
			let AuthStatus::Authenticated(auth) = &guard.status else {
				return;
			};
			auth.plan_working_set_tracking(&weak)
		};
		// Phase 2 runs with the guard RELEASED — see the module's lock-ordering rule.
		let result = match planned {
			Ok(plan) => register_planned(&state, plan).await,
			Err(e) => Err(e),
		};
		if let Err(e) = result {
			tracing::warn!("working-set tracking refresh failed: {e}");
		}
	});
}

/// What phase 1 decided, and everything phase 2 needs to act on it without the guard.
pub(crate) struct TrackingPlan {
	to_add: Vec<StableUuid>,
	/// Departed lineages, carrying their live handles out of the map so phase 2 can EVICT them —
	/// an engine round trip phase 1 must not await. Leaving the working set is a statement about
	/// the DATA: the engine row goes with the registration, or search would keep serving a frozen
	/// copy nothing updates. (Stopping tracking is a statement about the PROCESS — those paths
	/// still plain-drop, which unregisters without deleting and never awaits.)
	to_evict: Vec<(StableUuid, SyncRootHandle)>,
	client: Arc<Client>,
	sdk_cache_path: PathBuf,
	sender: UnboundedSender<Batch>,
}

/// Phase 2: the engine round trips, with NO `CacheState` guard held, then ONE brief re-acquire to
/// install the handles. Between the two the state can have been re-authenticated — a handle
/// registered against the old client belongs to nobody, so it is dropped (which unregisters it)
/// rather than filed under the new one.
async fn register_planned(
	state: &Arc<RwLock<CacheState>>,
	plan: TrackingPlan,
) -> Result<(), CacheError> {
	// Departures first: each evict rides the live worker's control channel through the handle
	// itself, so no configuration is needed. Best-effort — the engine skips the eviction when a
	// dir root still covers the file or another registration keeps the lineage live, and a
	// failure just leaves the row the way every departure left it before evictions existed.
	// Concurrent for the same reason the adds below are: one control burst, not one per ack.
	for (stable, result) in futures::future::join_all(
		plan.to_evict
			.into_iter()
			.map(|(stable, handle)| async move { (stable, handle.evict().await) }),
	)
	.await
	{
		match result {
			Ok(evicted) => {
				tracing::debug!(
					?stable,
					evicted,
					"departed working-set lineage unregistered"
				);
			}
			Err(e) => {
				tracing::warn!(?stable, "evicting a departed lineage failed: {e}");
			}
		}
	}
	if plan.to_add.is_empty() {
		return Ok(());
	}
	// Exactly what `query_search` does, and for the same reason: configuring is only refused
	// while a worker is already live, which is the case where it was configured already.
	let _ = plan
		.client
		.configure_cache(plan.sdk_cache_path, |messages| {
			tracing::debug!(?messages, "sdk cache status");
		})
		.await;
	// Registered CONCURRENTLY, and deliberately uncapped: these are in-process control-channel
	// sends plus oneshot acks, not HTTP — the network fan-out they trigger is already capped
	// downstream (`fetch_file_root_heads`). Concurrency here is not about latency: the worker
	// acks each add inside the handler and runs AT MOST ONE resync per control BURST, and each
	// resync fetches the head of EVERY registered file root. Awaiting each ack lands every add
	// in its own burst — a cold start's N registrations then cost N resyncs and O(N²) head
	// fetches. Sent together they drain as one burst and converge in a single pass (two at
	// worst: a straggler aborts an in-flight resync's lock wait and joins the next).
	let results = futures::future::join_all(plan.to_add.into_iter().map(|stable| {
		let client = plan.client.clone();
		let callback = tracked_file_callback(plan.sender.clone());
		async move { (stable, client.add_file_sync_root(stable, callback).await) }
	}))
	.await;
	let mut registered = Vec::with_capacity(results.len());
	let mut failure = None;
	for (stable, result) in results {
		match result {
			Ok(handle) => registered.push((stable, handle)),
			// Install everything that registered before reporting the first failure: the
			// missing ones are the next refresh's job.
			Err(e) => failure = failure.or(Some(e)),
		}
	}
	{
		let guard = state.read().await;
		if let AuthStatus::Authenticated(auth) = &guard.status
			&& Arc::ptr_eq(&auth.client, &plan.client)
		{
			let mut tracked = lock(&auth.tracked_files);
			for (stable, handle) in registered {
				// A concurrent refresh may have registered the same lineage first; keep its handle
				// and let ours drop, which removes only its own registration.
				tracked.handles.entry(stable).or_insert(handle);
			}
		}
	}
	failure.map_or(Ok(()), |e| Err(e.into()))
}

impl AuthCacheState {
	/// Phase 1: what the working set says should be tracked against what is, plus the handful of
	/// clones phase 2 needs. Everything here is local and cheap — a single query and a set
	/// difference — which is the whole reason it is the only part that runs under the guard.
	pub(crate) fn plan_working_set_tracking(
		&self,
		state: &Weak<RwLock<CacheState>>,
	) -> Result<TrackingPlan, CacheError> {
		let (to_add, to_evict, sender) = {
			let mut tracked = lock(&self.tracked_files);
			let registered = tracked.handles.keys().copied().collect();
			let (to_add, to_remove) = tracking_plan(&self.conn(), &registered)?;
			// Moving the handles out is local and cheap; the eviction they are owed is an
			// engine round trip, so it rides the plan into phase 2.
			let to_evict = to_remove
				.into_iter()
				.filter_map(|stable| tracked.handles.remove(&stable).map(|h| (stable, h)))
				.collect::<Vec<_>>();
			let sender = tracked.sender(state);
			(to_add, to_evict, sender)
		};
		tracing::debug!(
			?to_add,
			to_evict = ?to_evict.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
			"working-set tracking plan"
		);
		Ok(TrackingPlan {
			to_add,
			to_evict,
			client: self.client.clone(),
			sdk_cache_path: self.sdk_cache_path.clone(),
			sender,
		})
	}

	/// Put a tracked lineage's fresh state into the cache. Reports whether anything landed, which
	/// is what the working-set listener is told about.
	async fn apply_tracked_events(&self, events: Vec<CacheEvent<'static>>) -> bool {
		let mut applied = false;
		for event in events {
			let CacheEventType::File(file_event) = event.event else {
				// Dir events never reach a file root, and the account-global `DeleteAll` is not
				// something this cache acts on — a refresh reconciles it.
				continue;
			};
			applied |= match file_event {
				// The upsert `update_and_query_item`'s refresh uses, so the change-seq and
				// tombstone triggers fire as they do for any other refresh. It also IS the
				// identity update for a versioning-disabled edit: the record carries the lineage
				// id, which resolves onto the existing row and re-files it under the new uuid in
				// place, keeping its local data.
				FileEvent::New(file) | FileEvent::Changed(file) | FileEvent::Move(file) => {
					self.upsert_tracked(file.into())
				}
				// The other half of that edit — a trash on a versioning-disabled account, an
				// archive otherwise. The successor arrives as its own `fileNew` (in no guaranteed
				// order) and the arm above applies it; dropping the row here would take the local
				// bytes and the pending-upload marker with it.
				FileEvent::Trashed {
					new_uuid: Some(_), ..
				}
				| FileEvent::Archived {
					new_uuid: Some(_), ..
				} => false,
				// A remote TRASH is a trash, not a delete: the row stays, marked trashed with its
				// original parent, and its local bytes stay with it. That is this cache's own trash
				// model — the feed reports an updated item the provider files under
				// `.trashContainer`, and a restore is one more update, where a forget would have
				// been a tombstone plus a re-download.
				FileEvent::Trashed { uuid, .. } => self.trash_tracked(uuid),
				// Gone from the drive for good — a permanent delete, or the archive of a lineage
				// that was REPLACED (no successor). The path a refresh takes when the server no
				// longer has the item: bytes first, then the row, and never over an edit that has
				// not gone out.
				FileEvent::Removed(uuid) | FileEvent::Archived { uuid, .. } => {
					self.forget_tracked(uuid).await
				}
				// A rename. No record comes with it, so the meta is patched onto the row we hold.
				FileEvent::MetadataChanged { uuid, meta } => {
					let held = { DBFile::select(&self.conn(), uuid).optional() };
					match held {
						Ok(Some(file)) => match RemoteFile::try_from(file) {
							Ok(mut remote) => {
								remote.meta = FileMeta::Decoded(meta.into_owned_cow());
								self.upsert_tracked(remote)
							}
							Err(e) => {
								tracing::warn!("tracked file {uuid} is not convertible: {e}");
								false
							}
						},
						// Not a row we hold (e.g. an edit re-minted the uuid and the successor has
						// not landed here yet) — the next refresh of the item carries the name.
						Ok(None) => false,
						Err(e) => {
							tracing::warn!("failed to read tracked file {uuid}: {e}");
							false
						}
					}
				}
			};
		}
		applied
	}

	fn upsert_tracked(&self, file: RemoteFile) -> bool {
		match DBFile::upsert_from_remote(&mut self.conn(), file) {
			Ok(_) => true,
			Err(e) => {
				tracing::error!("failed to apply a tracked file: {e}");
				false
			}
		}
	}

	/// Mark a tracked file's row trashed, keeping the original parent (where a restore puts it
	/// back) and the local bytes. Goes through the same `upsert_from_remote` every other apply
	/// uses — `ParentUuid::Trash` is exactly how a trash listing delivers this — so the change-seq
	/// trigger fires and no tombstone does.
	fn trash_tracked(&self, uuid: Uuid) -> bool {
		let held = { DBFile::select(&self.conn(), uuid).optional() };
		match held {
			Ok(Some(file)) => match RemoteFile::try_from(file) {
				Ok(mut remote) => match remote.parent {
					// Already trashed: the row is where it belongs.
					ParentUuid::Trash(_) => false,
					ParentUuid::Uuid(parent) => {
						remote.parent = ParentUuid::Trash(parent);
						self.upsert_tracked(remote)
					}
					// A row parented to a virtual container has no original parent to restore to;
					// leave it for the next refresh rather than invent one.
					other => {
						tracing::warn!("tracked file {uuid} has no restorable parent ({other:?})");
						false
					}
				},
				Err(e) => {
					tracing::warn!("tracked file {uuid} is not convertible: {e}");
					false
				}
			},
			Ok(None) => false,
			Err(e) => {
				tracing::warn!("failed to read tracked file {uuid}: {e}");
				false
			}
		}
	}

	async fn forget_tracked(&self, uuid: Uuid) -> bool {
		let held = { DBObject::select(&self.conn(), uuid).optional() };
		match held {
			Ok(Some(obj)) => match self.forget_item(obj).await {
				Ok(_) => true,
				Err(e) => {
					tracing::error!("failed to drop tracked file {uuid}: {e}");
					false
				}
			},
			Ok(None) => false,
			Err(e) => {
				tracing::warn!("failed to read tracked file {uuid}: {e}");
				false
			}
		}
	}
}

/// The engine's post-commit notification for one tracked lineage.
///
/// It runs ON THE ENGINE'S WORKER THREAD, the same one draining socket events, and must neither
/// block nor reach for this cache's connection — so all it does is copy the batch out (the
/// iterator borrows the worker's own) and hand it to the drainer. Nothing here can block the
/// worker beyond that copy, and the send cannot fail in a way that matters: an absent receiver
/// means tracking stopped, where dropping the batch is exactly right.
fn tracked_file_callback(sender: UnboundedSender<Batch>) -> SyncRootCallback {
	Box::new(move |events| {
		let _ = sender.send(events.map(|event| event.to_owned_cow()).collect());
	})
}

/// The ONE consumer of that channel: every tracked lineage's batches are applied strictly in the
/// order the engine's worker sent them.
///
/// This is why it exists: a task spawned per callback applies in whatever order the scheduler
/// picks, so an edit's `fileNew` could land BEFORE the trash that retired its predecessor — the
/// pair converges either way in the engine, which sequences them, but here the second delivery
/// would then undo the first. One receiver, one task, no reordering possible.
///
/// The state is held WEAKLY: the registrations this drainer serves live inside it.
async fn drain_tracked_events(
	state: Weak<RwLock<CacheState>>,
	mut batches: UnboundedReceiver<Batch>,
) {
	while let Some(batch) = batches.recv().await {
		let Some(state) = state.upgrade() else {
			return;
		};
		let guard = state.read().await;
		let AuthStatus::Authenticated(auth) = &guard.status else {
			continue;
		};
		if auth.apply_tracked_events(batch).await {
			guard.notify_working_set();
		}
	}
}

impl CacheState {
	/// Tell the replica something in its working set moved. Bounced off the runtime like the
	/// search update callback, so a slow consumer cannot hold up the apply that woke it.
	fn notify_working_set(&self) {
		let listener = lock(&self.working_set_listener).clone();
		if let Some(listener) = listener {
			tokio::task::spawn_blocking(move || listener.working_set_changed());
		}
	}
}

#[filen_macros::create_uniffi_wrapper]
impl FilenMobileCacheState {
	/// Bring tracking in line with the working set: register the files that joined it, drop the
	/// ones that left.
	///
	/// Idempotent and cheap when nothing moved, so call it liberally — after serving a change
	/// diff, and after anything that can change membership (a download, an edit, a favourite).
	/// Registering a lineage the server cannot resolve is not an error either: a transient failure
	/// leaves it uncached until a later resync, and a definitive not-found is reaped — which
	/// arrives here as a removal, retires the row, and takes the lineage out of the working set,
	/// so the next call drops the registration with it.
	pub async fn refresh_working_set_tracking(&self) -> Result<(), CacheError> {
		let state = self.state.clone();
		self.async_execute_authed_owned(async move |auth_state| {
			let plan = auth_state.plan_working_set_tracking(&Arc::downgrade(&state))?;
			// The guard goes BEFORE the first engine await, never after — the module's
			// lock-ordering rule, and the reason phase 1 is kept to local work.
			drop(auth_state);
			register_planned(&state, plan).await
		})
		.await
	}
}

#[uniffi::export]
impl FilenMobileCacheState {
	/// Registers who to tell when working-set tracking has changed something — the replica's cue
	/// to ask for a diff (`signalEnumerator(.workingSet)` on iOS). `None` clears it.
	///
	/// Outlives auth changes, unlike the registrations themselves: a listener set at startup keeps
	/// working after a re-auth without the caller having to notice.
	pub fn set_working_set_listener(&self, listener: Option<Arc<dyn WorkingSetUpdateListener>>) {
		let state = self.sync_get_cache_state_borrowed();
		*lock(&state.working_set_listener) = listener;
	}

	/// Stops tracking every lineage. For teardown (the provider's `invalidate`) — the next
	/// [`refresh_working_set_tracking`](FilenMobileCacheState::refresh_working_set_tracking)
	/// rebuilds the whole set from the database, so nothing is lost by calling it.
	///
	/// Never awaits anything: dropping the registrations queues their removal on the engine's
	/// control channel without waiting for an ack, and the drainer is aborted rather than joined.
	/// `invalidate()` is a synchronous teardown the system does not wait politely for.
	pub fn stop_working_set_tracking(&self) {
		let state = self.sync_get_cache_state_borrowed();
		if let AuthStatus::Authenticated(auth_state) = &state.status {
			lock(&auth_state.tracked_files).stop();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use filen_types::fs::Uuid;

	fn db() -> Connection {
		let conn = Connection::open_in_memory().unwrap();
		crate::auth::configure_conn(&conn).unwrap();
		conn.execute_batch(sql::statements::INIT).unwrap();
		add_dir(&conn, uuid(9), 0);
		conn
	}

	fn uuid(byte: u8) -> Uuid {
		Uuid::from_bytes([byte; 16])
	}

	fn stable(byte: u8) -> StableUuid {
		StableUuid::new_for_test(uuid(byte))
	}

	/// A file row as a listing leaves it. `materialised_at`/`pending_upload_at`/`favorite_rank`
	/// are the three stakes that put it in the working set.
	fn add_file(
		conn: &Connection,
		uuid_: Uuid,
		stable_: StableUuid,
		materialised: bool,
		favorite_rank: i64,
	) {
		conn.execute(
			"INSERT INTO items (uuid, stable_uuid, parent, type, materialised_at)
			VALUES (?1, ?2, ?3, 2, ?4);",
			rusqlite::params![
				uuid_,
				Uuid::from(stable_),
				uuid(9),
				materialised.then_some(1_i64)
			],
		)
		.unwrap();
		conn.execute(
			"INSERT INTO files (id, size, chunks, favorite_rank, region, bucket, timestamp,
				metadata_state, raw_metadata)
			VALUES (last_insert_rowid(), 1, 1, ?1, 'de-1', 'b', 1, 2, 'encrypted');",
			rusqlite::params![favorite_rank],
		)
		.unwrap();
	}

	fn add_dir(conn: &Connection, uuid_: Uuid, favorite_rank: i64) {
		conn.execute(
			"INSERT INTO items (uuid, parent, type) VALUES (?1, ?2, 1);",
			rusqlite::params![uuid_, uuid(9)],
		)
		.unwrap();
		conn.execute(
			"INSERT INTO dirs (id, favorite_rank, color, timestamp, metadata_state, raw_metadata)
			VALUES (last_insert_rowid(), ?1, 'default', 1, 2, 'encrypted');",
			rusqlite::params![favorite_rank],
		)
		.unwrap();
	}

	/// Membership drives registration: a file with a stake is tracked, one without is not — and a
	/// favourited DIRECTORY is a working-set member that is deliberately never tracked.
	#[test]
	fn the_plan_registers_the_working_sets_files_and_nothing_else() {
		let conn = db();
		add_file(&conn, uuid(1), stable(2), true, 0);
		add_file(&conn, uuid(3), stable(4), false, 0);
		add_dir(&conn, uuid(5), 1);

		let (to_add, to_remove) = tracking_plan(&conn, &HashSet::new()).unwrap();
		assert_eq!(to_add, vec![stable(2)]);
		assert!(to_remove.is_empty());
	}

	/// The refresh is called after anything that might have moved the set, so the common case is
	/// that nothing did: it has to come out empty rather than re-registering what is registered.
	#[test]
	fn a_second_plan_over_an_unchanged_set_asks_for_nothing() {
		let conn = db();
		add_file(&conn, uuid(1), stable(2), true, 0);

		let (to_add, _) = tracking_plan(&conn, &HashSet::new()).unwrap();
		let registered: HashSet<StableUuid> = to_add.into_iter().collect();

		let (to_add, to_remove) = tracking_plan(&conn, &registered).unwrap();
		assert!(to_add.is_empty() && to_remove.is_empty());
	}

	/// The other direction: the stake goes away (the bytes were evicted, the favourite cleared),
	/// so the registration has to go with it.
	#[test]
	fn a_lineage_that_left_the_set_is_dropped() {
		let conn = db();
		add_file(&conn, uuid(1), stable(2), false, 0);

		let registered = HashSet::from([stable(2)]);
		let (to_add, to_remove) = tracking_plan(&conn, &registered).unwrap();
		assert!(to_add.is_empty());
		assert_eq!(to_remove, vec![stable(2)]);
	}

	/// A favourite is a stake of its own, with nothing cached.
	#[test]
	fn a_favourited_file_is_tracked_without_local_bytes() {
		let conn = db();
		add_file(&conn, uuid(1), stable(2), false, 1);

		let (to_add, _) = tracking_plan(&conn, &HashSet::new()).unwrap();
		assert_eq!(to_add, vec![stable(2)]);
	}
}
