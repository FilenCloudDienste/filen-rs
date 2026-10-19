//! The live path: an always-on socket subscription feeding `native_cache.db` directly.
//!
//! An `NSFileProviderReplicatedExtension` never re-enumerates a MATERIALIZED container on its own
//! — the working set is the only channel by which remote changes to such a container reach the
//! system, and the working set only moves when something writes this database. This module is
//! that writer: it subscribes to the drive's socket events **unconditionally at auth** (no
//! engine, no sync roots, no registration dance), applies each event through the same upserts a
//! listing refresh uses — so the change-sequence and tombstone triggers fire exactly as they do
//! for anything else — and pokes the working-set listener, whose platform side answers with
//! `signalEnumerator(.workingSet)`.
//!
//! One channel, one drainer: the socket thread's callback only deep-copies the event and sends
//! it; a single consumer applies strictly in delivery order, so an edit's `fileNew` can never be
//! undone by the trash event that retired its predecessor being applied after it.
//!
//! The relevance gate keeps this O(1) per event and stops the cache growing on its own: an event
//! is applied only when the cache already holds the row it names OR that row's parent. Anything
//! else is a container this device has never listed — dropped, because that folder is listed on
//! first browse anyway.
//!
//! LOCK DISCIPLINE, inherited from the tracking module this replaces: tokio's `RwLock` is fair,
//! so a queued auth-refresh writer parks every later reader behind whoever holds a read guard.
//! The drainer takes the guard PER EVENT, and the common (local-only) arms hold it across
//! nothing but SQL. The one arm that reaches the network (`trashEmpty`'s trash relist) holds it
//! the way every FFI operation already does — for its own duration only.

use std::{
	borrow::Cow,
	collections::HashSet,
	sync::{
		Arc, Mutex, Weak,
		atomic::{AtomicBool, Ordering},
	},
};

use chrono::{DateTime, Utc};

use filen_sdk_rs::{
	fs::{
		HasParent, HasUUID,
		categories::NonRootItemType,
		dir::RemoteDirectory,
		file::{RemoteFile, meta::FileMeta},
	},
	socket::{DecryptedDriveEvent, DecryptedSocketEvent, ListenerHandle},
};
use filen_types::{
	fs::{ParentUuid, StableUuid, Uuid, UuidStr},
	traits::{CowHelpers, CowHelpersExt},
};
use futures::StreamExt;
use rusqlite::{Connection, OptionalExtension};
use tokio::sync::{
	RwLock,
	mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};

use crate::{
	CacheError,
	auth::{AuthCacheState, AuthStatus, CacheState, FilenMobileCacheState},
	env::BackgroundTasks,
	replay::{self, ReplayOutcome},
	sql::{
		dir::DBDir, error::OptionalExtensionSQL, file::DBFile, item::RawDBItem, object::DBObject,
	},
	traits::WorkingSetUpdateListener,
};

/// One deep-copied socket event, on its way from the socket thread to the drainer.
type OwnedEvent = DecryptedSocketEvent<'static>;

/// Everything the live path owns, hanging off [`AuthCacheState`] so deauth tears it down:
/// dropping the listener handle unsubscribes, dropping the state aborts the drainer.
#[derive(Default)]
pub(crate) struct LiveState {
	/// Claimed exactly once per authenticated state; a failed start releases it so a later call
	/// retries.
	started: AtomicBool,
	handle: Mutex<Option<LiveHandle>>,
}

struct LiveHandle {
	/// Held for its `Drop`: it unregisters the callback from the socket's routing table.
	_listener: ListenerHandle,
	drainer: tokio::task::AbortHandle,
}

impl Drop for LiveState {
	fn drop(&mut self) {
		if let Some(handle) = lock(&self.handle).take() {
			handle.drainer.abort();
		}
	}
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	mutex
		.lock()
		.unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn conn(mutex: &Mutex<Connection>) -> std::sync::MutexGuard<'_, Connection> {
	lock(mutex)
}

/// The dispatch-side names of every event the drainer consumes. Registering the list (rather
/// than a global listener) keeps the socket layer from decrypting chat/note traffic nobody here
/// reads. `authSuccess`/`reconnecting` are the connection-state cues the gap path keys on.
fn live_event_types() -> Vec<Cow<'static, str>> {
	[
		"fileArchiveRestored",
		"fileNew",
		"fileRestore",
		"fileMove",
		"fileTrash",
		"fileArchived",
		"folderTrash",
		"folderMove",
		"folderSubCreated",
		"folderRestore",
		"folderColorChanged",
		"trashEmpty",
		"itemFavorite",
		"fileDeletedPermanent",
		"folderMetadataChanged",
		"folderDeletedPermanent",
		"fileMetadataChanged",
		"deleteAll",
		"deleteVersioned",
		"driveMalformed",
		"authSuccess",
		"reconnecting",
	]
	.into_iter()
	.map(Cow::Borrowed)
	.collect()
}

/// Brings the live path up for the current authenticated state, if it is not up already.
/// Idempotent and cheap when it is (one read guard, one atomic swap), so it is called from every
/// path that (re)establishes auth. Fire-and-forget: a failure only means the next call retries.
/// The start and the drainer it spawns are filed under `tasks`, so dropping or shutting down
/// the state stops them.
pub(crate) fn ensure_started(state: &Arc<RwLock<CacheState>>, tasks: &BackgroundTasks) {
	// Fast path, called from every authenticated FFI entry: no task and no queueing when the
	// live path is already up (or the state is not authenticated). `try_read` failing means a
	// writer is active — fall through to the task, which waits its turn.
	if let Ok(guard) = state.try_read() {
		match &guard.status {
			AuthStatus::Authenticated(auth) if !auth.live.started.load(Ordering::Acquire) => {}
			_ => return,
		}
	}
	let state = state.clone();
	let drainer_tasks = tasks.clone();
	tasks.spawn(async move {
		// Phase 1, GUARDED and local-only: check auth, claim the start, clone the client.
		let client = {
			let guard = state.read().await;
			let AuthStatus::Authenticated(auth) = &guard.status else {
				return;
			};
			if auth.live.started.swap(true, Ordering::AcqRel) {
				return;
			}
			auth.client.clone()
		};

		// Phase 2 with the guard RELEASED (see the module's lock discipline). Subscribe FIRST —
		// before any gap check the launch path runs — so nothing can land between a check and
		// the subscription; `add_event_listener_sync` registers the callback synchronously even
		// while the socket is still connecting, so no event past this point is missed.
		let (sender, receiver) = unbounded_channel();
		let listener = match client
			.add_event_listener_sync(live_callback(sender), Some(live_event_types()))
			.await
		{
			Ok(listener) => listener,
			Err(e) => {
				tracing::warn!("live socket subscription failed: {e}");
				// Release the claim so a later auth-path call retries the subscription.
				let guard = state.read().await;
				if let AuthStatus::Authenticated(auth) = &guard.status
					&& Arc::ptr_eq(&auth.client, &client)
				{
					auth.live.started.store(false, Ordering::Release);
				}
				return;
			}
		};
		let drainer = drainer_tasks.spawn(drain_live_events(Arc::downgrade(&state), receiver));

		// One brief re-acquire to install the handle. A re-auth may have replaced the state in
		// between — a subscription on the old client belongs to nobody, so it is dropped (which
		// unregisters it) and the drainer aborted rather than filed under the new state.
		let guard = state.read().await;
		match &guard.status {
			AuthStatus::Authenticated(auth) if Arc::ptr_eq(&auth.client, &client) => {
				*lock(&auth.live.handle) = Some(LiveHandle {
					_listener: listener,
					drainer,
				});
				tracing::debug!("live socket path started");
			}
			_ => drainer.abort(),
		}
	});
}

/// The socket thread's side: deep-copy and send, nothing else. It runs on the thread draining
/// the websocket and must not block; a send failing means the drainer is gone, where dropping
/// the event is exactly right.
fn live_callback(
	sender: UnboundedSender<OwnedEvent>,
) -> Box<dyn Fn(&DecryptedSocketEvent<'_>) + Send + 'static> {
	Box::new(move |event| {
		let _ = sender.send(event.to_owned_cow());
	})
}

/// The ONE consumer: applies events strictly in the order the socket delivered them.
///
/// The gap pass runs INSIDE this loop (on `authSuccess`, which the socket broadcasts on every
/// connect and reconnect), which is what closes the snapshot race by construction: events
/// arriving while a pass's listings are in flight buffer in the channel and are applied only
/// after every listing has landed — a listing taken before a rename can therefore never be
/// applied after the rename's event and silently revert it.
async fn drain_live_events(
	state: Weak<RwLock<CacheState>>,
	mut events: UnboundedReceiver<OwnedEvent>,
) {
	while let Some(event) = events.recv().await {
		let Some(state) = state.upgrade() else {
			return;
		};
		match event {
			// Everything on the drive is gone — or so the event says. Never applied as a bulk
			// delete: the reconciling pass tells "gone" from "unreachable" per container, and
			// the guarded forgets keep any unsent local edit. The watermark advances only if
			// the pass converged; otherwise the pass is recorded as owed and the next gap check
			// retries it.
			DecryptedSocketEvent::Drive {
				inner: DecryptedDriveEvent::DeleteAll,
				drive_message_id,
			} => {
				tracing::warn!("deleteAll received; reconciling every materialized container");
				let before = change_stamp(&state).await;
				// Stamped from before the pass, and only over a pass that converged — the same
				// rule `gap_check` follows. Without it the cursor still points behind this
				// deleteAll, and the next gap's replay walks back across an event it must
				// refuse, spending a whole log walk to reach the pass it could have started with.
				let started = replay::pass_cursor(Utc::now());
				if reconcile_pass(&state, drive_message_id).await {
					advance_events_cursor(&state, started).await;
				}
				notify_if_changed(&state, before).await;
			}
			DecryptedSocketEvent::Drive {
				inner,
				drive_message_id,
			} => {
				let event_type = inner.event_type();
				let before = change_stamp(&state).await;
				let applied = {
					let guard = state.read().await;
					let AuthStatus::Authenticated(auth) = &guard.status else {
						continue;
					};
					auth.apply_drive_event(inner).await
				};
				match applied {
					Ok(applied) => {
						// Advanced AFTER the apply committed, never before: a crash between
						// the two re-delivers (via the gap pass) instead of skipping forever.
						advance_watermark(&state, drive_message_id).await;
						if applied {
							tracing::debug!("applied live {event_type} event");
						}
						// NOT gated on `applied`: the change stamp is the authority on whether
						// anything a replica can see actually moved, and it is strictly the
						// safer test. An arm that writes while reporting `applied == false`
						// would otherwise be a silently missed signal; the cost of asking
						// anyway is at most one spurious signal, which is an empty diff.
						notify_if_changed(&state, before).await;
					}
					Err(e) => {
						// The event did NOT commit, so the watermark must not move past it —
						// but holding it back is not enough on its own: the NEXT event advances
						// the watermark past this id, and a watermark-only gap check would then
						// see nothing to close. The debt is recorded durably instead, and the
						// next gap check re-lists what this failed to apply. Best-effort beyond
						// that: the cache is authoritative for nothing.
						tracing::warn!("failed to apply live {event_type} event: {e}");
						mark_pending_reconcile(&state).await;
					}
				}
			}
			// A drive event happened whose payload could not be read. Nothing to apply — and
			// nothing to fear for the watermark: the id is known, and the affected item
			// reconciles on its next event or browse.
			DecryptedSocketEvent::DriveMalformed { drive_message_id } => {
				advance_watermark(&state, drive_message_id).await;
			}
			// The socket (re)connected: check for a gap and close it. This is ALSO the launch
			// path's check — the first authSuccess after subscribing is the first moment a gap
			// is even answerable, and it cannot race the subscription that produced it.
			DecryptedSocketEvent::AuthSuccess => {
				gap_check(&state).await;
			}
			// Remaining connection-state cues need no action: a reconnect ends in its own
			// authSuccess, and a failed auth delivers nothing to fall behind on.
			_ => {}
		}
	}
}

/// Compares the persisted watermark against the account's drive counter and, on any gap — or on
/// a durably owed pass — re-lists every reported materialized container, the only directories
/// whose staleness the system cannot heal by itself. The watermark advances to the PRE-PASS
/// remote counter, and only when every container converged; a partial pass leaves it put and
/// records the debt, because the watermark alone cannot hold a retry open (the next event
/// advances it past the gap).
async fn gap_check(state: &Arc<RwLock<CacheState>>) {
	let (client, watermark, owed, cursor) = {
		let guard = state.read().await;
		let AuthStatus::Authenticated(auth) = &guard.status else {
			return;
		};
		(
			auth.client.clone(),
			auth.drive_watermark(),
			auth.pending_reconcile() > 0,
			auth.events_cursor(),
		)
	};
	// NO state guard across the round trip (the module's lock discipline).
	let remote = match client.get_last_event_ids().await {
		Ok(response) => response.drive,
		// At INFO like the decisions below: it skips the whole check, at most once per connect.
		Err(e) => {
			tracing::info!("gap check could not read the drive counter: {e}");
			return;
		}
	};
	if !pass_owed(owed, watermark, remote) {
		return;
	}
	// Says what was found, NOT what will be done about it — which is decided below, and was
	// worth splitting apart: this line used to promise a full re-list before anything had
	// chosen one, so a log could show a sweep that never happened.
	tracing::info!("drive event gap (local {watermark:?}, remote {remote}, owed {owed})");
	// A pass that wrote no row has nothing for the replica to diff (see `notify_if_changed`).
	let before = change_stamp(state).await;
	// The event log can usually say WHICH containers moved, turning a ~3,000-listing sweep into
	// one or two. It refuses rather than guesses (see [`crate::replay`]), and a refusal — or a
	// targeted pass that failed to converge — leaves the full pass exactly as it was.
	// Leans BEHIND the local clock on purpose — see [`replay::pass_cursor`]; this value is read
	// back against server timestamps, and the two clocks are not the same clock.
	let started = replay::pass_cursor(Utc::now());
	// Whether the log may answer this gap at all is [`replay::replay_allowed`]'s to decide — a
	// pure predicate precisely because the debt rule it encodes is one careless reordering away
	// from reintroducing the stale-forever bug, and a predicate can be unit-tested where this
	// `if` cannot.
	let replayed = match cursor {
		Some(cursor) if replay::replay_allowed(owed, Some(cursor)) => {
			close_gap_by_replay(state, &client, remote, cursor).await
		}
		// Each refusal says so. Silence here is what made a correct first-run sweep look like a
		// broken feature: the decision left no trace at all, so the only way to tell which path
		// had run was to read the source.
		Some(_) => {
			tracing::info!("a reconcile pass is owed, so the log cannot answer this gap");
			false
		}
		None => {
			tracing::info!(
				"no event cursor yet, so this gap has no lower bound to replay from; the pass \
				 below stamps one and the next gap can use the log"
			);
			false
		}
	};
	if !replayed {
		// The cursor may only move over a pass that actually converged: it says "everything
		// before this is accounted for", and a partial pass has not accounted for it. Stamped
		// from BEFORE the pass, not after, so events that landed while it ran are re-covered
		// rather than skipped — re-listing a container is idempotent.
		if reconcile_pass(state, remote).await {
			advance_events_cursor(state, started).await;
		}
	}
	notify_if_changed(state, before).await;
}

/// Tries to close the gap from the event log. `false` means the caller must run the full pass —
/// no cursor yet, an event replay could not attribute, paging that could not reach the cursor,
/// or a targeted re-list that did not converge.
async fn close_gap_by_replay(
	state: &Arc<RwLock<CacheState>>,
	client: &filen_sdk_rs::auth::Client,
	remote: u64,
	cursor: DateTime<Utc>,
) -> bool {
	let (containers, relist_trash, next) = match attempt_replay(state, client, cursor).await {
		ReplayOutcome::Fallback => return false,
		ReplayOutcome::Targeted {
			containers,
			relist_trash,
			cursor,
		} => (containers, relist_trash, cursor),
	};
	tracing::info!(
		"replaying the gap from the event log: {} container(s) to re-list{}",
		containers.len(),
		if relist_trash { " plus the trash" } else { "" }
	);
	if relist_trash {
		let guard = state.read().await;
		let AuthStatus::Authenticated(auth) = &guard.status else {
			return false;
		};
		if let Err(e) = auth.update_trash().await {
			tracing::warn!("replay could not re-list the trash: {e}");
			return false;
		}
	}
	if !relist_containers(state, containers.into_iter().collect()).await {
		tracing::info!("a container the replay named did not converge; full pass");
		return false;
	}
	// Only now: a cursor advanced over a container that did not converge would leave it stale
	// with nothing owed, which is the stale-forever bug this whole path exists to remove.
	advance_events_cursor(state, next).await;
	// This gap is as closed as a pass closes it, and has to be recorded as such. Without it the
	// watermark sits behind the counter forever: `pass_owed` stays true, the cheap "nothing
	// happened" return is dead, and every reconnect replays a gap that is no longer there.
	advance_watermark(state, remote).await;
	true
}

/// Pages the event feed back to `cursor`, folding each page into the container set it implies.
///
/// The loop's own termination is the truncation check: `v3/user/events` is cursored by a
/// seconds-resolution timestamp, so a second holding more events than one page cannot be paged
/// through — asking again just returns the same page. A page that contributes no new event id
/// is exactly that, and falls back rather than silently skipping the remainder of the second.
async fn attempt_replay(
	state: &Arc<RwLock<CacheState>>,
	client: &filen_sdk_rs::auth::Client,
	cursor: DateTime<Utc>,
) -> ReplayOutcome {
	let mut containers: HashSet<Uuid> = HashSet::new();
	let mut relist_trash = false;
	let mut seen: HashSet<u64> = HashSet::new();
	let mut newest: Option<DateTime<Utc>> = None;
	let mut cutoff = replay::first_cutoff(Utc::now());

	for page_number in 0.. {
		if replay::page_budget_exhausted(page_number) {
			tracing::info!("event replay exceeded its page budget; a full pass is cheaper");
			return ReplayOutcome::Fallback;
		}
		let page = match client.get_user_events(None, Some(cutoff)).await {
			Ok(page) => page,
			Err(e) => {
				tracing::info!("event replay could not read the log: {e}");
				return ReplayOutcome::Fallback;
			}
		};
		let mut decoded = Vec::with_capacity(page.len());
		for result in page {
			match result {
				Ok(event) => decoded.push(event),
				// An event we cannot read may be a mutation we cannot see. The pass can.
				Err(e) => {
					tracing::info!("event replay hit an undecodable event ({e}); full pass");
					return ReplayOutcome::Fallback;
				}
			}
		}
		// The feed ran out before reaching the cursor: the gap is older than what the log keeps.
		if decoded.is_empty() {
			tracing::info!("event log is exhausted before the cursor; full pass");
			return ReplayOutcome::Fallback;
		}
		if !replay::made_progress(&mut seen, &decoded) {
			tracing::info!(
				"event log page made no progress (a second exceeds one page); full pass"
			);
			return ReplayOutcome::Fallback;
		}
		{
			let guard = state.read().await;
			let AuthStatus::Authenticated(auth) = &guard.status else {
				return ReplayOutcome::Fallback;
			};
			if !replay::fold_targets(
				&auth.conn(),
				&decoded,
				cursor,
				&mut containers,
				&mut relist_trash,
			) {
				return ReplayOutcome::Fallback;
			}
		}
		newest = newest.max(replay::newest(&decoded));
		let Some(next) = replay::next_cutoff(&decoded) else {
			return ReplayOutcome::Fallback;
		};
		if replay::reached(next, cursor) {
			break;
		}
		cutoff = next;
	}

	match newest {
		Some(cursor) => ReplayOutcome::Targeted {
			containers,
			relist_trash,
			cursor,
		},
		None => ReplayOutcome::Fallback,
	}
}

/// The gap gate: a pass runs when the remote counter has moved past our watermark, and ALSO
/// whenever one is durably owed — a container that failed to converge, an event that failed to
/// apply, a container reported for the first time. The debt has to be its own answer here,
/// because the events that follow the failure advance the watermark past the gap and leave the
/// counters saying "caught up" over a container nothing has re-listed.
fn pass_owed(owed: bool, watermark: Option<u64>, remote: u64) -> bool {
	owed || watermark.is_none_or(|local| local < remote)
}

/// One reconcile pass and the durable bookkeeping it owes: the watermark advances to `id` and
/// the pending flag clears only when EVERY container converged. A partial pass leaves the
/// watermark where it was AND records the debt, which is what makes the retry survive the later
/// events that carry the watermark past `id`. Returns whether it converged — the caller's cue
/// for whether anything downstream of the pass may be recorded as accounted for.
async fn reconcile_pass(state: &Arc<RwLock<CacheState>>, id: u64) -> bool {
	// Read BEFORE the container set is, so a report landing mid-pass fails the compare-and-clear
	// below instead of being cleared by a pass that never listed it.
	let owed = {
		let guard = state.read().await;
		let AuthStatus::Authenticated(auth) = &guard.status else {
			return false;
		};
		auth.pending_reconcile()
	};
	if reconcile_containers(state).await {
		advance_watermark(state, id).await;
		clear_pending_reconcile(state, owed).await;
		true
	} else {
		mark_pending_reconcile(state).await;
		false
	}
}

async fn mark_pending_reconcile(state: &Arc<RwLock<CacheState>>) {
	let guard = state.read().await;
	if let AuthStatus::Authenticated(auth) = &guard.status {
		auth.mark_pending_reconcile().await;
	}
}

async fn clear_pending_reconcile(state: &Arc<RwLock<CacheState>>, owed: u64) {
	let guard = state.read().await;
	if let AuthStatus::Authenticated(auth) = &guard.status {
		auth.clear_pending_reconcile(owed).await;
	}
}

/// Re-lists every reported materialized container (the account root included), concurrency
/// bounded, best-effort per container — one failure must not abort the pass. Returns whether
/// EVERY container converged, which is what gates the watermark.
async fn reconcile_containers(state: &Arc<RwLock<CacheState>>) -> bool {
	let containers = {
		let guard = state.read().await;
		let AuthStatus::Authenticated(auth) = &guard.status else {
			return false;
		};
		let containers: Vec<Uuid> = auth
			.working_set_containers()
			.into_iter()
			.map(Uuid::from)
			.collect();
		// Stalest first. The pass is thousands of listings long and is routinely killed before
		// it finishes — the extension is torn down far more often than it runs to completion —
		// so the order decides what a truncated pass actually accomplished. Unordered, a
		// container can lose the race every time and stay stale indefinitely while others are
		// re-listed for the third time.
		match crate::sql::select_dir_last_listed(&auth.conn()) {
			Ok(stamps) => stalest_first(&stamps, containers),
			// Ordering is an optimisation; a pass in an arbitrary order still converges.
			Err(e) => {
				tracing::debug!("could not order the pass by staleness: {e}");
				containers
			}
		}
	};
	tracing::info!(
		"re-listing all {} materialized container(s)",
		containers.len()
	);
	relist_containers(state, containers).await
}

/// Orders containers by how long they have gone unlisted, oldest first.
///
/// A container with no stamp sorts first: it has either never been listed, or it is the account
/// root (roots carry no `last_listed` row of their own), and both are the least safe thing to
/// leave for a pass that may not finish. Ties keep their relative order, so the result is stable
/// across passes rather than reshuffling on every reconnect.
fn stalest_first(
	stamps: &std::collections::HashMap<Uuid, i64>,
	mut containers: Vec<Uuid>,
) -> Vec<Uuid> {
	containers.sort_by_key(|uuid| stamps.get(uuid).copied().unwrap_or(0));
	containers
}

/// Re-lists the given containers, concurrency bounded, best-effort per container — one failure
/// must not abort the rest. Returns whether EVERY container converged, which is what gates the
/// watermark and the event cursor.
///
/// The working-set signal fires PER CONTAINER rather than once at the end. A container is fresh
/// the moment its own listing lands, and on a full pass the end is thousands of listings away —
/// on the trace that prompted this, 46 seconds of already-cached rows sat undelivered behind the
/// last of them. Signalling as they land also means a pass torn down early (the extension is
/// killed far more often than it finishes) has still delivered everything it managed to list.
/// The extra signals are nearly free: `notify_if_changed` drops the ones that moved no row, and
/// the platform side coalesces what is left.
async fn relist_containers(state: &Arc<RwLock<CacheState>>, containers: Vec<Uuid>) -> bool {
	let results: Vec<bool> = futures::stream::iter(containers.into_iter().map(|uuid| {
		let state = state.clone();
		async move {
			let before = change_stamp(&state).await;
			let converged = {
				// The guard is PER CONTAINER, so a queued writer waits for one listing at most.
				let guard = state.read().await;
				let AuthStatus::Authenticated(auth) = &guard.status else {
					return false;
				};
				auth.refresh_container(uuid).await
			};
			notify_if_changed(&state, before).await;
			converged
		}
	}))
	.buffer_unordered(CONTAINER_PROBE_CONCURRENCY)
	.collect()
	.await;
	results.into_iter().all(|converged| converged)
}

async fn advance_events_cursor(state: &Arc<RwLock<CacheState>>, at: DateTime<Utc>) {
	let guard = state.read().await;
	if let AuthStatus::Authenticated(auth) = &guard.status {
		auth.advance_events_cursor(at).await;
	}
}

async fn advance_watermark(state: &Arc<RwLock<CacheState>>, id: u64) {
	let guard = state.read().await;
	if let AuthStatus::Authenticated(auth) = &guard.status {
		auth.advance_drive_watermark(id).await;
	}
}

/// The database's change stamp — the instance that issued it and the sequence it has reached —
/// or `None` when it cannot be read (no authenticated state, or the read failed).
///
/// This pair IS everything a replica can be shown: `changes_since` derives both the diff and the
/// anchor it hands back from these two values ([`crate::local`]), and every trigger that stamps a
/// row or writes a tombstone raises the counter first (`sql/init.sql`) behind a guard that is a
/// real OLD/NEW comparison. A pass that leaves the stamp where it was therefore has nothing to
/// signal: the enumeration it would provoke is guaranteed to come back empty.
async fn change_stamp(state: &Arc<RwLock<CacheState>>) -> Option<([u8; 16], i64)> {
	let guard = state.read().await;
	let AuthStatus::Authenticated(auth) = &guard.status else {
		return None;
	};
	crate::sql::select_change_meta(&auth.conn()).ok()
}

/// Whether the change stamp moved between the two reads. Unreadable on either side counts as
/// moved: a spurious signal costs one enumeration, a missed one costs freshness until the
/// container is next presented.
fn stamp_moved(before: Option<([u8; 16], i64)>, after: Option<([u8; 16], i64)>) -> bool {
	before
		.zip(after)
		.is_none_or(|(before, after)| before != after)
}

/// Tells the replica its working set moved — but only if this pass actually moved it.
///
/// The gate exists because "applied" over-reports: `apply_file_record` answers `true` whenever
/// the row was RELEVANT, and `RelistTrash` answers `true` unconditionally, so this device's own
/// uploads and renames echoing back over the socket upsert to identical values and still
/// signalled. Each signal is an `enumerateChanges` round trip.
///
/// The stamp is shared with every other writer on this database — the app process browsing, a
/// listing landing on another task — so it can move for a reason this pass did not cause. That
/// yields a spurious signal, never a missed one: our own writes can only raise it.
async fn notify_if_changed(state: &Arc<RwLock<CacheState>>, before: Option<([u8; 16], i64)>) {
	if stamp_moved(before, change_stamp(state).await) {
		state.read().await.notify_working_set();
	}
}

/// What the pure dispatch decided about one event: either it was applied (or dropped) locally,
/// or it needs one of the three operations that reach past the database — a guarded forget, or
/// the trash relist.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LocalApply {
	/// Locally settled; `true` means a row changed.
	Applied(bool),
	/// The lineage is gone for good — retire it through the pending-guarded forget path.
	ForgetFile(StableUuid),
	ForgetDir(Uuid),
	/// The trash was emptied elsewhere — reconcile our trashed rows against a fresh listing.
	RelistTrash,
}

/// Applies one decrypted drive event to the database, or names the follow-up it needs.
///
/// Pure with respect to everything but the connection, which is what makes the whole 19-arm
/// dispatch — folds, relevance gate, trash semantics — testable against an in-memory database.
/// Every arm goes through the same upserts a server refresh uses, so the change-sequence and
/// tombstone triggers fire identically, and the pending-upload marker column stays untouchable.
pub(crate) fn apply_drive_event_local(
	conn_mutex: &Mutex<Connection>,
	event: DecryptedDriveEvent<'static>,
) -> Result<LocalApply, CacheError> {
	use filen_sdk_rs::socket as ev;
	Ok(LocalApply::Applied(match event {
		// A full file record: a new upload, an edit's new head, a restore from the trash, or a
		// move. One arm, because they all carry the same thing and land the same way — the
		// stable tier of the upsert resolves them onto the row we hold.
		DecryptedDriveEvent::FileNew(ev::FileNew(file))
		| DecryptedDriveEvent::FileRestore(ev::FileRestore(file))
		| DecryptedDriveEvent::FileMove(ev::FileMove(file)) => apply_file_record(conn_mutex, file)?,
		// A version restore replacing the current head: `file` IS the new head.
		DecryptedDriveEvent::FileArchiveRestored(e) => apply_file_record(conn_mutex, e.file)?,
		// `new_uuid` set means this is the RETIREMENT half of a versioning-disabled edit, not a
		// user trash — the successor arrives as its own `fileNew` (in no guaranteed order), and
		// trashing here would flicker the live row through the trash.
		DecryptedDriveEvent::FileTrash(e) => {
			if e.new_uuid.is_some() {
				false
			} else {
				apply_file_trash(conn_mutex, e.uuid, e.stable_uuid)?
			}
		}
		// `new_uuid` set is a normal edit's archive of the superseded version — the row already
		// follows the lineage. Absent, the lineage itself was REPLACED (move-with-replace):
		// gone for good.
		DecryptedDriveEvent::FileArchived(e) => {
			if e.new_uuid.is_some() {
				false
			} else {
				return Ok(LocalApply::ForgetFile(e.stable_uuid));
			}
		}
		// `stable_uuid` absent means only archived VERSIONS died; the live head stands.
		DecryptedDriveEvent::FileDeletedPermanent(e) => match e.stable_uuid {
			Some(stable) => return Ok(LocalApply::ForgetFile(stable)),
			None => false,
		},
		// A rename (or other metadata change): no full record comes with it, so the new meta is
		// patched onto the row we hold. Keyed by the whole-life id — a rename arriving after an
		// edit re-minted the uuid still names the same lineage.
		DecryptedDriveEvent::FileMetadataChanged(e) => {
			apply_file_meta(conn_mutex, e.stable_uuid, e.metadata)?
		}
		DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir))
		| DecryptedDriveEvent::FolderMove(ev::FolderMove(dir))
		| DecryptedDriveEvent::FolderRestore(ev::FolderRestore(dir)) => {
			apply_dir_record(conn_mutex, dir)?
		}
		DecryptedDriveEvent::FolderTrash(e) => apply_dir_trash(conn_mutex, e.uuid)?,
		DecryptedDriveEvent::FolderColorChanged(e) => apply_dir_patch(conn_mutex, e.uuid, |dir| {
			dir.color = e.color.into_owned_cow();
		})?,
		DecryptedDriveEvent::FolderMetadataChanged(e) => {
			apply_dir_patch(conn_mutex, e.uuid, |dir| {
				dir.meta = e.meta.into_owned_cow();
			})?
		}
		DecryptedDriveEvent::FolderDeletedPermanent(e) => {
			return Ok(LocalApply::ForgetDir(e.uuid));
		}
		// Carries the full record, so it lands like any other — including the favorite flag,
		// which the per-type upsert folds into the local rank.
		DecryptedDriveEvent::ItemFavorite(ev::ItemFavorite(item)) => match item {
			NonRootItemType::File(file) => apply_file_record(conn_mutex, file.into_owned())?,
			NonRootItemType::Dir(dir) => apply_dir_record(conn_mutex, dir.into_owned())?,
		},
		// The trash emptied elsewhere. The rows to retire are exactly the trashed rows we hold,
		// and the trash relist already reconciles them properly (per-row probes, pending markers
		// released on definitive not-found) — reuse it rather than guess. Gated on actually
		// holding trashed rows, so an account that never trashed anything pays nothing.
		DecryptedDriveEvent::TrashEmpty => {
			if holds_trashed_rows(&conn(conn_mutex))? {
				return Ok(LocalApply::RelistTrash);
			}
			false
		}
		// Everything on the drive is gone. Deliberately NOT applied as a bulk delete: an unsent
		// local edit somewhere in the tree outranks the event, and the reconciling paths
		// (browse, the launch pass) already tell "gone" from "unreachable" per item. Rare
		// enough that correctness wins over immediacy.
		DecryptedDriveEvent::DeleteAll => {
			tracing::warn!("deleteAll received; leaving reconciliation to the refresh paths");
			false
		}
		// Only archived versions died; this cache holds heads only.
		DecryptedDriveEvent::DeleteVersioned => false,
	}))
}

/// The relevance gate for a full file record: the row (by whole-life id, then uuid), or its
/// parent.
fn file_record_is_relevant(conn: &Connection, file: &RemoteFile) -> Result<bool, rusqlite::Error> {
	Ok(
		RawDBItem::select_by_stable(conn, file.stable_uuid.into())?.is_some()
			|| RawDBItem::select(conn, file.uuid())?.is_some()
			|| parent_row_exists(conn, &file.parent)?,
	)
}

fn apply_file_record(conn_mutex: &Mutex<Connection>, file: RemoteFile) -> Result<bool, CacheError> {
	if !file_record_is_relevant(&conn(conn_mutex), &file)? {
		return Ok(false);
	}
	DBFile::upsert_from_remote(&mut conn(conn_mutex), file)?;
	Ok(true)
}

fn apply_dir_record(
	conn_mutex: &Mutex<Connection>,
	dir: RemoteDirectory,
) -> Result<bool, CacheError> {
	let relevant = {
		let conn = conn(conn_mutex);
		RawDBItem::select(&conn, dir.uuid())?.is_some() || parent_row_exists(&conn, &dir.parent)?
	};
	if !relevant {
		return Ok(false);
	}
	DBDir::upsert_from_remote(&mut conn(conn_mutex), dir)?;
	Ok(true)
}

/// A remote TRASH is a trash, not a delete: the row stays, marked trashed with its original
/// parent (where a restore puts it back), and its local bytes stay with it — this cache's own
/// trash model. Goes through `upsert_from_remote` with a `Trash` parent, exactly how a trash
/// listing delivers the same fact, so the change-seq trigger fires and no tombstone does.
fn apply_file_trash(
	conn_mutex: &Mutex<Connection>,
	uuid: Uuid,
	stable: StableUuid,
) -> Result<bool, CacheError> {
	let held = {
		let conn = conn(conn_mutex);
		match RawDBItem::select_by_stable(&conn, stable.into())? {
			Some(item) => DBFile::select(&conn, item.uuid).optional()?,
			None => DBFile::select(&conn, uuid).optional()?,
		}
	};
	let Some(file) = held else {
		return Ok(false);
	};
	let mut remote = RemoteFile::try_from(file)?;
	match remote.parent {
		// Already trashed: the row is where it belongs.
		ParentUuid::Trash(_) => Ok(false),
		ParentUuid::Uuid(parent) => {
			remote.parent = ParentUuid::Trash(parent);
			DBFile::upsert_from_remote(&mut conn(conn_mutex), remote)?;
			Ok(true)
		}
		// A row parented to a virtual container has no original parent to restore to; leave it
		// for the next refresh rather than invent one.
		other => {
			tracing::warn!("trashed file {uuid} has no restorable parent ({other:?})");
			Ok(false)
		}
	}
}

fn apply_dir_trash(conn_mutex: &Mutex<Connection>, uuid: Uuid) -> Result<bool, CacheError> {
	let held = { DBObject::select(&conn(conn_mutex), uuid).optional()? };
	let Some(DBObject::Dir(dir)) = held else {
		return Ok(false);
	};
	let mut remote = RemoteDirectory::from(dir);
	match remote.parent {
		ParentUuid::Trash(_) => Ok(false),
		ParentUuid::Uuid(parent) => {
			remote.parent = ParentUuid::Trash(parent);
			DBDir::upsert_from_remote(&mut conn(conn_mutex), remote)?;
			Ok(true)
		}
		other => {
			tracing::warn!("trashed dir {uuid} has no restorable parent ({other:?})");
			Ok(false)
		}
	}
}

fn apply_file_meta(
	conn_mutex: &Mutex<Connection>,
	stable: StableUuid,
	meta: FileMeta<'static>,
) -> Result<bool, CacheError> {
	let held = {
		let conn = conn(conn_mutex);
		match RawDBItem::select_by_stable(&conn, stable.into())? {
			Some(item) => DBFile::select(&conn, item.uuid).optional()?,
			None => None,
		}
	};
	let Some(file) = held else {
		return Ok(false);
	};
	let mut remote = RemoteFile::try_from(file)?;
	remote.meta = meta;
	DBFile::upsert_from_remote(&mut conn(conn_mutex), remote)?;
	Ok(true)
}

fn apply_dir_patch(
	conn_mutex: &Mutex<Connection>,
	uuid: Uuid,
	patch: impl FnOnce(&mut RemoteDirectory),
) -> Result<bool, CacheError> {
	let held = { DBObject::select(&conn(conn_mutex), uuid).optional()? };
	let Some(DBObject::Dir(dir)) = held else {
		return Ok(false);
	};
	let mut remote = RemoteDirectory::from(dir);
	patch(&mut remote);
	DBDir::upsert_from_remote(&mut conn(conn_mutex), remote)?;
	Ok(true)
}

fn holds_trashed_rows(conn: &Connection) -> Result<bool, rusqlite::Error> {
	conn.query_one(
		"SELECT EXISTS (SELECT 1 FROM items WHERE trashed = TRUE);",
		[],
		|row| row.get(0),
	)
}

/// Whether the cache holds a row for the container an incoming record is parented to.
fn parent_row_exists(conn: &Connection, parent: &ParentUuid) -> Result<bool, rusqlite::Error> {
	let uuid = match parent {
		ParentUuid::Uuid(uuid) | ParentUuid::Trash(uuid) => *uuid,
		_ => return Ok(false),
	};
	Ok(RawDBItem::select(conn, uuid)?.is_some())
}

impl AuthCacheState {
	/// Applies one decrypted drive event to the cache. `Ok(true)` when a row changed — the cue
	/// to signal the working set. The local dispatch does everything that stays inside the
	/// database; the follow-ups it names run here, where the io and the client live.
	pub(crate) async fn apply_drive_event(
		&self,
		event: DecryptedDriveEvent<'static>,
	) -> Result<bool, CacheError> {
		// The applier is the WRITER on the listing barrier (see `listing_barrier`): an event
		// waits out every listing already in flight, so a pre-event snapshot can never be
		// applied on top of it and revert it. SQL only under the write guard — the follow-ups
		// below reach past the database, and the barrier is the one lock every browse listing
		// queues on.
		let outcome = {
			let _apply = self.listing_barrier.write().await;
			apply_drive_event_local(self.conn_mutex(), event)?
		};
		// The follow-ups run UNGUARDED, all three of them. A forget takes the item's transfer
		// lock (`io_delete_local`), which a download or upload of that same item holds for its
		// whole duration — under the write guard that would park every browse listing, and the
		// drainer itself, behind a transfer that can take minutes. There is nothing for the
		// barrier to order here anyway: a forget fetches nothing, and the trash relist takes the
		// barrier as a reader of its own (and must not nest inside a write guard).
		//
		// The guard cannot instead be scoped to the row delete inside `forget_item`: that
		// function is also reached from `refresh_file`/`refresh_dir` while they hold the barrier
		// as READERS, and a write guard requested from inside a read guard deadlocks on itself.
		// The cost of going unguarded is that a listing already in flight can re-add the row it
		// snapshotted before the delete — a stale row with no local bytes, which the next
		// listing of its parent sweeps.
		match outcome {
			LocalApply::Applied(applied) => Ok(applied),
			LocalApply::ForgetFile(stable) => self.forget_by_stable(stable).await,
			LocalApply::ForgetDir(uuid) => self.forget_dir(uuid).await,
			LocalApply::RelistTrash => {
				self.update_trash().await?;
				Ok(true)
			}
		}
	}

	/// Retires a file lineage the server says is gone for good, through the same guarded path a
	/// refresh uses: an unsent local edit keeps the row.
	async fn forget_by_stable(&self, stable: StableUuid) -> Result<bool, CacheError> {
		let held = {
			let conn = self.conn();
			match RawDBItem::select_by_stable(&conn, stable.into())? {
				Some(item) => DBFile::select(&conn, item.uuid).optional()?,
				None => None,
			}
		};
		let Some(file) = held else {
			return Ok(false);
		};
		Ok(self.forget_item(DBObject::File(file)).await?.is_none())
	}

	async fn forget_dir(&self, uuid: Uuid) -> Result<bool, CacheError> {
		let held = { DBObject::select(&self.conn(), uuid).optional()? };
		let Some(obj @ DBObject::Dir(_)) = held else {
			return Ok(false);
		};
		Ok(self.forget_item(obj).await?.is_none())
	}
}

impl AuthCacheState {
	/// The container set the working-set predicate binds: every reported materialized container
	/// plus — unconditionally — the account root. The root is `type = 0` and excluded from the
	/// working set itself, so without this injection every top-level item would be a non-member
	/// and the root folder would stay exactly as stale as an unreported container.
	pub(crate) fn working_set_containers(&self) -> Vec<UuidStr> {
		let set = self
			.materialized_containers
			.read()
			.unwrap_or_else(|poisoned| poisoned.into_inner());
		set.iter()
			.copied()
			.chain(std::iter::once(self.client.root().uuid()))
			.map(UuidStr::from)
			.collect()
	}

	pub(crate) fn drive_watermark(&self) -> Option<u64> {
		*lock(&self.drive_watermark)
	}

	/// The newest event-log second this cache has replayed (see
	/// [`crate::auth::SavedDBState::events_cursor`]). `None` means replay has never run and the
	/// next gap must be closed by a full pass.
	pub(crate) fn events_cursor(&self) -> Option<DateTime<Utc>> {
		lock(&self.events_cursor).and_then(|secs| DateTime::from_timestamp(secs, 0))
	}

	/// Advances the event cursor (monotonic; an older stamp is a no-op) and mirrors it to
	/// `db_state.json`. Called only after the containers the replay named have converged.
	pub(crate) async fn advance_events_cursor(&self, at: DateTime<Utc>) {
		let secs = at.timestamp();
		{
			let mut cursor = lock(&self.events_cursor);
			if cursor.is_some_and(|current| current >= secs) {
				return;
			}
			*cursor = Some(secs);
		}
		if let Err(e) = crate::auth::update_saved_db_state(&self.cache_state_file, move |state| {
			state.events_cursor = Some(state.events_cursor.unwrap_or(i64::MIN).max(secs));
		})
		.await
		{
			tracing::warn!("failed to persist the event cursor: {e}");
		}
	}

	/// Advances the drive watermark to `id` (monotonic; a lower id is a no-op) and mirrors it to
	/// `db_state.json`. Called AFTER an apply commits, never before — the difference between a
	/// crash re-delivering a change and skipping it forever.
	pub(crate) async fn advance_drive_watermark(&self, id: u64) {
		{
			let mut watermark = lock(&self.drive_watermark);
			if watermark.is_some_and(|current| current >= id) {
				return;
			}
			*watermark = Some(id);
		}
		if let Err(e) = crate::auth::update_saved_db_state(&self.cache_state_file, move |state| {
			// Monotonic against a concurrent writer of the same file too.
			state.drive_watermark = Some(state.drive_watermark.unwrap_or(0).max(id));
		})
		.await
		{
			tracing::warn!("failed to persist the drive watermark: {e}");
		}
	}

	/// The reconcile debt this cache carries (see [`crate::auth::SavedDBState::pending_reconcile`]):
	/// zero means none. Any other value means a pass is owed, and IDENTIFIES that debt — a pass
	/// clears only the value it saw.
	pub(crate) fn pending_reconcile(&self) -> u64 {
		self.pending_reconcile.load(Ordering::Acquire)
	}

	/// Records that a reconcile pass is owed, durably: the flag has to outlive both the process
	/// and the watermark advances that follow it.
	pub(crate) async fn mark_pending_reconcile(&self) {
		if self.pending_reconcile.fetch_add(1, Ordering::AcqRel) > 0 {
			// Already owed and already persisted; the bump alone invalidates any pass in flight.
			return;
		}
		self.persist_pending_reconcile(true).await;
	}

	/// Clears the debt `owed` — and only that one. A mark that landed since (a report of a
	/// container this pass never listed) leaves it standing for the next pass.
	pub(crate) async fn clear_pending_reconcile(&self, owed: u64) {
		if owed == 0
			|| self
				.pending_reconcile
				.compare_exchange(owed, 0, Ordering::AcqRel, Ordering::Acquire)
				.is_err()
		{
			return;
		}
		self.persist_pending_reconcile(false).await;
	}

	async fn persist_pending_reconcile(&self, pending: bool) {
		if let Err(e) = crate::auth::update_saved_db_state(&self.cache_state_file, move |state| {
			state.pending_reconcile = Some(pending);
		})
		.await
		{
			tracing::warn!("failed to persist the pending-reconcile flag: {e}");
		}
	}

	/// One container's share of a reconcile pass: relist it in place, `true` when it converged.
	/// A container the cache holds no dir row for converges trivially — the seeding probes own
	/// it, and its children cannot be staler than unknown. A listing that fails is classified
	/// with one probe: definitively gone converges through the guarded forget; anything else is
	/// a transient failure the next pass retries.
	pub(crate) async fn refresh_container(&self, uuid: Uuid) -> bool {
		let held = match DBObject::select(&self.conn(), uuid).optional() {
			Ok(held) => held,
			Err(e) => {
				tracing::warn!("could not read container {uuid}: {e}");
				return false;
			}
		};
		let mut dir = match held {
			Some(DBObject::Dir(dir)) => crate::sql::DBDirObject::Dir(dir),
			Some(DBObject::Root(root)) => crate::sql::DBDirObject::Root(root),
			// A file id in the set (a mis-filtered report) has nothing to list, ever.
			Some(DBObject::File(_)) => return true,
			// No row: the report's seeding probe failed (an offline launch). NOT convergence —
			// claiming it would advance the watermark past a container that was never listed
			// and re-create the stale-forever bug. Seed it here, or hold the watermark back so
			// the next connect retries.
			None => match self.client.get_dir(uuid).await {
				Ok(remote) => match DBDir::upsert_from_remote(&mut self.conn(), remote) {
					Ok(dir) => crate::sql::DBDirObject::Dir(dir),
					Err(e) => {
						tracing::warn!("could not seed container {uuid} for the pass: {e}");
						return false;
					}
				},
				// Definitively gone (or never a dir): nothing below it can be stale.
				Err(e) if e.kind() == filen_sdk_rs::ErrorKind::FolderNotFound => return true,
				Err(e) => {
					tracing::debug!("could not seed container {uuid} for the pass: {e}");
					return false;
				}
			},
		};
		match self.inner_update_dir(&mut dir).await {
			Ok(()) => true,
			Err(e) => {
				// `CacheError` folds the SDK error kind away, so classify with one probe.
				match self.client.get_dir(uuid).await {
					// Definitively gone: nothing below it can be stale once it is forgotten.
					Err(probe) if probe.kind() == filen_sdk_rs::ErrorKind::FolderNotFound => {
						tracing::debug!("container {uuid} is gone; forgetting it");
						self.forget_dir(uuid).await.is_ok()
					}
					// Trashed, and the probe is the only way to learn it: `v3/dir/content`
					// refuses to list a trashed directory while `v3/dir` still resolves it
					// (pinned by `create_list_trash` in filen-sdk-rs). Read as a transient
					// failure it held the pass — and with it the watermark — back forever, so
					// every reconnect re-listed every materialized container. It is not this
					// path's to refresh either: every other caller routes a trashed item into
					// the `trash/` namespace (`canonicalize_id`), whose listing owns it and
					// carries its own restore reconciliation. Converged.
					//
					// Asking the SERVER rather than the cached row is what makes this safe: a
					// container restored while the socket was down still has a trashed row
					// here, and skipping that one on the row's say-so would advance the
					// watermark past a container whose children were never re-listed.
					Ok(remote) if remote.parent().is_trash() => {
						tracing::debug!("container {uuid} is trashed; the trash listing owns it");
						true
					}
					_ => {
						tracing::warn!(
							"re-listing container {uuid} (parent {parent}) failed: {e}",
							parent = match &dir {
								crate::sql::DBDirObject::Dir(dir) => dir.parent.to_string(),
								crate::sql::DBDirObject::Root(_) => "root".to_owned(),
							}
						);
						false
					}
				}
			}
		}
	}

	/// Replaces the reported materialized-container set (see the FFI wrapper below).
	///
	/// The ORDER inside is load-bearing: the set is stored — in memory and in `db_state.json` —
	/// BEFORE any probe runs, because the probes are recovery, not a gate. An offline launch
	/// whose probes all fail must still hold the full set, or it silently reproduces the
	/// stale-forever bug for the process lifetime.
	pub(crate) async fn set_materialized_containers(
		&self,
		ids: Vec<String>,
	) -> Result<(), CacheError> {
		let root = self.client.root().uuid();
		let mut parsed: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
		for id in &ids {
			// The platform reports the identifiers it holds, which are `stable/<uuid>` for
			// directories; take the bare form too.
			let bare = id.strip_prefix(crate::local::STABLE_PREFIX).unwrap_or(id);
			match bare.parse::<Uuid>() {
				// The root is injected at query time, always; storing it would only duplicate.
				Ok(uuid) if uuid != root => {
					parsed.insert(uuid);
				}
				Ok(_) => {}
				Err(e) => {
					tracing::warn!("ignoring unparseable materialized container id {id}: {e}");
				}
			}
		}
		tracing::debug!("materialized containers reported: {} ids", parsed.len());

		let mirrored: Vec<UuidStr> = parsed.iter().copied().map(UuidStr::from).collect();
		let added = {
			let mut held = self
				.materialized_containers
				.write()
				.unwrap_or_else(|poisoned| poisoned.into_inner());
			let added = parsed.iter().any(|uuid| !held.contains(uuid));
			*held = parsed.clone();
			added
		};
		// A container reported for the first time has never been kept fresh by this cache, and
		// the watermark says nothing about it — it may sit at the remote counter while this
		// container's contents are months old. Owe a pass for it. (After the set is stored, so
		// the pass that answers the debt lists it.)
		if added {
			self.mark_pending_reconcile().await;
		}
		// Mirrored durably, best-effort: a failed write costs the NEXT launch's pre-drain
		// freshness, never this session's.
		if let Err(e) = crate::auth::update_saved_db_state(&self.cache_state_file, move |state| {
			state.materialized_containers = Some(mirrored);
		})
		.await
		{
			tracing::warn!("failed to mirror the materialized containers to db_state.json: {e}");
		}

		// Best-effort probes seed rows for containers the (possibly wiped) cache has never
		// heard of, so their children can resolve. A failed probe never drops the id.
		let missing: Vec<Uuid> = {
			let conn = self.conn();
			parsed
				.into_iter()
				.filter(|uuid| !matches!(RawDBItem::select(&conn, *uuid), Ok(Some(_))))
				.collect()
		};
		futures::stream::iter(missing.into_iter().map(|uuid| async move {
			match self.client.get_dir(uuid).await {
				Ok(dir) => {
					if let Err(e) = DBDir::upsert_from_remote(&mut self.conn(), dir) {
						tracing::warn!("failed to seed materialized container {uuid}: {e}");
					}
				}
				// Recovery only: the id stays in the set whatever this said.
				Err(e) => {
					tracing::debug!("could not seed materialized container {uuid}: {e}");
				}
			}
		}))
		.buffer_unordered(CONTAINER_PROBE_CONCURRENCY)
		.collect::<Vec<()>>()
		.await;
		Ok(())
	}
}

/// Concurrency for the container seeding probes (and the gap pass's listings): 4 keeps the
/// worst case around a few MB of listing buffers inside a ~20 MB extension.
const CONTAINER_PROBE_CONCURRENCY: usize = 4;

#[filen_macros::create_uniffi_wrapper]
impl FilenMobileCacheState {
	/// Replaces the set of MATERIALIZED CONTAINERS the platform reports — the directories the
	/// system has synced to disk and will never re-enumerate on its own, whose contents are
	/// therefore this cache's to keep fresh through the working set. Wholesale replace on every
	/// call (idempotent, self-healing after a missed report); ids are container uuids in either
	/// bare or `stable/<uuid>` form. Call it with the drained
	/// `enumeratorForMaterializedItems` set at launch and again (debounced) from
	/// `materializedItemsDidChange`.
	pub async fn set_materialized_containers(&self, ids: Vec<String>) -> Result<(), CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.set_materialized_containers(ids).await
		})
		.await
	}
}

impl CacheState {
	/// Tell the replica something in its working set moved. Bounced off the runtime like the
	/// search update callback, so a slow consumer cannot hold up the apply that woke it.
	pub(crate) fn notify_working_set(&self) {
		let listener = lock(&self.working_set_listener).clone();
		if let Some(listener) = listener {
			tokio::task::spawn_blocking(move || listener.working_set_changed());
		}
	}
}

#[uniffi::export]
impl FilenMobileCacheState {
	/// Brings the live socket path up for the current authenticated state, if it is not up
	/// already. Idempotent, and the production auth paths call it on their own — this export
	/// exists for explicit platform control and for tests (the in-memory constructor
	/// deliberately never auto-starts).
	pub fn start_live_updates(&self) {
		ensure_started(&self.state, &self.tasks);
	}

	/// Tears the live path down: unsubscribes and stops applying. On a production (auth-file)
	/// state the next authenticated FFI call brings it back up on its own; an in-memory test
	/// state stays down until an explicit [`Self::start_live_updates`]. Calling this from a
	/// teardown (`invalidate()` on iOS) is safe: nothing is lost that the next launch does not
	/// rebuild.
	pub fn stop_live_updates(&self) {
		let state = self.sync_get_cache_state_borrowed();
		if let AuthStatus::Authenticated(auth) = &state.status {
			if let Some(handle) = lock(&auth.live.handle).take() {
				handle.drainer.abort();
			}
			auth.live.started.store(false, Ordering::Release);
		}
	}
	/// Registers who to tell when the live path has changed something — the replica's cue to
	/// ask for a diff (`signalEnumerator(.workingSet)` on iOS). `None` clears it.
	///
	/// Outlives auth changes, unlike the subscription itself: a listener set at startup keeps
	/// working after a re-auth without the caller having to notice.
	pub fn set_working_set_listener(&self, listener: Option<Arc<dyn WorkingSetUpdateListener>>) {
		let state = self.sync_get_cache_state_borrowed();
		*lock(&state.working_set_listener) = listener;
	}
}

#[cfg(test)]
mod pass_order_tests {
	use std::collections::HashMap;

	use filen_types::fs::Uuid;

	use super::stalest_first;

	fn uuid(byte: u8) -> Uuid {
		Uuid::from_bytes([byte; 16])
	}

	/// The pass is routinely killed before it finishes, so the order is what a truncated pass
	/// actually delivered. Oldest-listed first, and anything with no stamp at all — never listed,
	/// or the account root, which carries no `last_listed` row — ahead of everything.
	#[test]
	fn the_pass_walks_the_stalest_containers_first() {
		let stamps = HashMap::from([(uuid(1), 300), (uuid(2), 100), (uuid(3), 200)]);
		assert_eq!(
			stalest_first(&stamps, vec![uuid(1), uuid(2), uuid(3), uuid(4)]),
			vec![uuid(4), uuid(2), uuid(3), uuid(1)],
			"unstamped first, then oldest listing to newest"
		);
	}

	/// Stable across passes: two containers listed in the same millisecond must not reshuffle on
	/// every reconnect, or a pass that is always cut short keeps starting somewhere new.
	#[test]
	fn ties_keep_their_order() {
		let stamps = HashMap::from([(uuid(1), 100), (uuid(2), 100), (uuid(3), 100)]);
		let order = vec![uuid(3), uuid(1), uuid(2)];
		assert_eq!(stalest_first(&stamps, order.clone()), order);
	}
}

#[cfg(test)]
mod applier_tests {
	use std::borrow::Cow;

	use chrono::{DateTime, Utc};
	use filen_sdk_rs::{
		cache::open_sqlite_in_memory,
		fs::{dir::meta::DirectoryMeta, file::meta::FileMeta},
		socket as ev,
	};
	use filen_types::{api::v3::dir::color::DirColor, crypto::EncryptedString};

	use super::*;

	fn uuid(byte: u8) -> Uuid {
		Uuid::from_bytes([byte; 16])
	}

	fn stable(byte: u8) -> StableUuid {
		StableUuid::new_for_test(uuid(byte))
	}

	const ROOT: u8 = 9;

	fn db() -> Mutex<Connection> {
		let conn = open_sqlite_in_memory().unwrap();
		crate::auth::configure_conn(&conn).unwrap();
		conn.execute_batch(crate::sql::statements::INIT).unwrap();
		conn.execute(
			"INSERT INTO items (uuid, parent, type) VALUES (?1, NULL, 0);",
			[uuid(ROOT)],
		)
		.unwrap();
		Mutex::new(conn)
	}

	fn file_record(uuid_: u8, stable_: u8, parent: u8) -> RemoteFile {
		RemoteFile {
			uuid: uuid(uuid_),
			stable_uuid: stable(stable_),
			meta: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-file"))),
			parent: ParentUuid::Uuid(uuid(parent)),
			size: 3,
			favorited: false,
			region: "de-1".into(),
			bucket: "b".into(),
			timestamp: DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000).unwrap(),
			chunks: 1,
		}
	}

	fn dir_record(uuid_: u8, parent: u8) -> RemoteDirectory {
		RemoteDirectory {
			uuid: uuid(uuid_),
			parent: ParentUuid::Uuid(uuid(parent)),
			color: DirColor::Default,
			favorited: false,
			timestamp: DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000).unwrap(),
			meta: DirectoryMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-dir"))),
		}
	}

	fn apply(conn: &Mutex<Connection>, event: DecryptedDriveEvent<'static>) -> LocalApply {
		apply_drive_event_local(conn, event).unwrap()
	}

	fn row(conn: &Mutex<Connection>, uuid_: u8) -> Option<(Option<Uuid>, bool)> {
		super::conn(conn)
			.query_row(
				"SELECT parent, trashed FROM items WHERE uuid = ?1;",
				[uuid(uuid_)],
				|r| Ok((r.get(0)?, r.get(1)?)),
			)
			.optional()
			.unwrap()
	}

	fn item_count(conn: &Mutex<Connection>) -> i64 {
		super::conn(conn)
			.query_row("SELECT COUNT(*) FROM items WHERE type != 0;", [], |r| {
				r.get(0)
			})
			.unwrap()
	}

	fn stamp(conn: &Mutex<Connection>) -> Option<([u8; 16], i64)> {
		Some(crate::sql::select_change_meta(&super::conn(conn)).unwrap())
	}

	/// The signal gate. `Applied(true)` means the event was RELEVANT, not that anything changed —
	/// this device's own uploads echo back over the socket and upsert to identical values — so
	/// the working-set signal is gated on the change stamp instead, which is the very thing the
	/// diff the signal provokes is computed from.
	#[test]
	fn the_signal_gate_follows_the_change_stamp_not_the_apply_result() {
		let conn = db();
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
		);

		// A file the cache does not hold yet: a real change, and the stamp moves.
		let before = stamp(&conn);
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8)))
			),
			LocalApply::Applied(true)
		);
		let after = stamp(&conn);
		assert!(
			stamp_moved(before, after),
			"a first upload must still signal"
		);

		// The same record again — the echo of our own upload. Still `Applied(true)`, still
		// nothing for a replica to be shown.
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8)))
			),
			LocalApply::Applied(true)
		);
		assert!(
			!stamp_moved(after, stamp(&conn)),
			"an echo that changed no row must not cost an enumerateChanges round trip"
		);

		// A pass that touched nothing at all — the `gap_check` / `deleteAll` case — is the same
		// answer.
		let idle = stamp(&conn);
		assert!(!stamp_moved(idle, stamp(&conn)));

		// And an unreadable stamp falls through to signalling rather than swallowing a change.
		assert!(stamp_moved(None, idle));
		assert!(stamp_moved(idle, None));
	}

	/// The relevance gate, both ways: an event for a container this cache has never listed is a
	/// no-op drop; once the container is held, the same event applies. This is what keeps the
	/// live path O(1) per event and the cache from growing on its own.
	#[test]
	fn the_gate_drops_what_we_do_not_hold_and_applies_what_we_do() {
		let conn = db();

		// A file under a dir nobody here has listed: dropped.
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8)))
			),
			LocalApply::Applied(false)
		);
		assert_eq!(item_count(&conn), 0);

		// The dir arrives under the root, which IS held — so it lands...
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT)))
			),
			LocalApply::Applied(true)
		);
		// ...and the same file event is now relevant through its parent.
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8)))
			),
			LocalApply::Applied(true)
		);
		assert_eq!(item_count(&conn), 2);
	}

	/// A remote edit re-mints the file's uuid; the stable tier must re-file the row IN PLACE —
	/// one row, new uuid — and the archive of the superseded version (which names a successor)
	/// must change nothing.
	#[test]
	fn a_remote_edit_refiles_the_row_in_place() {
		let conn = db();
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
		);

		// The edit, as the socket delivers it: fileNew with a fresh uuid and the same stable id,
		// plus the fileArchived retiring the old version.
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileNew(ev::FileNew(file_record(11, 2, 8)))
			),
			LocalApply::Applied(true)
		);
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileArchived(ev::FileArchived {
					uuid: uuid(1),
					stable_uuid: stable(2),
					new_uuid: Some(uuid(11)),
				})
			),
			LocalApply::Applied(false),
			"an archive naming a successor is the edit's own bookkeeping, not a removal"
		);

		assert_eq!(
			item_count(&conn),
			2,
			"the same row, re-filed — never a second one"
		);
		assert!(row(&conn, 1).is_none());
		assert!(row(&conn, 11).is_some());
	}

	/// A remote trash keeps the row — marked trashed, original parent preserved for the restore —
	/// and the retirement half of a versioning-disabled edit (which names a successor) must not
	/// flicker the live row through the trash.
	#[test]
	fn a_remote_trash_marks_the_row_and_a_supersede_does_not() {
		let conn = db();
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
		);

		// The supersede: never a trash. As on the wire, its stable id is the retired row's
		// freshly minted one, not the lineage's (2).
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileTrash(ev::FileTrash {
					uuid: uuid(1),
					stable_uuid: stable(99),
					new_uuid: Some(uuid(11)),
				})
			),
			LocalApply::Applied(false)
		);
		assert_eq!(row(&conn, 1), Some((Some(uuid(8)), false)));

		// The real trash: marked, parent kept.
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileTrash(ev::FileTrash {
					uuid: uuid(1),
					stable_uuid: stable(2),
					new_uuid: None,
				})
			),
			LocalApply::Applied(true)
		);
		assert_eq!(row(&conn, 1), Some((Some(uuid(8)), true)));

		// The replay another device's relist could produce: already where it belongs.
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileTrash(ev::FileTrash {
					uuid: uuid(1),
					stable_uuid: stable(2),
					new_uuid: None,
				})
			),
			LocalApply::Applied(false)
		);
	}

	/// A rename arriving AFTER an edit re-minted the uuid still lands: the patch is keyed by the
	/// whole-life id, which is the only name that survives the edit.
	#[test]
	fn a_rename_lands_across_a_uuid_remint() {
		let conn = db();
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(11, 2, 8))),
		);

		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileMetadataChanged(ev::FileMetadataChanged {
					// The event may still carry the OLD uuid; the stable id is what resolves.
					uuid: uuid(1),
					stable_uuid: stable(2),
					metadata: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("renamed-enc"))),
				})
			),
			LocalApply::Applied(true)
		);
		let raw: String = super::conn(&conn)
			.query_row(
				"SELECT files.raw_metadata FROM files
				JOIN items ON items.id = files.id WHERE items.uuid = ?1;",
				[uuid(11)],
				|r| r.get(0),
			)
			.unwrap();
		assert_eq!(raw, "renamed-enc");
	}

	/// The destructive arms answer with the follow-up they need rather than deleting here: the
	/// forget path owns the pending-edit guard, and the trash relist owns phantom reconciliation.
	/// The folds that mean "nothing died" must stay local no-ops.
	#[test]
	fn destructive_arms_defer_to_the_guarded_paths() {
		let conn = db();
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
		);

		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileDeletedPermanent(ev::FileDeletedPermanent {
					uuid: uuid(1),
					stable_uuid: None,
				})
			),
			LocalApply::Applied(false),
			"a versions-only purge must not touch the live head"
		);
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileDeletedPermanent(ev::FileDeletedPermanent {
					uuid: uuid(1),
					stable_uuid: Some(stable(2)),
				})
			),
			LocalApply::ForgetFile(stable(2))
		);
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileArchived(ev::FileArchived {
					uuid: uuid(1),
					stable_uuid: stable(2),
					new_uuid: None,
				})
			),
			LocalApply::ForgetFile(stable(2)),
			"an archive with NO successor is the lineage's death"
		);
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FolderDeletedPermanent(ev::FolderDeletedPermanent {
					uuid: uuid(8),
				})
			),
			LocalApply::ForgetDir(uuid(8))
		);

		// TrashEmpty: nothing trashed, nothing to do; something trashed, relist.
		assert_eq!(
			apply(&conn, DecryptedDriveEvent::TrashEmpty),
			LocalApply::Applied(false)
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileTrash(ev::FileTrash {
				uuid: uuid(1),
				stable_uuid: stable(2),
				new_uuid: None,
			}),
		);
		assert_eq!(
			apply(&conn, DecryptedDriveEvent::TrashEmpty),
			LocalApply::RelistTrash
		);
	}

	/// A move into a directory this cache has never listed still applies (the row is held), and
	/// the row ends up naming a dangling parent the provider resolves by asking about it — the
	/// documented one-level-per-ask behaviour.
	#[test]
	fn a_move_into_an_unheld_parent_applies_with_a_dangling_parent() {
		let conn = db();
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
		);

		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileMove(ev::FileMove(file_record(1, 2, 77)))
			),
			LocalApply::Applied(true)
		);
		assert_eq!(row(&conn, 1), Some((Some(uuid(77)), false)));
		assert!(
			row(&conn, 77).is_none(),
			"the destination itself stays unknown"
		);
	}

	/// Folder metadata and color changes patch rows we hold and drop for rows we do not.
	#[test]
	fn folder_patches_respect_the_gate() {
		let conn = db();
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FolderMetadataChanged(ev::FolderMetadataChanged {
					uuid: uuid(8),
					meta: DirectoryMeta::Encrypted(EncryptedString(Cow::Borrowed("new-enc"))),
				})
			),
			LocalApply::Applied(false)
		);
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
		);
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FolderMetadataChanged(ev::FolderMetadataChanged {
					uuid: uuid(8),
					meta: DirectoryMeta::Encrypted(EncryptedString(Cow::Borrowed("new-enc"))),
				})
			),
			LocalApply::Applied(true)
		);
		let raw: String = super::conn(&conn)
			.query_row(
				"SELECT dirs.raw_metadata FROM dirs
				JOIN items ON items.id = dirs.id WHERE items.uuid = ?1;",
				[uuid(8)],
				|r| r.get(0),
			)
			.unwrap();
		assert_eq!(raw, "new-enc");
	}
}

#[cfg(test)]
mod gap_gate_tests {
	use super::pass_owed;

	/// The regression the durable debt exists for: a pass that failed leaves the watermark
	/// behind, the next events carry it past the remote counter this cache ever compared
	/// against, and from then on the counters alone say "caught up" forever. The debt must
	/// outrank them — and, once a pass converges and clears it, stop forcing passes.
	#[test]
	fn an_owed_pass_runs_even_when_the_watermark_says_caught_up() {
		assert!(
			pass_owed(true, Some(50), 50),
			"a pass owed by a failed container (or apply) must run past a caught-up watermark"
		);
		assert!(
			pass_owed(true, Some(9000), 50),
			"a watermark advanced by later events must not swallow the debt"
		);
		assert!(
			!pass_owed(false, Some(50), 50),
			"nothing owed and nothing missed: no pass"
		);
		assert!(
			pass_owed(false, Some(49), 50),
			"a moved remote counter still runs a pass on its own"
		);
		assert!(
			pass_owed(false, None, 0),
			"a fresh cache has missed everything by definition"
		);
	}
}
