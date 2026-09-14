//! The engine orchestration: register pairs, plan a pass (read-only), and run one (plan + apply).
//!
//! `prepare` runs the read-only half — load the baseline, scan the local tree (fast-path),
//! enumerate the remote subtree from the cache, build the remote view — shared by `plan_pair` (a
//! dry run) and `sync_once` (plan + guard + apply + baseline advance).

use std::{
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	mem,
	path::{Path, PathBuf},
	sync::Arc,
	time::{Duration, Instant},
};

use chrono::Utc;
use filen_types::fs::ParentUuid;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
	Backlog, SyncEvent, SyncMode,
	apply::{self, ApplyContext, SyncReport},
	baseline::{
		BaselineEntry, BaselineState, BaselineStore, NodeKind, PairId, PairRecord, PathFailure,
		PendingRow,
	},
	guard::{self, DeleteGuard, GuardReason},
	outcome::{
		PlanOutcome, PlannedAction, PlannedConflict, PlannedNodeKind, RefuseReason, UnsyncablePath,
		UnsyncableReason, planned_action, planned_conflict,
	},
	pause::{PassControl, PassGate, PauseOptions, cancel_suspension},
	plan::{self, RemoteNode, RemoteView, SyncAction},
	scan::{self, LocalScan, ScanDepth, ScanError},
};
use crate::{
	Error, ErrorKind,
	auth::Client,
	cache::{CacheEvent, CacheEventType, DirEvent, FileEvent, SyncRootCallback, SyncRootHandle},
	fs::dir::cache::CacheableDir,
	fs::file::cache::CacheableFile,
	fs::{HasParent, HasUUID},
	io::client_impl::IoSharedClientExt,
};

/// The ceiling on how long a remote write this engine made stays trusted over the cache snapshot.
///
/// A write normally retires long before this: the cache announces the item (see [`Observations`])
/// or the snapshot shows what happened at its path. The window is the last resort for the case
/// where neither ever arrives — past it the snapshot is believed again, so an item genuinely
/// deleted elsewhere is picked up late, never ignored.
const PENDING_CREATE_GRACE: Duration = Duration::from_secs(180);

/// How many CONSECUTIVE failures at one path the engine retries before it stops planning that path
/// and reports it as [`UnsyncableReason::RepeatedFailure`].
///
/// Small on purpose: the point is to stop a permanently-broken path (a local file the OS will not
/// let us read, a remote item the account may not write) from failing on every pass forever, while
/// still riding out the transient failures a couple of retries cover. A path is unblocked by a
/// success, by [`SyncEngine::retry_path`], or — for one more attempt — once its last failure is
/// [`PATH_FAILURE_RETRY_INTERVAL`] old.
const MAX_PATH_FAILURES: u32 = 3;

/// How long a path whose failure streak ran out (see [`UnsyncableReason::RepeatedFailure`]) stays
/// unplanned before the engine tries it once more on its own.
///
/// The streak never simply expires: the first pass after the interval plans the path again, and
/// a failure there extends the streak and dates it anew, so the path waits another interval; a
/// success clears it. That is what lets a path that is only broken for a while — a file another
/// process holds locked, a file still being written, a server that kept failing — sync again with
/// nobody calling [`SyncEngine::retry_path`], at a cost of one attempt per interval for a path that
/// is broken for good.
pub const PATH_FAILURE_RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// How long [`SyncEngine::remove_pair`] waits for the pass it cancelled to end before it retires
/// the pair's rows anyway.
///
/// A cancelled pass unwinds in milliseconds — the transfer in flight is dropped and everything
/// behind it skipped — but an action the gate does not cover (a directory create, move or trash
/// already sent) has to come back from the server first. Past this the removal goes ahead: waiting
/// longer would hang a UI on a straggler, and a write from one lands on a pair id that no longer
/// exists, where its foreign key refuses it rather than let it into whatever pair sqlite gives that
/// id next.
const REMOVE_CANCEL_GRACE: Duration = Duration::from_secs(10);

/// How many levels [`SyncEngine::add_pair`]'s remote-ancestry walk climbs before it gives up.
///
/// A flat ceiling rather than a chain read in one request, because no such endpoint exists — the
/// walk is one `get_dir` per level, and a folder nested this deep is not somewhere a sync pair gets
/// registered. A chain that hits the ceiling is logged, and a nesting above it goes undetected
/// exactly as it did before the walk existed.
const MAX_REMOTE_ANCESTRY_DEPTH: usize = 64;

/// One remote write this engine made, and how to tell whether the cache has caught up to it.
#[derive(Debug)]
struct PendingWrite {
	at: Instant,
	/// The pair whose pass made the write. Only that pair's remote snapshot covers the item, so
	/// only that pair's pass may read a snapshot as evidence about it.
	pair: PairId,
	/// The observation counter as of the moment this write was recorded. Only a cache event seen
	/// AFTER that proves the cache caught up to THIS write: a move's uuid was necessarily
	/// announced earlier, when the item was first created.
	seq: u64,
	kind: PendingKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PendingKind {
	/// An item created at `path`. `replaced` is the remote uuid that held the path immediately
	/// before — a same-name upload versions the existing file, minting a new uuid — or `None` for
	/// a path nothing occupied.
	Created {
		path: String,
		replaced: Option<Uuid>,
	},
	/// An item moved from `from` to `to`. The source path is what makes "the cache has not applied
	/// our move yet" distinguishable from "someone else moved the item after us", which must be
	/// reconciled at once rather than waited out; the destination is where the view has to show it
	/// in the meantime.
	Moved { from: String, to: String },
	/// An item this engine sent to the remote trash. Its baseline row is gone, so a snapshot that
	/// has not applied the trash yet reads as "present remotely, untracked, absent locally" — a
	/// deletion to make, which would trash it a second time.
	Trashed,
}

/// How long a push of ours must stand as the remote head before the engine takes its content as
/// AGREED, with no snapshot ever having listed it.
///
/// A push proves only that the server took our bytes. What makes the content agreed is the remote
/// still holding it a while later — long enough that another client's edit of the same file would
/// have to have been made against our version rather than beside it. Ten seconds is that "a while":
/// comfortably above the socket round trip that announces a foreign write, and above the spread an
/// UNCONTENDED same-round race leaves between the two clients' versions — 0–6 s where the live
/// suite measures it — so a genuinely concurrent edit is still INSIDE the window and stays a
/// conflict. Past that spread the window only costs confirmations: a sequential edit made a few
/// seconds after ours is the ordinary case, and it should confirm without a pass having to catch
/// our version as the head.
///
/// What the window is NOT above is the account-wide drive-write lock. Two clients of one account
/// serialize their uploads on it, so the one that queues lands its version as far behind the other's
/// as its wait was long — the live suite has measured a same-round race spread 30 s apart, back when
/// a pass polled for the lock on the shared ladder's 30 s ceiling. A pass polls on
/// `PASS_LOCK_MAX_SLEEP` now, which bounds the part of that wait the acquisition adds; what no
/// ceiling can bound is the rest of the holder's batch, which runs on under the lock after its
/// upload for up to `PASS_LOCK_MAX_HOLD` (longer while a transfer started inside it finishes). A
/// race stretched past the window is, by every piece of evidence the engine has, an ordinary later
/// edit: our push confirms and the foreign version is pulled over our local copy, leaving only the
/// pusher's interleave check to surface it, and a CREATE race has no replaced version for that check
/// to read at all. That residual class is accepted; a shorter window widens it.
pub const CONFIRM_TENURE: Duration = Duration::from_secs(10);

/// The ceiling on ONE sleep of the drive-lock acquisition a pass makes, with the lock module's
/// default attempt count, so the whole wait's bound moves with it: 8640 polls of at most 5 s give
/// up after about 12 h where the default ladder allowed about 72 h, with the same `RetryFailed`
/// at the end.
///
/// A pass holds the account-wide drive-write lock for a batch of its apply at a time (see
/// [`PASS_LOCK_MAX_HOLD`]), so two clients of one account run their batches one after the other, and
/// the second one's version lands as far behind the first's as its wait for the lock was long. Past [`CONFIRM_TENURE`] that is no longer a race by
/// any evidence the engine has: the first client's push has tenured, so it pulls the second's
/// version over its own copy instead of surfacing a conflict, and the bytes it planned survive only
/// as a server version. The default ladder ([`crate::sync::lock`], capped at
/// [`MAX_SLEEP_TIME_DEFAULT`](crate::sync::lock::MAX_SLEEP_TIME_DEFAULT), 30 s per sleep) puts a
/// missed release well past the window on its own: a client that polls a moment too early sleeps out
/// its whole current step before it asks again. Half the window bounds that miss to a fraction of it
/// instead, for the price of more polls on a lock held through a long batch. Only the ENGINE's own
/// acquisition is bounded this way; every other caller of [`Client::lock_drive`] keeps the default
/// ladder.
///
/// It does NOT bound the whole gap between the two versions. The winner holds the lock until the end
/// of its batch, so its upload can already be older than the window by the time the loser is let in;
/// what this bounds is the part the acquisition itself adds — the wait AFTER the release.
pub(super) const PASS_LOCK_MAX_SLEEP: Duration = Duration::from_secs(5);

/// How long a pass keeps the account-wide drive-write lock before it lets it go at the next action
/// boundary where a release splits nothing (see `apply::release_points`), and takes it again before
/// its next remote write.
///
/// The lock is what makes every other device of the account wait to write, so a pass that held it
/// for its whole apply shut them out for as long as a large first sync ran — hours at the request
/// rate ceiling. A minute keeps the per-op acquire/release round trips the pass-wide lock replaced
/// down to one pair per batch, while nothing else on the account waits longer than about a minute
/// plus the transfers still finishing when it runs out: a transfer that started inside the batch is
/// never cut short to release, so one long upload stretches that batch to its own length.
pub(super) const PASS_LOCK_MAX_HOLD: Duration = Duration::from_secs(60);

/// How many remote writes a pass starts under one hold of the drive-write lock before it lets it go,
/// whichever of this and [`PASS_LOCK_MAX_HOLD`] comes first. Small writes at the request rate ceiling
/// fit a few hundred to a minute; this caps a batch of them on its own.
pub(super) const PASS_LOCK_MAX_BATCH: usize = 500;

/// How long a pass that let the drive-write lock go stays away from it before asking again.
///
/// The lock has no queue: whoever polls first after a release takes it. A pass asking again at once
/// would win nearly every time against a client sleeping out the default ladder
/// ([`MAX_SLEEP_TIME_DEFAULT`](crate::sync::lock::MAX_SLEEP_TIME_DEFAULT), 30 s per sleep), so the
/// release would free nothing. Staying away past one whole default sleep lets every waiting client
/// poll at least once; the extra 5 s covers the release itself, which is a request sent after the
/// lock is dropped. Only remote writes wait on it — local actions and downloads carry on meanwhile.
pub(super) const PASS_LOCK_YIELD: Duration =
	Duration::from_secs(crate::sync::lock::MAX_SLEEP_TIME_DEFAULT.as_secs() + 5);

/// The bounds a pass keeps each hold of the drive-write lock within. Always the three constants
/// above, except where a test shrinks them to make a release happen inside a small pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LockBudget {
	/// How long one hold lasts ([`PASS_LOCK_MAX_HOLD`]).
	pub(super) hold: Duration,
	/// How many remote writes one hold starts ([`PASS_LOCK_MAX_BATCH`]).
	pub(super) batch: usize,
	/// How long the pass stays away from the lock after letting it go ([`PASS_LOCK_YIELD`]).
	pub(super) rest: Duration,
}

impl Default for LockBudget {
	fn default() -> Self {
		Self {
			hold: PASS_LOCK_MAX_HOLD,
			batch: PASS_LOCK_MAX_BATCH,
			rest: PASS_LOCK_YIELD,
		}
	}
}

/// How long a push record survives with nothing ever resolving it.
///
/// A pass forgets the record of every row it persisted as agreed, so this only catches the ones no
/// pass will ever look at again — a row whose pair was removed, a conflict resolved out from under
/// it. Generous, because a paused pair's records are what confirm it on resume.
const PUSH_RECORD_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long the engine remembers that a version uuid came out of one of its OWN uploads.
///
/// Only the version chain of a path this engine has just written asks, and only about versions
/// around that write, so this needs to outlive a pass and nothing more. Sized like the pending-write
/// grace for the same reason: past it, a write of ours is not something a pass still reasons about.
const MINTED_TTL: Duration = PENDING_CREATE_GRACE;

/// What the cache's announcements say about one push of ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PushVerdict {
	/// Our version stood as the remote head for [`CONFIRM_TENURE`] before anything superseded it
	/// (or still stands): its content is what both sides hold.
	Confirmed,
	/// Evidence exists and does not confirm — something superseded our version inside the window,
	/// or it has not stood long enough yet.
	Unconfirmed,
	/// No evidence either way: the announcement was never seen, or this engine was restarted since.
	Unknown,
}

/// One push of ours awaiting confirmation: when the cache announced our version and when anything
/// superseded it. The difference is the tenure the confirmation rule reads.
#[derive(Debug)]
struct PushTenure {
	/// The file's whole-life id, so a NEW version of the same file recognisably supersedes ours
	/// even when no archive/trash event names our uuid (a resync, say).
	lineage: Option<filen_types::fs::StableUuid>,
	recorded: Instant,
	announced: Option<Instant>,
	superseded: Option<Instant>,
}

impl PushTenure {
	fn verdict(&self, now: Instant) -> PushVerdict {
		match self.stood_for(now) {
			Some(stood_for) if stood_for >= CONFIRM_TENURE => PushVerdict::Confirmed,
			Some(_) => PushVerdict::Unconfirmed,
			None => PushVerdict::Unknown,
		}
	}

	/// How long our version has stood as the remote head — until whatever superseded it, or until
	/// now if nothing has. `None` while the cache has not announced it at all.
	fn stood_for(&self, now: Instant) -> Option<std::time::Duration> {
		let announced = self.announced?;
		Some(
			self.superseded
				.unwrap_or(now)
				.saturating_duration_since(announced),
		)
	}
}

/// Uuids the cache has announced to this engine, each stamped with the observation counter and the
/// moment it arrived — plus the tenure of every push of ours still awaiting confirmation.
///
/// The counter is what retires a pending write in the ordinary case. Waiting for the written uuid
/// to APPEAR in a snapshot only works while the item survives, and two everyday events destroy it:
/// a re-upload (ours or another client's) versions the file under a fresh uuid, and a deletion
/// removes it outright. Either way the cache announces the uuid — as a `New`, a `Trashed`, an
/// `Archived` or a `Removed` — and that announcement alone proves the cache is no longer behind
/// on our write.
///
/// The timestamps are what confirms a push no snapshot ever caught as the head (see
/// [`CONFIRM_TENURE`]). Both are recorded on the cache worker thread, so this type never does more
/// than take a lock and look a couple of uuids up.
#[derive(Debug, Default)]
pub(super) struct Observations(std::sync::Mutex<ObservationState>);

#[derive(Debug, Default)]
struct ObservationState {
	seq: u64,
	seen: HashMap<Uuid, (u64, Instant)>,
	/// Pushes of ours whose content is not agreed yet, keyed by the version uuid the upload minted.
	///
	/// Only ever touched through [`ObservationState::insert_push`] and
	/// [`ObservationState::drop_push`], which keep `pushes_by_lineage` in step with it.
	pushes: HashMap<Uuid, PushTenure>,
	/// The same records' uuids grouped by the file they belong to, so an event about a file we
	/// pushed reaches its record without walking every push being watched — a first sync leaves one
	/// per uploaded file until the pass after it records what got confirmed.
	pushes_by_lineage: HashMap<filen_types::fs::StableUuid, Vec<Uuid>>,
	/// Every version uuid this engine's own uploads minted recently, confirmed or not.
	///
	/// The version chain cannot say who made a version and the server stamps them to the second,
	/// so our own previous upload and another client's concurrent edit are indistinguishable there
	/// whenever both fall in one second. This is the difference: a uuid in here came out of an
	/// upload of ours, so it is not an edit anybody interleaved (see
	/// [`plan::interleaved_version`]).
	///
	/// It is process-local, so a restart forgets what it wrote and a chain question asked
	/// within one second of a pre-restart upload can read that upload as a stranger's — one
	/// spurious conflict, cleared with KeepLocal. Persisting it would mean a table for a fact that
	/// matters for [`MINTED_TTL`].
	minted: HashMap<Uuid, Instant>,
}

impl Observations {
	/// Record every uuid a committed cache batch touched, and time-stamp what it says about a push
	/// of ours. Runs on the cache worker thread, so it does nothing but take a lock and insert.
	fn note(&self, events: &mut dyn Iterator<Item = &CacheEvent<'_>>) {
		let now = Instant::now();
		let mut state = self.state();
		for event in events {
			for uuid in event_uuids(event).into_iter().flatten() {
				state.see(uuid, now);
			}
			state.note_tenure(event, now);
		}
	}

	/// Record an announcement directly, for the tests that have no cache event to hand.
	#[cfg(test)]
	fn note_uuids(&self, uuids: impl IntoIterator<Item = Uuid>) {
		let now = Instant::now();
		let mut state = self.state();
		for uuid in uuids {
			state.see(uuid, now);
		}
	}

	/// Start watching a push of ours: it is unconfirmed until the cache says otherwise.
	///
	/// `replaced` is the version this upload went on top of — its own record, if it still has one,
	/// answers for a path the baseline has since moved past, so it goes.
	pub(super) fn watch_push(
		&self,
		uuid: Uuid,
		lineage: Option<filen_types::fs::StableUuid>,
		replaced: Option<Uuid>,
	) {
		let now = Instant::now();
		let mut state = self.state();
		if let Some(replaced) = replaced {
			state.drop_push(replaced);
		}
		// The cache can announce our own upload before the call that made it has returned; the
		// announcement map is the only place that moment survives.
		let announced = state.seen.get(&uuid).map(|(_, at)| *at);
		state.minted.insert(uuid, now);
		state.insert_push(
			uuid,
			PushTenure {
				lineage,
				recorded: now,
				announced,
				superseded: None,
			},
		);
	}

	/// What the announcements say about the push that minted `uuid`.
	fn push_verdict(&self, uuid: Uuid, now: Instant) -> PushVerdict {
		let state = self.state();
		let Some(push) = state.pushes.get(&uuid) else {
			return PushVerdict::Unknown;
		};
		let verdict = push.verdict(now);
		tracing::debug!(
			"sync engine: the push that minted {uuid} stood as the head for {:?} — {verdict:?}",
			push.stood_for(now)
		);
		verdict
	}

	/// Whether this engine's own upload minted `uuid` recently (see [`MINTED_TTL`]).
	pub(super) fn minted(&self, uuid: Uuid) -> bool {
		self.state().minted.contains_key(&uuid)
	}

	/// Forget the push records a pass has finished with, and any that no pass will ever read again.
	fn forget_pushes(&self, decided: &[Uuid], now: Instant) {
		let mut state = self.state();
		let expired: Vec<Uuid> = state
			.pushes
			.iter()
			.filter(|(_, push)| now.saturating_duration_since(push.recorded) >= PUSH_RECORD_TTL)
			.map(|(uuid, _)| *uuid)
			.collect();
		for uuid in decided.iter().chain(&expired) {
			state.drop_push(*uuid);
		}
		state
			.minted
			.retain(|_, at| now.saturating_duration_since(*at) < MINTED_TTL);
	}

	/// The current counter — the stamp a write recorded now must be beaten by to retire.
	fn stamp(&self) -> u64 {
		self.state().seq
	}

	/// A copy to settle against. Taken BEFORE the remote snapshot is read: an event committed
	/// after the snapshot must not retire a write this pass, or the pass would go on to read the
	/// just-written item as deleted.
	fn snapshot(&self) -> HashMap<Uuid, u64> {
		self.state()
			.seen
			.iter()
			.map(|(uuid, (seq, _))| (*uuid, *seq))
			.collect()
	}

	/// Forget the observations no live pending write can ever consult — only one stamped LATER
	/// than a write retires it, so everything up to the oldest live write's stamp is dead weight
	/// (and with no write left, so is the whole map).
	fn prune_before(&self, oldest: Option<u64>) {
		let mut state = self.state();
		match oldest {
			Some(seq) => state.seen.retain(|_, (stamp, _)| *stamp > seq),
			None => state.seen.clear(),
		}
	}

	fn state(&self) -> std::sync::MutexGuard<'_, ObservationState> {
		// Plain data behind the lock: a panic while holding it cannot leave the state inconsistent.
		self.0
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
	}
}

impl ObservationState {
	/// Stamp one announced uuid with the next observation counter and the moment it arrived.
	fn see(&mut self, uuid: Uuid, now: Instant) {
		self.seq += 1;
		let seq = self.seq;
		self.seen.insert(uuid, (seq, now));
	}

	/// Start watching a push, filed under its lineage as well as its own uuid.
	fn insert_push(&mut self, uuid: Uuid, push: PushTenure) {
		// A second record for one uuid would leave the first one's index entry behind.
		self.drop_push(uuid);
		if let Some(lineage) = push.lineage {
			self.pushes_by_lineage
				.entry(lineage)
				.or_default()
				.push(uuid);
		}
		self.pushes.insert(uuid, push);
	}

	/// Retire one push record, index entry and all. A stale entry there would either point at a
	/// record that is gone or keep a retired one reachable.
	fn drop_push(&mut self, uuid: Uuid) {
		let Some(lineage) = self.pushes.remove(&uuid).and_then(|push| push.lineage) else {
			return;
		};
		if let Some(watched) = self.pushes_by_lineage.get_mut(&lineage) {
			watched.retain(|watched| *watched != uuid);
			if watched.is_empty() {
				self.pushes_by_lineage.remove(&lineage);
			}
		}
	}

	/// Note that something else now stands where our push of this file stood.
	fn supersede_lineage(&mut self, lineage: filen_types::fs::StableUuid, now: Instant) {
		let Some(watched) = self.pushes_by_lineage.get(&lineage) else {
			return;
		};
		for uuid in watched {
			if let Some(push) = self.pushes.get_mut(uuid) {
				push.superseded.get_or_insert(now);
			}
		}
	}

	/// Time-stamp what one cache event says about the pushes being watched: a live announcement of
	/// our own version, or something taking its place.
	///
	/// Both are map lookups: this runs on the cache worker thread, for every event of every
	/// committed batch, while a first sync can leave one watched push per uploaded file.
	fn note_tenure(&mut self, event: &CacheEvent<'_>, now: Instant) {
		let CacheEventType::File(file) = &event.event else {
			return;
		};
		match file {
			FileEvent::New(f) | FileEvent::Changed(f) | FileEvent::Move(f) => {
				if let Some(push) = self.pushes.get_mut(&f.uuid) {
					push.announced.get_or_insert(now);
					return;
				}
				// A different version of a file we pushed: whatever announced it, ours is no longer
				// what the remote holds.
				self.supersede_lineage(f.stable_uuid, now);
			}
			// An edit (either versioning mode) or a deletion of our own version: either way it stopped
			// standing at this moment.
			FileEvent::Archived { uuid, .. } | FileEvent::Trashed { uuid, .. } => {
				if let Some(push) = self.pushes.get_mut(uuid) {
					push.superseded.get_or_insert(now);
				}
			}
			FileEvent::Removed(_) | FileEvent::MetadataChanged { .. } => {}
		}
	}
}

/// The uuids one cache event touches: the item's own, plus the successor a trash or archive of an
/// edited file carries (the same file re-minted under a new id).
fn event_uuids(event: &CacheEvent<'_>) -> [Option<Uuid>; 2] {
	match &event.event {
		CacheEventType::File(file) => match file {
			FileEvent::New(f) | FileEvent::Move(f) | FileEvent::Changed(f) => [Some(f.uuid), None],
			FileEvent::Archived { uuid, new_uuid, .. }
			| FileEvent::Trashed { uuid, new_uuid, .. } => [Some(*uuid), *new_uuid],
			FileEvent::Removed(uuid) | FileEvent::MetadataChanged { uuid, .. } => {
				[Some(*uuid), None]
			}
		},
		CacheEventType::Dir(dir) => match dir {
			DirEvent::New(d) | DirEvent::Move(d) | DirEvent::Changed(d) => [Some(d.uuid), None],
			DirEvent::Removed(uuid)
			| DirEvent::MetadataChanged { uuid, .. }
			| DirEvent::ColorChanged { uuid, .. } => [Some(*uuid), None],
		},
		CacheEventType::Global(_) | CacheEventType::NoOp => [None, None],
	}
}

/// The sync-root callback the engine registers per pair: it records the uuids of every committed
/// cache batch and nothing else. It runs on the cache worker thread, so it never awaits and never
/// touches the database.
fn observation_callback(observations: Arc<Observations>) -> SyncRootCallback {
	Box::new(move |events| observations.note(events))
}

/// Read what the cache's announcements say about every unconfirmed push in `baseline`: the version
/// uuids they confirm, and the rows (`rel_path`, ours, the foreign version at the path) they say
/// nothing about that are worth asking the server about.
///
/// Purely a READ of the observations. The records it consulted stay put, because the caller may be
/// a dry run: [`SyncEngine::plan_pair`] reaches this through `prepare` and persists nothing, and a
/// record dropped there would leave the pass that follows with no evidence at all that the push was
/// ever confirmed. Only the callers that write the advanced rows retire the records
/// ([`Observations::forget_pushes`]).
fn observed_confirmations(
	observed: &Observations,
	baseline: &HashMap<String, BaselineEntry>,
	raw_remote: &HashMap<String, RemoteNode>,
	now: Instant,
) -> (std::collections::HashSet<Uuid>, Vec<(String, Uuid, Uuid)>) {
	let mut confirmed = std::collections::HashSet::new();
	// rel_path is only for the log; the lookup runs on the foreign version sitting at it.
	let mut ask_server = Vec::new();
	for (rel_path, entry) in baseline.iter() {
		if !plan::awaits_confirmation(entry) {
			continue;
		}
		let Some(ours) = entry.remote_uuid else {
			continue;
		};
		match observed.push_verdict(ours, now) {
			PushVerdict::Confirmed => {
				confirmed.insert(ours);
			}
			PushVerdict::Unconfirmed => {}
			// Nothing observed. Worth a round trip only where the answer changes this pass:
			// a foreign version of the same file already sitting at the row's path.
			PushVerdict::Unknown => {
				if let Some(node) = raw_remote.get(rel_path)
					&& node.remote_uuid != ours
					&& node.stable_uuid.is_some()
					&& node.stable_uuid == entry.remote_stable_uuid
				{
					ask_server.push((rel_path.clone(), ours, node.remote_uuid));
				}
			}
		}
	}
	(confirmed, ask_server)
}

/// Remote writes this engine made recently. The apply layer records every item it creates or
/// moves; [`settle`](PendingWrites::settle) drops the records the cache has demonstrably caught up
/// to and the ones past [`PENDING_CREATE_GRACE`], so the map only ever holds one grace window of
/// writes.
#[derive(Debug, Default)]
pub(super) struct PendingWrites(std::sync::Mutex<HashMap<Uuid, PendingWrite>>);

impl PendingWrites {
	/// Record a remote write this pass just made. The apply layer persists the same write to the
	/// baseline DB FIRST (see [`BaselineStore::record_pending`]), so a restart re-learns it.
	pub(super) fn record(
		&self,
		observations: &Observations,
		pair: PairId,
		uuid: Uuid,
		kind: PendingKind,
	) {
		self.insert(pair, uuid, kind, observations.stamp(), Instant::now());
	}

	/// Take a write a PREVIOUS engine journalled back into memory, `age` after it was made.
	///
	/// Its stamp is 0: this process has observed nothing yet, so the first cache announcement of
	/// the uuid — stamped 1 or later — retires it, exactly as it would have retired the original.
	/// The announcements that arrived before the restart are gone with the process, so a write
	/// whose item is no longer in a snapshot (superseded, trashed) now waits for a fresh
	/// announcement, a foreign uuid at its path, or the grace ceiling — bounded, and never longer
	/// than a write made right now.
	pub(super) fn restore(&self, pair: PairId, uuid: Uuid, kind: PendingKind, age: Duration) {
		// `checked_sub` only fails for an age older than this machine's uptime — a journal carried
		// across a reboot — and its fallback is the conservative answer anyway: a full grace
		// window from now.
		let at = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
		self.insert(pair, uuid, kind, 0, at);
	}

	fn insert(&self, pair: PairId, uuid: Uuid, kind: PendingKind, seq: u64, at: Instant) {
		self.map().insert(
			uuid,
			PendingWrite {
				at,
				pair,
				seq,
				kind,
			},
		);
	}

	/// The uuids currently journalled, for diffing what a [`settle`](Self::settle) retired.
	fn uuids(&self) -> std::collections::HashSet<Uuid> {
		self.map().keys().copied().collect()
	}

	/// Drop every record for `pair` — the pair is gone, so nothing may fold on its behalf (a
	/// re-registration of the same roots gets the same id back).
	fn forget_pair(&self, pair: PairId) {
		self.map().retain(|_, write| write.pair != pair);
	}

	/// The oldest live write's observation stamp, for pruning [`Observations`].
	fn oldest_stamp(&self) -> Option<u64> {
		self.map().values().map(|write| write.seq).min()
	}

	/// Drop the records the cache has demonstrably caught up to and the ones past
	/// [`PENDING_CREATE_GRACE`], and return what the reconciler still has to suppress on their
	/// behalf. Whether a surviving create may actually correct the view is [`fold_create`]'s
	/// question, not this one's.
	///
	/// `observed` must have been copied BEFORE `remote` was read, so a cache batch committed after
	/// the snapshot cannot retire a record whose item that snapshot still predates. `remote` is
	/// `pair`'s subtree alone, so it is only evidence about `pair`'s own writes.
	fn settle(
		&self,
		pair: PairId,
		observed: &HashMap<Uuid, u64>,
		remote: &HashMap<String, RemoteNode>,
	) -> plan::PassHolds {
		let now = Instant::now();
		let snapshot_path: HashMap<Uuid, &str> = remote
			.iter()
			.map(|(path, node)| (node.remote_uuid, path.as_str()))
			.collect();
		let mut map = self.map();
		map.retain(|uuid, write| {
			// The cache announced this uuid after we wrote it: it has caught up, whether the item
			// still exists, was superseded by a re-upload, or has since been trashed.
			if observed.get(uuid).is_some_and(|stamp| *stamp > write.seq) {
				return false;
			}
			if now.duration_since(write.at) >= PENDING_CREATE_GRACE {
				return false;
			}
			// Another pair's write: this snapshot covers a different remote subtree and says
			// nothing about it, least of all that it is gone. (The two rules above are about the
			// uuid alone, so they hold whichever pair's pass applies them.)
			if write.pair != pair {
				return true;
			}
			match &write.kind {
				PendingKind::Created { .. } => {
					// The snapshot lists what we wrote: the cache has caught up. What it lists
					// INSTEAD is a question of identity — the version our upload superseded reads
					// exactly like somebody else's write from here — and the row that answers it is
					// the fold's ([`fold_create`]), which refuses to paint over an item that is not
					// ours. Keeping the record until then costs nothing: the announcement above or
					// the grace ceiling retires it either way.
					!snapshot_path.contains_key(uuid)
				}
				PendingKind::Moved { from, .. } => snapshot_path
					.get(uuid)
					.is_none_or(|path| *path == from.as_str()),
				PendingKind::Trashed => snapshot_path.contains_key(uuid),
			}
		});
		plan::PassHolds {
			trashed: map
				.iter()
				.filter(|(_, write)| {
					write.pair == pair && matches!(write.kind, PendingKind::Trashed)
				})
				.map(|(uuid, _)| *uuid)
				.collect(),
			..Default::default()
		}
	}

	/// This pair's pending CREATES whose path the snapshot shows another version of the same file
	/// at — neither what we wrote nor the version we replaced. Each is
	/// `(record uuid, path, the version our row records, the stranger)`.
	///
	/// This is the one state where [`fold_create`] paints over something it cannot identify from
	/// the record alone, so it is the one worth asking the server about (see
	/// `SyncEngine::retire_superseded_creates`). Everything else the fold decides for itself.
	fn strangers(
		&self,
		pair: PairId,
		baseline: &HashMap<String, BaselineEntry>,
		nodes: &HashMap<String, RemoteNode>,
	) -> Vec<(Uuid, String, Uuid, Uuid)> {
		self.map()
			.iter()
			.filter(|(_, write)| write.pair == pair)
			.filter_map(|(uuid, write)| {
				let PendingKind::Created { path, replaced } = &write.kind else {
					return None;
				};
				let ours = written_node(baseline.get(path))?;
				let current = nodes.get(path)?;
				if current.remote_uuid == ours.remote_uuid
					|| current.remote_uuid == *uuid
					|| Some(current.remote_uuid) == *replaced
				{
					return None;
				}
				(current.stable_uuid.is_some() && current.stable_uuid == ours.stable_uuid)
					.then(|| (*uuid, path.clone(), ours.remote_uuid, current.remote_uuid))
			})
			.collect()
	}

	/// Drop one record: whatever it was waiting for has been answered another way.
	fn retire(&self, uuid: Uuid) {
		self.map().remove(&uuid);
	}

	/// Fold this pair's surviving records into `nodes` — the path-keyed remote view built from the
	/// cache snapshot — and return how many were applied.
	///
	/// The cache learns of this engine's own writes only through socket events and resyncs, so a
	/// pass run seconds after the previous one reads a remote that is missing what that pass just
	/// wrote. The engine knows exactly what it wrote, so its remote view is the snapshot PLUS its
	/// own unacknowledged writes: that is what lets the pass act on the path — push a second edit,
	/// carry the file on — instead of leaving it alone until the cache agrees.
	///
	/// `baseline` supplies the written state: the apply layer writes each row immediately after
	/// the write it describes, so the row IS the post-write truth. A record only survives
	/// [`settle`](Self::settle) while the cache demonstrably still shows the pre-write state, so a
	/// fold never paints over somebody else's write.
	pub(super) fn fold_into(
		&self,
		pair: PairId,
		baseline: &HashMap<String, BaselineEntry>,
		nodes: &mut HashMap<String, RemoteNode>,
	) -> usize {
		let map = self.map();
		let mut writes: Vec<(&Uuid, &PendingWrite)> =
			map.iter().filter(|(_, write)| write.pair == pair).collect();
		if writes.is_empty() {
			return 0;
		}
		// Oldest first: two writes can name one path (a re-upload on the very next pass), and the
		// later one has to land on top.
		writes.sort_unstable_by_key(|(_, write)| write.at);
		// uuid -> where the view holds it, kept current as the fold edits the view; a scan per
		// record would be quadratic on a large tree right after a large pass.
		let mut path_of: HashMap<Uuid, String> = nodes
			.iter()
			.map(|(path, node)| (node.remote_uuid, path.clone()))
			.collect();
		let mut folded = 0;
		for (uuid, write) in writes {
			let applied = match &write.kind {
				PendingKind::Created { path, replaced } => {
					fold_create(nodes, &mut path_of, baseline, *uuid, path, *replaced)
				}
				PendingKind::Moved { from, to } => {
					fold_move(nodes, &mut path_of, baseline, *uuid, from, to)
				}
				PendingKind::Trashed => fold_trash(nodes, &mut path_of, *uuid),
			};
			folded += usize::from(applied);
		}
		folded
	}

	fn map(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, PendingWrite>> {
		// Plain data behind the lock: a panic while holding it cannot leave the map inconsistent.
		self.0
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
	}
}

/// [`PENDING_CREATE_GRACE`] as the journal stores it: wall-clock millis.
fn grace_millis() -> i64 {
	PENDING_CREATE_GRACE.as_millis() as i64
}

/// Take the journal a previous engine persisted back into memory, each row aged by the wall clock
/// so it expires on the same ceiling as a write recorded in this process.
/// [`BaselineStore::load_pending`] has already dropped the rows that ceiling ran out on; the clamp
/// here only guards a clock that moved between the two.
fn restore_journal(pending: &PendingWrites, rows: Vec<PendingRow>, now: i64) {
	for row in rows {
		let age = now.saturating_sub(row.recorded_at).clamp(0, grace_millis()) as u64;
		pending.restore(row.pair, row.uuid, row.kind, Duration::from_millis(age));
	}
}

/// The remote node a baseline row describes — the state the write it records left behind.
fn written_node(entry: Option<&BaselineEntry>) -> Option<RemoteNode> {
	let entry = entry?;
	Some(RemoteNode {
		rel_path: entry.rel_path.clone(),
		kind: entry.kind,
		remote_uuid: entry.remote_uuid?,
		stable_uuid: entry.remote_stable_uuid,
		content_hash: entry.content_hash,
		size: entry.size.unwrap_or(0),
		modified_millis: entry.remote_modified.unwrap_or(0),
	})
}

/// Put `node` at `path`, keeping the uuid index in step with whatever it displaces.
fn place_node(
	nodes: &mut HashMap<String, RemoteNode>,
	path_of: &mut HashMap<Uuid, String>,
	path: String,
	node: RemoteNode,
) {
	let uuid = node.remote_uuid;
	if let Some(previous) = nodes.insert(path.clone(), node) {
		path_of.remove(&previous.remote_uuid);
	}
	path_of.insert(uuid, path);
}

/// Show what we wrote at `path`, over the version it superseded there — and decide, for a path
/// showing something else entirely, whether this engine may claim it at all. [`PendingWrites`]
/// keeps a create's record while the cache has announced nothing of it; this is where what the
/// snapshot shows at its path is weighed against it.
///
/// The node comes from the baseline row at `path` whatever uuid that row names, not from the
/// record's own uuid: a second write to the same path inside the window supersedes the first, and
/// the row is where the latest write recorded itself, so both records fold the same node and the
/// first one to run wins.
fn fold_create(
	nodes: &mut HashMap<String, RemoteNode>,
	path_of: &mut HashMap<Uuid, String>,
	baseline: &HashMap<String, BaselineEntry>,
	uuid: Uuid,
	path: &str,
	replaced: Option<Uuid>,
) -> bool {
	let Some(node) = written_node(baseline.get(path)) else {
		return false;
	};
	match nodes.get(path) {
		// The cache is showing what our baseline records: it has caught up.
		Some(current) if current.remote_uuid == node.remote_uuid => return false,
		// Ours — the write itself, or the version it superseded — so the cache is behind on us.
		Some(current) if current.remote_uuid == uuid || Some(current.remote_uuid) == replaced => {}
		// Another VERSION of the same file. A record only reaches the fold while the cache has
		// announced nothing of our write, and the cache cannot have applied a version that landed
		// AFTER ours without announcing ours on the way — so what it shows here is a version our
		// upload went on top of, whichever client made it. Left standing it reads as a foreign edit
		// over an unconfirmed push and conflicts the client whose copy is the remote head. (A
		// resync that skips straight past our version can still show a genuinely newer one; that
		// costs a pass or two at this path until the grace ceiling retires the record.)
		Some(current)
			if current.stable_uuid.is_some() && current.stable_uuid == node.stable_uuid => {}
		None => {}
		// Somebody else's item — a different file has taken the path over, and the pass has to
		// reconcile against it at once.
		Some(_) => return false,
	}
	place_node(nodes, path_of, path.to_string(), node);
	true
}

/// Show the item we moved at its destination rather than where the cache still lists it. A
/// directory — see [`plan::fold_dir_moves`] — carries the subtree the cache still lists under its
/// old path along with it.
fn fold_move(
	nodes: &mut HashMap<String, RemoteNode>,
	path_of: &mut HashMap<Uuid, String>,
	baseline: &HashMap<String, BaselineEntry>,
	uuid: Uuid,
	from: &str,
	to: &str,
) -> bool {
	let vacated = match path_of.get(&uuid).map(String::as_str) {
		// Already where we moved it.
		Some(at) if at == to => return false,
		// The pre-move path the cache still shows: take the node off it, whatever the destination
		// turns out to hold — the one thing this move makes certain is that the item is not here.
		Some(at) if at == from => {
			path_of.remove(&uuid);
			nodes.remove(from)
		}
		// Somewhere else entirely — not our move lagging, so the snapshot stands.
		Some(_) => return false,
		None => None,
	};
	// Somebody else's item holds the destination: it superseded ours there while the move was in
	// flight, so leave the path showing what really sits on it. Vacating the pre-move path was
	// still right — without it the reconciler reads the item we moved as an untracked remote entry
	// and trashes it.
	if nodes.get(to).is_some_and(|node| node.remote_uuid != uuid) {
		return vacated.is_some();
	}
	let carries_subtree = vacated
		.as_ref()
		.is_some_and(|node| node.kind == NodeKind::Dir);
	let node =
		vacated.or_else(|| written_node(baseline.get(to)).filter(|node| node.remote_uuid == uuid));
	let Some(mut node) = node else {
		return false;
	};
	node.rel_path = to.to_string();
	place_node(nodes, path_of, to.to_string(), node);
	if carries_subtree {
		let children: Vec<String> = nodes
			.keys()
			.filter(|key| plan::is_under(key, from))
			.cloned()
			.collect();
		for old in children {
			let Some(mut child) = nodes.remove(&old) else {
				continue;
			};
			let new = format!("{to}{}", &old[from.len()..]);
			child.rel_path = new.clone();
			place_node(nodes, path_of, new, child);
		}
	}
	true
}

/// Take the item we trashed out of the view — and, for a directory, everything under it, which the
/// server trashed with it.
fn fold_trash(
	nodes: &mut HashMap<String, RemoteNode>,
	path_of: &mut HashMap<Uuid, String>,
	uuid: Uuid,
) -> bool {
	let Some(path) = path_of.remove(&uuid) else {
		return false;
	};
	let Some(node) = nodes.remove(&path) else {
		return false;
	};
	if node.kind == NodeKind::Dir {
		nodes.retain(|key, node| {
			let keep = !plan::is_under(key, &path);
			if !keep {
				path_of.remove(&node.remote_uuid);
			}
			keep
		});
	}
	true
}

/// A configured sync engine: an `Arc<Client>` (whose cache supplies the remote view) plus the
/// per-pair baseline store.
pub struct SyncEngine {
	pub(super) client: Arc<Client>,
	pub(super) store: Mutex<BaselineStore>,
	/// Remote writes this engine made that the cache may not reflect yet (see [`PendingWrites`]).
	pub(super) pending: PendingWrites,
	/// Uuids the cache has announced, shared with the per-pair sync-root callbacks — the evidence
	/// that retires a pending write (see [`Observations`]).
	pub(super) observed: Arc<Observations>,
	/// One cache sync-root registration per pair, kept alive for as long as the pair is
	/// registered; dropping a handle unsubscribes it.
	roots: Mutex<HashMap<PairId, SyncRootHandle>>,
	/// One-shot mass-delete approvals: pair -> the batch token the caller approved. The next pass
	/// whose held batch hashes to that token executes it; any other batch is held again.
	approvals: Mutex<HashMap<PairId, String>>,
	/// Serializes [`add_pair`](SyncEngine::add_pair): its overlap check reads the registry and then
	/// inserts into it, with an `await` in between, so two concurrent registrations would otherwise
	/// both find the roots free and both commit — the exact overlap the check exists to refuse.
	registrations: Mutex<()>,
	/// The live pause state of every pair the engine has had to look at, one
	/// [`watch`](tokio::sync::watch) channel each (see [`SyncEngine::pause_pair_with`]). It mirrors
	/// the persisted `paused` column — which stays the source of truth across restarts — and adds
	/// what the pass IN FLIGHT must do about the pause, which is process-local by nature: a restart
	/// has no pass to suspend. Bounded by the pair count; an entry goes with its pair.
	paused: Mutex<HashMap<PairId, tokio::sync::watch::Sender<PassControl>>>,
	/// One removal signal per pair that has been watched: [`remove_pair`](SyncEngine::remove_pair)
	/// flips it and drops it, so the pair's watch loops stop instead of failing every pass against a
	/// pair that no longer exists — and a pair re-registered under the same id (sqlite reuses one)
	/// gets a fresh signal rather than an already-tripped one.
	removals: Mutex<HashMap<PairId, tokio::sync::watch::Sender<bool>>>,
	/// The bounds of each hold of the drive-write lock a pass takes (see [`LockBudget`]).
	lock_budget: LockBudget,
	/// One lock per pair, held by a pass from before it reads the baseline until it has recorded the
	/// conflicts it found, and by [`resolve_conflict`](SyncEngine::resolve_conflict) for the whole
	/// resolution — so a resolution never lands between a pass's read and the conflict rows it writes
	/// from that read. `Arc`: the guard is held across awaits after the map's own lock is released.
	/// Bounded by the pair count; an entry goes with its pair.
	reading: Mutex<HashMap<PairId, Arc<Mutex<()>>>>,
}

#[cfg(feature = "malformed")]
impl SyncEngine {
	/// Test-only: bound each hold of the drive-write lock a pass takes to `hold` or `batch` remote
	/// writes, whichever comes first, and stay away from the lock for `rest` after letting it go —
	/// so a pass small enough for a test still releases the lock part-way. Every production engine
	/// keeps `PASS_LOCK_MAX_HOLD`, `PASS_LOCK_MAX_BATCH` and `PASS_LOCK_YIELD`.
	pub fn set_lock_budget(&mut self, hold: Duration, batch: usize, rest: Duration) {
		self.lock_budget = LockBudget { hold, batch, rest };
	}
}

/// Why [`SyncEngine::add_pair`] refused to register a pair: its roots overlap one already
/// registered, so the two pairs would fight over the same items — each reading the other's writes
/// as foreign changes, re-uploading and re-deleting them without ever converging.
///
/// Carried as the source of the returned [`Error`], so a caller that wants to react per case can
/// recover it with [`Error::downcast`].
///
/// # How remote nesting is established
///
/// Two uuids only relate through their ancestry. The CACHE answers that for free, but only for what
/// it has enumerated — a folder created seconds ago is not in it yet — so what the cache cannot
/// prove is asked of the SERVER, one [`Client::get_dir`] per level up to the account root. That
/// makes the nested-remote checks exact rather than best-effort, at the price of
/// [`SyncEngine::add_pair`] needing the network: a server error refuses the registration instead of
/// registering a pair whose roots were never actually checked.
#[derive(Debug, thiserror::Error)]
pub enum PairOverlap {
	/// Another pair already syncs this exact local folder.
	#[error("sync pair {pair} already syncs the local root {existing:?}")]
	LocalRootInUse { pair: PairId, existing: String },
	/// The new local root is INSIDE an existing pair's local root.
	#[error("the local root is inside sync pair {pair}'s root {existing:?}")]
	LocalRootNested { pair: PairId, existing: String },
	/// The new local root CONTAINS an existing pair's local root.
	#[error("the local root contains sync pair {pair}'s root {existing:?}")]
	LocalRootContains { pair: PairId, existing: String },
	/// Another pair already syncs this exact remote folder.
	#[error("sync pair {pair} already syncs the remote root {existing}")]
	RemoteRootInUse { pair: PairId, existing: Uuid },
	/// The new remote root is INSIDE an existing pair's remote root.
	#[error("the remote root is inside sync pair {pair}'s remote root {existing}")]
	RemoteRootNested { pair: PairId, existing: Uuid },
	/// The new remote root CONTAINS an existing pair's remote root.
	#[error("the remote root contains sync pair {pair}'s remote root {existing}")]
	RemoteRootContains { pair: PairId, existing: Uuid },
}

/// How a candidate local root relates to an existing pair's, or `None` when they are disjoint.
/// Both paths are canonical (every stored root was canonicalized when it was registered), so a
/// prefix comparison is a real containment test rather than a string coincidence.
fn local_overlap(candidate: &Path, existing: &Path, pair: PairId) -> Option<PairOverlap> {
	let shown = || existing.to_string_lossy().into_owned();
	if candidate == existing {
		Some(PairOverlap::LocalRootInUse {
			pair,
			existing: shown(),
		})
	} else if candidate.starts_with(existing) {
		Some(PairOverlap::LocalRootNested {
			pair,
			existing: shown(),
		})
	} else if existing.starts_with(candidate) {
		Some(PairOverlap::LocalRootContains {
			pair,
			existing: shown(),
		})
	} else {
		None
	}
}

/// How a candidate remote root relates to an existing pair's, or `None` when they are disjoint.
///
/// The same test as [`local_overlap`] with the two roots' upward ancestor chains — each the root
/// itself followed by every ancestor up to the account root — standing in for path prefixes. A
/// chain short of the account root only ever proves a relation, never the absence of one, which is
/// why the caller runs this over the cache's partial chains for a refusal and over
/// [`SyncEngine::server_ancestry`]'s complete ones before it accepts.
fn remote_overlap(
	candidate: Uuid,
	candidate_chain: &[Uuid],
	existing: Uuid,
	existing_chain: &[Uuid],
	pair: PairId,
) -> Option<PairOverlap> {
	if candidate == existing {
		Some(PairOverlap::RemoteRootInUse { pair, existing })
	} else if candidate_chain.contains(&existing) {
		Some(PairOverlap::RemoteRootNested { pair, existing })
	} else if existing_chain.contains(&candidate) {
		Some(PairOverlap::RemoteRootContains { pair, existing })
	} else {
		None
	}
}

/// The read-only inputs to a pass, shared by planning and applying.
struct Prepared {
	record: PairRecord,
	baseline: Arc<HashMap<String, BaselineEntry>>,
	/// The directory moves this pass makes, in the order they run (see [`plan::fold_dir_moves`]).
	/// `baseline`, `local_scan` and `remote_view` are already keyed by the paths those subtrees
	/// end up at.
	dir_moves: Vec<SyncAction>,
	local_scan: LocalScan,
	remote_view: RemoteView,
	/// Whether the cache's remote view has converged at least once (the snapshot carried a
	/// watermark). When false, the snapshot's emptiness is untrustworthy and remote-driven
	/// deletions are held by the guard.
	remote_converged: bool,
	/// Whether the cache SNAPSHOT — before this engine's own unacknowledged writes were folded
	/// into it — was wholly empty while the baseline still tracks remote items: what a transient
	/// backend/cache fault looks like. Read from the snapshot, never the folded view, so folding
	/// our own writes back in cannot mask the fault and let the guard through.
	remote_emptied: bool,
	/// What this pass must not act on (see [`plan::PassHolds`]).
	holds: plan::PassHolds,
	/// The per-path failure streaks that block planning this pass (see [`streak_blocks`]). A streak
	/// below the threshold, or one whose retry interval has run out, is not here.
	failures: HashMap<String, PathFailure>,
	/// Synced paths whose remote item still exists but is out of the view (see
	/// [`plan::unknown_remote_paths`]), with why. Blocked and reported, never read as deleted.
	unknown_remote: BTreeMap<String, UnsyncableReason>,
	/// Remote items out of the view that no baseline row names: reported only.
	never_synced_remote: Vec<UnsyncablePath>,
	/// Baseline rows whose agreed-content marker this pass's raw snapshot advanced (see
	/// [`plan::confirm_agreed_content`]). Already applied to `baseline`, so planning reads them
	/// either way; a real pass persists them, a dry run writes nothing.
	confirmed: Vec<BaselineEntry>,
	dirs: Vec<CacheableDir<'static>>,
	files: Vec<CacheableFile<'static>>,
}

impl Prepared {
	/// Carry each directory move across as one move — a case-only rename included — and read the
	/// rest of the pass where those subtrees end up (see [`plan::fold_dir_moves`]).
	fn fold_dir_moves(&mut self) {
		self.dir_moves = plan::fold_dir_moves(
			self.record.mode,
			Arc::make_mut(&mut self.baseline),
			&mut self.local_scan.nodes,
			&mut self.remote_view.nodes,
			&self.holds.held_remote,
		);
		// What this pass blocks and reports is keyed by path too, and has to follow the fold, or a
		// block stays behind at a path nothing is keyed by any more while its item reads as absent at
		// the new one: a remote item the view cannot place would have its local copy quarantined.
		// Replayed in order, each move re-keys exactly what the fold re-keyed for it.
		for action in &self.dir_moves {
			let (from, to) = action.endpoints();
			// Keyed like the baseline rows they were read from. A park on the moved directory itself
			// is cleared by its rename, as any rename clears one; carried along, it would hold back the
			// very move that renames it.
			rekey_paths(&mut self.unknown_remote, from, to);
			self.failures.remove(from);
			rekey_paths(&mut self.failures, from, to);
			if matches!(action, SyncAction::MoveRemote { .. }) {
				// Keyed like the remote view, which only a remote move re-keys.
				for report in &mut self.never_synced_remote {
					if let Some(path) = plan::moved_path(&report.rel_path, from, to) {
						report.rel_path = path;
					}
				}
			} else {
				// Keyed like the local scan, which only a local move re-keys.
				rekey_paths(&mut self.local_scan.invalid_names, from, to);
				rekey_paths(&mut self.local_scan.aliased_dirs, from, to);
				for target in self.local_scan.aliased_dirs.values_mut() {
					if let Some(path) = plan::moved_path(target, from, to) {
						*target = path;
					}
				}
			}
		}
	}

	/// Map internal actions onto the public [`PlannedAction`] shape, resolving each one's size from
	/// whichever side of this pass knows it.
	fn planned(&self, actions: &[SyncAction]) -> Vec<PlannedAction> {
		actions
			.iter()
			.map(|action| planned_action(action, &self.local_scan.nodes, &self.remote_view.nodes))
			.collect()
	}

	/// Every path this pass will not act on, and why — reported identically by the dry run and by
	/// the pass itself, so a caller sees the same list either way.
	fn unsyncable(&self) -> Vec<UnsyncablePath> {
		let names = self
			.local_scan
			.invalid_names
			.iter()
			.map(|(rel_path, detail)| UnsyncablePath {
				rel_path: rel_path.clone(),
				reason: UnsyncableReason::InvalidName {
					detail: detail.clone(),
				},
			});
		let mut all: Vec<UnsyncablePath> = names
			.chain(
				self.failures
					.iter()
					.map(|(rel_path, failure)| UnsyncablePath {
						rel_path: rel_path.clone(),
						reason: UnsyncableReason::RepeatedFailure {
							attempts: failure.attempts,
							last_error: failure.last_error.clone(),
						},
					}),
			)
			.chain(
				self.unknown_remote
					.iter()
					.map(|(rel_path, reason)| UnsyncablePath {
						rel_path: rel_path.clone(),
						reason: reason.clone(),
					}),
			)
			.chain(self.never_synced_remote.iter().cloned())
			.chain(
				self.local_scan
					.aliased_dirs
					.iter()
					.map(|(rel_path, target)| UnsyncablePath {
						rel_path: rel_path.clone(),
						reason: UnsyncableReason::LocalAlias {
							target: target.clone(),
						},
					}),
			)
			.collect();
		// One stable order, so a caller diffing consecutive reports sees only real changes.
		all.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
		all
	}

	/// Every path this pass must not plan an action for: a name the remote would reject, a local
	/// symlink to a directory inside the root, a path whose failure streak ran out, and a synced path
	/// whose remote item is out of the view. All are reported by [`unsyncable`](Self::unsyncable).
	fn blocked_paths(&self) -> BTreeSet<String> {
		self.failures
			.keys()
			.chain(self.local_scan.blocked_paths())
			.chain(self.unknown_remote.keys())
			.cloned()
			.collect()
	}
}

/// Re-key every path at or under `from` in `map` to the same place under `to`.
fn rekey_paths<M, V>(map: &mut M, from: &str, to: &str)
where
	M: Default + IntoIterator<Item = (String, V)> + FromIterator<(String, V)>,
{
	*map = mem::take(map)
		.into_iter()
		.map(|(path, value)| (plan::moved_path(&path, from, to).unwrap_or(path), value))
		.collect();
}

/// Whether a failure streak keeps its path out of a pass planned at `now` (unix millis): it has
/// reached [`MAX_PATH_FAILURES`], and its last failure is younger than
/// [`PATH_FAILURE_RETRY_INTERVAL`]. A last failure dated in the future (the clock went back) counts
/// as fresh, so a clock step never releases a path early.
fn streak_blocks(failure: &PathFailure, now: i64) -> bool {
	let interval = i64::try_from(PATH_FAILURE_RETRY_INTERVAL.as_millis()).unwrap_or(i64::MAX);
	failure.attempts >= MAX_PATH_FAILURES && now.saturating_sub(failure.last_failure_at) < interval
}

/// Which side wins when a caller resolves a held two-way conflict via
/// [`SyncEngine::resolve_conflict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
	/// The local copy wins: the next pass pushes it to the remote (or, if the conflict was a
	/// local deletion, propagates that deletion).
	KeepLocal,
	/// The remote copy wins: the next pass pulls it, with a local file whose content differed moved
	/// to the `.filen-sync-trash` bin first (or, if the conflict was a remote deletion, propagates
	/// that deletion — quarantining the local file).
	KeepRemote,
	/// Keep both: the local copy is renamed aside to `<stem>.old.<ext>` (`<stem>.old.N.<ext>` on
	/// collision) so it uploads as a new file, and the conflicting path itself then resolves as
	/// [`KeepRemote`](Self::KeepRemote).
	KeepBoth,
}

/// A `Synced` baseline row for `rel_path` with every side-specific field cleared — the base the
/// resolution branches fill in with the winning side's anchor.
fn synced_shell(rel_path: &str) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: super::baseline::NodeKind::File,
		remote_uuid: None,
		content_hash: None,
		size: None,
		local_mtime: None,
		remote_modified: None,
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: None,
		agreed_hash: None,
	}
}

/// Whether a held conflict row records the same content on both sides.
fn sides_converged(held: &BaselineEntry) -> bool {
	held.content_hash.is_some()
		&& held.content_hash == held.remote_hash
		&& held.size == held.remote_size
}

/// The baseline row that resolving a held conflict writes, or `None` when the winning side had
/// nothing at the path (the row is dropped instead, so the other side reads as a fresh create).
///
/// Decided purely so the policy is unit-testable. `winner` is already normalized:
/// [`KeepBoth`](ConflictResolution::KeepBoth) has moved the local copy aside and become
/// [`KeepRemote`](ConflictResolution::KeepRemote).
fn resolution_entry(
	rel_path: &str,
	held: &BaselineEntry,
	winner: ConflictResolution,
) -> Option<BaselineEntry> {
	match winner {
		// The local side wins: anchor the baseline to the REMOTE's recorded state, with no
		// local evidence, so the next scan reads the local copy (or its absence) as the change
		// and pushes it.
		//
		// Unless the two sides converged on identical content while the conflict was held: there
		// is nothing left to push, so record the whole converged state at once instead of leaving
		// a row a further pass has to adopt.
		ConflictResolution::KeepLocal => held.remote_kind.map(|kind| {
			let converged = kind == NodeKind::File && sides_converged(held);
			BaselineEntry {
				kind,
				remote_uuid: held.remote_uuid,
				remote_stable_uuid: held.remote_stable_uuid,
				remote_modified: held.remote_modified,
				content_hash: converged.then_some(held.content_hash).flatten(),
				size: converged.then_some(held.size).flatten(),
				local_mtime: converged.then_some(held.local_mtime).flatten(),
				// Resolving is an act of agreement: whatever the row now records as this side's
				// content is what both sides are taken to hold from here on, so the next foreign
				// edit reads as an ordinary one instead of re-conflicting forever.
				agreed_hash: converged.then_some(held.content_hash).flatten(),
				..synced_shell(rel_path)
			}
		}),
		// The remote side wins: anchor to the LOCAL's recorded state with no remote evidence,
		// so the remote (or its absence) reads as the change and is pulled.
		//
		// With the same convergence exception, mirrored: identical content on both sides leaves
		// nothing to pull, so the remote anchor is recorded too rather than left for a later
		// adopt pass.
		ConflictResolution::KeepRemote => held.local_kind.map(|kind| {
			let converged = kind == NodeKind::File && sides_converged(held);
			BaselineEntry {
				kind,
				content_hash: held.content_hash,
				size: held.size,
				local_mtime: held.local_mtime,
				remote_uuid: converged.then_some(held.remote_uuid).flatten(),
				remote_stable_uuid: converged.then_some(held.remote_stable_uuid).flatten(),
				remote_modified: converged.then_some(held.remote_modified).flatten(),
				// As above — and here the row's content is the local copy either way, so the marker
				// records it whether or not the two sides converged while the conflict was held.
				agreed_hash: held.content_hash,
				..synced_shell(rel_path)
			}
		}),
		ConflictResolution::KeepBoth => unreachable!("normalized to KeepRemote above"),
	}
}

impl SyncEngine {
	/// Open the engine, creating the baseline DB at `db_path` if needed. The `client`'s cache must
	/// be configured (it supplies the remote view).
	pub async fn open(client: Arc<Client>, db_path: PathBuf) -> Result<Self, Error> {
		let store = tokio::task::spawn_blocking(move || BaselineStore::open(&db_path))
			.await
			.map_err(|e| {
				Error::custom(ErrorKind::Internal, format!("baseline open panicked: {e}"))
			})??;
		// A pair paused by an earlier session stays paused: read the flags before the store moves
		// into the engine, so no pass can run against an empty map in the meantime. The pass that
		// session had in flight died with it, so every pair comes back on the DEFAULT pause options
		// — there is nothing left for a mode to apply to.
		let paused = store
			.paused_pairs()
			.map_err(|e| db_error(e, "loading paused sync pairs"))?
			.into_iter()
			.map(|pair| {
				(
					pair,
					tokio::sync::watch::channel(PassControl::pausing(PauseOptions::default())).0,
				)
			})
			.collect();
		let engine = Self {
			client,
			store: Mutex::new(store),
			pending: PendingWrites::default(),
			observed: Arc::new(Observations::default()),
			roots: Mutex::new(HashMap::new()),
			approvals: Mutex::new(HashMap::new()),
			registrations: Mutex::new(()),
			paused: Mutex::new(paused),
			removals: Mutex::new(HashMap::new()),
			lock_budget: LockBudget::default(),
			reading: Mutex::new(HashMap::new()),
		};
		// Pairs registered by an earlier session are live again from here on, so they need their
		// cache subscription back too.
		for record in engine.list_pairs().await? {
			engine.observe_pair(record.id, record.remote_root).await;
		}
		// An engine that went down inside the cache-lag window left the writes it had made in the
		// journal; folding them is what stops this one from making them a second time.
		let now = Utc::now().timestamp_millis();
		let rows = engine
			.store
			.lock()
			.await
			.load_pending(now, grace_millis())
			.map_err(|e| db_error(e, "loading the pending-write journal"))?;
		if !rows.is_empty() {
			tracing::debug!(
				"sync engine: restoring {} unacknowledged write(s)",
				rows.len()
			);
		}
		restore_journal(&engine.pending, rows, now);
		Ok(engine)
	}

	/// Subscribe to the cache's notifications for a pair's remote root, so a write this engine
	/// made retires as soon as the cache commits any event for it — including the trash and
	/// supersede events a deletion or a re-upload produces, which never make the written uuid
	/// appear in a snapshot at all.
	///
	/// Best-effort: a root the cache refuses (deleted, unreachable) leaves that pair on the
	/// [`PENDING_CREATE_GRACE`] fallback rather than failing the whole engine. The watch loop
	/// registers its own subscription for wakeups; the two are independent.
	async fn observe_pair(&self, pair: PairId, remote_root: Uuid) {
		let callback = observation_callback(Arc::clone(&self.observed));
		match Arc::clone(&self.client)
			.add_sync_root(remote_root, callback)
			.await
		{
			Ok(handle) => {
				self.roots.lock().await.insert(pair, handle);
			}
			Err(error) => tracing::warn!(
				"sync pair {pair}: cache notifications are unavailable ({error}); its pending writes will retire on the grace window alone"
			),
		}
	}

	/// Refuse a pair whose roots overlap one already registered (see [`PairOverlap`]). The pair
	/// being re-registered unchanged is exempt: identical roots are the idempotent case, not an
	/// overlap.
	async fn check_roots_free(
		&self,
		local_root: &Path,
		local: &str,
		remote_root: Uuid,
	) -> Result<(), Error> {
		let existing = self.list_pairs().await?;
		let same_pair =
			|record: &PairRecord| record.local_root == local && record.remote_root == remote_root;
		let others: Vec<&PairRecord> = existing.iter().filter(|r| !same_pair(r)).collect();

		for record in &others {
			if let Some(overlap) =
				local_overlap(local_root, Path::new(&record.local_root), record.id)
			{
				return Err(overlap_error(overlap));
			}
			if record.remote_root == remote_root {
				return Err(overlap_error(PairOverlap::RemoteRootInUse {
					pair: record.id,
					existing: record.remote_root,
				}));
			}
		}

		// With no other pair there is nothing for this one to overlap, so the ancestry below is not
		// asked for at all: the first pair a caller registers must not need the network to be
		// checked against a registry that is empty.
		if others.is_empty() {
			return Ok(());
		}

		// Nested remote roots need each root's ancestry (see `PairOverlap`). The cache answers for
		// free and a chain it already carries is a nesting it has actually observed, so it runs first
		// as a short-circuit — but never as an acceptance: it is only as fresh as the last event it
		// applied, and a folder moved into a pair's root seconds ago still reads there as unrelated.
		if let Some(overlap) = self.cached_overlap(remote_root, &others).await {
			return Err(overlap_error(overlap));
		}

		// The exact answer: both chains read from the SERVER to the account root, so a nesting is
		// refused whether or not the cache has caught up with either folder — and a chain the server
		// will not supply refuses the registration rather than let a pair through unchecked.
		let account_root = self.client.root().uuid();
		let candidate_chain = self.server_ancestry(remote_root, account_root).await?;
		for record in &others {
			let existing_chain = self
				.server_ancestry(record.remote_root, account_root)
				.await?;
			if let Some(overlap) = remote_overlap(
				remote_root,
				&candidate_chain,
				record.remote_root,
				&existing_chain,
				record.id,
			) {
				return Err(overlap_error(overlap));
			}
		}
		Ok(())
	}

	/// The overlap the CACHE can already see between `candidate` and one of `others`, or `None`.
	///
	/// Free — one indexed SQLite read per root — and enough to REFUSE on: a chain carrying the other
	/// root is a nesting the cache has observed. It is not enough to ACCEPT on, so `None` here means
	/// "ask the server", never "disjoint". The cost of the asymmetry is a stale refusal: a folder the
	/// cache still believes sits inside a pair's root is refused until the cache catches up, which is
	/// the safe direction and self-clearing.
	async fn cached_overlap(&self, candidate: Uuid, others: &[&PairRecord]) -> Option<PairOverlap> {
		let candidate_chain = self.cached_ancestry(candidate).await;
		for record in others {
			let existing_chain = self.cached_ancestry(record.remote_root).await;
			if let Some(overlap) = remote_overlap(
				candidate,
				&candidate_chain,
				record.remote_root,
				&existing_chain,
				record.id,
			) {
				return Some(overlap);
			}
		}
		None
	}

	/// `uuid`'s cached upward chain, EMPTY when the cache cannot answer — which
	/// [`Client::cached_ancestors`] documents as "unknown", never as "no ancestors".
	async fn cached_ancestry(&self, uuid: Uuid) -> Vec<Uuid> {
		self.client
			.cached_ancestors(uuid)
			.await
			.unwrap_or_else(|error| {
				tracing::debug!(
					"add_pair: cannot read the cached ancestry of {uuid} ({error}); asking the server instead"
				);
				Vec::new()
			})
	}

	/// `uuid`'s upward chain read from the server, one [`Client::get_dir`] per level.
	///
	/// Bounded three ways: at the account root, at [`MAX_REMOTE_ANCESTRY_DEPTH`] levels, and at a
	/// parent already on the chain (a cycle no healthy server serves). A parent that is not a folder
	/// at all — the trash, or one of the virtual parents — ends the walk too: there is no folder
	/// chain above it to compare against. That last stop is what keeps a registered pair whose
	/// remote root someone else deleted from wedging every later registration: the server still
	/// answers for such a folder, with `Trash` as its parent, so the walk yields the root alone and
	/// the pair simply relates to nothing (CONTROL-09c).
	async fn server_ancestry(&self, uuid: Uuid, account_root: Uuid) -> Result<Vec<Uuid>, Error> {
		let mut chain = vec![uuid];
		let mut current = uuid;
		while current != account_root {
			if chain.len() >= MAX_REMOTE_ANCESTRY_DEPTH {
				tracing::warn!(
					"add_pair: the ancestry of {uuid} is more than {MAX_REMOTE_ANCESTRY_DEPTH} levels deep; a nesting above that is not detected"
				);
				break;
			}
			let dir = self.client.get_dir(current).await?;
			let ParentUuid::Uuid(parent) = *dir.parent() else {
				break;
			};
			if chain.contains(&parent) {
				tracing::warn!("add_pair: the ancestry of {uuid} loops at {parent}");
				break;
			}
			chain.push(parent);
			current = parent;
		}
		Ok(chain)
	}

	/// Register a sync pair, returning its id. `remote_root` must be a sync-rooted SUBFOLDER the
	/// cache covers (not the account root).
	///
	/// Idempotent for the same `(local_root, remote_root, mode)`: the existing pair's id comes back
	/// and nothing is disturbed. The same roots with a DIFFERENT mode is an ERROR, not a silent
	/// switch — an established pair's direction decides whether the next pass overwrites the local
	/// copy or the remote one, which is far too consequential to change as a side effect of a
	/// registration call. Use [`reconfigure_pair`](Self::reconfigure_pair) to change it.
	///
	/// `local_root` must already EXIST and be a directory; it is canonicalized before being stored,
	/// so the pair's identity (and the write-confinement anchor) is symlink-stable. A missing or
	/// non-directory root is rejected here rather than surfacing a pass later as an
	/// incomplete-scan guard hold with every deletion withheld.
	pub async fn add_pair(
		&self,
		local_root: PathBuf,
		remote_root: Uuid,
		mode: SyncMode,
	) -> Result<PairId, Error> {
		let local_root = std::fs::canonicalize(&local_root).map_err(|error| {
			Error::custom_with_source(
				ErrorKind::IO,
				error,
				Some(format!(
					"sync pair local root {local_root:?} cannot be resolved"
				)),
			)
		})?;
		if !local_root.is_dir() {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!("sync pair local root {local_root:?} is not a directory"),
			));
		}
		let local = local_root.to_string_lossy().into_owned();
		// Held across the check AND the insert: concurrent registrations must not both read a
		// registry neither has written to yet.
		let _registering = self.registrations.lock().await;
		self.check_roots_free(&local_root, &local, remote_root)
			.await?;
		let (pair, stored_mode) = self
			.store
			.lock()
			.await
			.create_pair(&local, remote_root, mode)
			.map_err(|e| db_error(e, "registering a sync pair"))?;
		if stored_mode != mode {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!(
					"sync pair {pair} is already registered for these roots in {stored_mode:?} \
					 mode; call reconfigure_pair to change it to {mode:?}"
				),
			));
		}
		self.observe_pair(pair, remote_root).await;
		Ok(pair)
	}

	/// Change `pair`'s [`SyncMode`]. The new mode applies from the NEXT pass; nothing is re-run
	/// under it and no baseline row is dropped, so an established pair does not re-transfer its
	/// contents and a held conflict stays held for
	/// [`resolve_conflict`](Self::resolve_conflict).
	///
	/// "Prospective" is about the PASSES, not about the divergence between the two sides. The next
	/// pass reconciles whatever state the sides are in right now under the new rules, so a
	/// divergence the old mode deliberately left standing is acted on as soon as the new mode says
	/// to act on it. Concretely:
	///
	/// - to `LocalToRemote`: the remote stops being an independent side — a remote-only edit is
	///   overwritten by the local copy rather than pulled — and, going the other way, a
	///   `RemoteToLocal` switch has the local copy overwritten instead.
	/// - a backup mode (`LocalBackup` / `RemoteBackup`) to a deletion-propagating one: every source
	///   deletion the backup mode had left standing on the destination is a pending deletion under
	///   the new mode. This is what `backlog` decides — see below.
	/// - to a backup mode: deletions simply stop propagating from the next pass on; nothing already
	///   deleted comes back.
	///
	/// # The standing backlog
	///
	/// [`Backlog::Propagate`] takes the sides as they are: the whole set of source deletions the
	/// backup mode left standing is pending at once, and the next pass propagates it. The
	/// mass-delete guard still screens it (see [`set_delete_guard`](Self::set_delete_guard)), so a
	/// large backlog is held for approval rather than applied unasked — but a small one is not.
	///
	/// [`Backlog::AdoptDestination`] reads both sides FIRST (one snapshot + one local scan, the same
	/// pair a pass reads) and re-seeds the baseline from the destination for every path the source no
	/// longer has, in the same transaction as the mode change. Those copies then count as intended: a
	/// one-way mirror neither deletes them nor pushes them back to the source, and `TwoWay` reads
	/// them as newly created on the side that still has them and flows them back. In the ONE-WAY
	/// modes that covers a destination item the pair never TRACKED as well — a file another client
	/// created straight on the destination, which the mirror would otherwise trash on the very next
	/// pass. `TwoWay` re-seeds only the paths it tracked: an untracked copy already flows to the
	/// other side under the ordinary rules, and a row there would only take the path out of move
	/// detection.
	///
	/// Either way, [`plan_pair`](Self::plan_pair) after the switch shows exactly what the first pass
	/// will do.
	///
	/// Errors if the pair is unknown, and — for [`Backlog::AdoptDestination`] only — if that read
	/// cannot be trusted to say what either side no longer has: a name collision, an incomplete local
	/// scan, or a remote view that has never converged or came back wholly empty. The mode is left
	/// exactly as it was, so the call is safe to retry. Re-registering an existing pair through
	/// [`add_pair`](Self::add_pair) with a different mode is an error rather than a silent switch,
	/// so a mode change is always this explicit call.
	pub async fn reconfigure_pair(
		&self,
		pair: PairId,
		mode: SyncMode,
		backlog: Backlog,
	) -> Result<(), Error> {
		// Read both sides before touching the mode: the adoption is anchored to what the
		// destination holds NOW, and the switch must not take effect without it.
		let adopted = match backlog {
			Backlog::Propagate => Vec::new(),
			Backlog::AdoptDestination => {
				let prep = self.prepare(pair, ScanDepth::Fast).await?;
				// The adoption rewrites baseline rows from this one read, and every row it writes is
				// for a path it reads as GONE on the source side — so it needs the same evidence a
				// pass needs before acting on an absence. Read through an incomplete scan or an
				// unconverged/transiently-empty remote view it does the opposite of what was asked:
				// it adopts nothing and the switch then propagates the very backlog the caller wanted
				// kept, or it adopts a whole pair from whichever side still lists something and drops
				// the other side's anchors. Refuse instead — the mode is left as it was, and the
				// caller can retry once the view is healthy.
				if let Some(reason) = adoption_refusal(refusal(&prep), &screen_state(&prep)) {
					return Err(Error::custom(
						ErrorKind::InvalidState,
						format!("cannot adopt the destination: {reason}"),
					));
				}
				plan::adopt_destination_rows(
					mode,
					&prep.baseline,
					&prep.local_scan.nodes,
					&prep.remote_view.nodes,
				)
			}
		};
		let changed = self
			.store
			.lock()
			.await
			.set_mode(pair, mode, &adopted)
			.map_err(|e| db_error(e, "reconfiguring a sync pair"))?;
		if changed == 0 {
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		tracing::debug!(
			"sync pair {pair}: mode changed to {mode:?} from the next pass ({backlog:?}, {} destination item(s) adopted)",
			adopted.len()
		);
		Ok(())
	}

	/// Resolve a two-way conflict the engine is holding at `rel_path` (one reported in
	/// [`SyncReport::conflicts`]) by naming the winning side. The resolution takes effect on the
	/// NEXT pass: the baseline is re-anchored so the winner reads as the changed side and is
	/// propagated normally, and the path (with its subtree) stops being held.
	///
	/// [`KeepBoth`](ConflictResolution::KeepBoth) additionally renames the local copy aside to
	/// `<stem>.old.<ext>` first, so that copy uploads as a new file instead of being overwritten.
	///
	/// [`KeepRemote`](ConflictResolution::KeepRemote) moves a local file whose content differs from
	/// the remote's into the pair's `.filen-sync-trash` bin first: that edit was never uploaded, so
	/// the download would otherwise destroy the only copy.
	///
	/// A resolution never interleaves with a pass: a pass still reading the pair, or recording the
	/// conflicts it read, is waited for, since the rows it writes would overwrite this one. A
	/// [`paused`](Self::pause_pair) pair stays resolvable — a pause parks a pass between actions,
	/// never inside that read, so the wait is at most the read in flight. That read includes the
	/// local scan, so behind a watch's deep-scan pass (see
	/// [`WatchConfig::deep_scan_every`](super::WatchConfig::deep_scan_every)), which re-hashes every
	/// local file, the wait can run to minutes on a large tree. A pair whose
	/// [`removal`](Self::remove_pair) is under way is refused, as the other control verbs are, and
	/// that removal waits for a resolution already running.
	///
	/// Errors if the pair is unknown, is being removed, or no conflict is currently held at
	/// `rel_path`.
	pub async fn resolve_conflict(
		&self,
		pair: PairId,
		rel_path: &str,
		resolution: ConflictResolution,
	) -> Result<(), Error> {
		// The gate (refusing an unknown pair) keeps a removal waiting for this call; the lock keeps
		// it out of a pass's read. The retirement is read AFTER the lock, since a removal may start
		// while this waits for it.
		let gate = self.pass_gate(pair).await?;
		let _reading = self.reading_lock(pair).await.lock_owned().await;
		if gate.retired() {
			return Err(being_removed(pair));
		}
		let (record, mut held) = {
			let store = self.store.lock().await;
			let record = store
				.pair(pair)
				.map_err(|e| db_error(e, "loading the sync pair"))?
				.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?;
			let held = store
				.entry(pair, rel_path)
				.map_err(|e| db_error(e, "loading the held conflict"))?
				.filter(|entry| entry.state.is_conflict())
				.ok_or_else(|| {
					Error::custom(
						ErrorKind::InvalidState,
						format!("no conflict is being held at {rel_path:?} for this pair"),
					)
				})?;
			(record, held)
		};

		// A divergence this engine's own push created is resolved against the SERVER's version
		// history rather than against what the remote holds now — our upload is what it holds.
		if held.state == BaselineState::Overwritten {
			return self
				.resolve_overwritten(pair, &record, rel_path, &held, resolution)
				.await;
		}

		let store = self.store.lock().await;
		let mut winner = resolution;
		if winner == ConflictResolution::KeepBoth {
			// Move the losing local copy out of the way FIRST, then resolve the path itself to the
			// remote: the moved-aside copy has no baseline row, so it uploads as a new file.
			if held.local_kind.is_some() {
				let moved = apply::rename_aside(Path::new(&record.local_root), rel_path)?;
				tracing::debug!(
					"resolve_conflict[pair {pair}]: kept the local copy of {rel_path:?} aside as {moved:?}"
				);
				held.local_kind = None;
			}
			// Either way the local side is now empty at this path, so the remote copy lands there.
			winner = ConflictResolution::KeepRemote;
		} else if winner == ConflictResolution::KeepRemote
			&& held.local_kind == Some(NodeKind::File)
			&& held.remote_kind.is_some()
			&& !sides_converged(&held)
		{
			// The losing local edit goes to the bin, as the `Overwritten` shape's does. Anchoring the
			// row to it instead would let the next pass vouch for it and download straight over it.
			// With the local side emptied the path resolves the way `KeepBoth` leaves it.
			let bin = apply::quarantine_local(Path::new(&record.local_root), rel_path)?;
			tracing::debug!(
				"resolve_conflict[pair {pair}]: quarantined the local copy of {rel_path:?} as {bin:?}"
			);
			held.local_kind = None;
		}

		match resolution_entry(rel_path, &held, winner) {
			Some(entry) => store.upsert_entry(pair, &entry),
			// The winner's side had nothing at this path: drop the row entirely, so the other
			// side reads as a fresh create (or as already-gone) rather than as a second conflict.
			None => store.delete_entry(pair, rel_path),
		}
		.map_err(|e| db_error(e, "resolving a conflict"))
	}

	/// Resolve a divergence this engine's own upload created — it buried another client's edit (see
	/// [`BaselineState::Overwritten`]).
	///
	/// The shape is the mirror of an ordinary conflict: the winning side is already the remote head
	/// and the losing side is a version in the file's history, so every resolution acts on the
	/// history rather than on the current head.
	///
	/// - [`KeepLocal`](ConflictResolution::KeepLocal): nothing to do on the remote — our bytes are
	///   the head and theirs stay in the version history. The row goes, and the next pass records
	///   the two sides as converged (they hold the same content) with no transfer.
	/// - [`KeepRemote`](ConflictResolution::KeepRemote): restore the buried version, so it is the
	///   head again, and quarantine our own copy. The next pass downloads what we restored; our
	///   bytes are recoverable from the bin and from the history.
	/// - [`KeepBoth`](ConflictResolution::KeepBoth): fetch the buried version beside ours as
	///   `<stem>.old.<ext>`. It has no baseline row, so the next pass uploads it — and both
	///   clients end up holding both files instead of one of them silently losing an edit.
	async fn resolve_overwritten(
		&self,
		pair: PairId,
		record: &PairRecord,
		rel_path: &str,
		held: &BaselineEntry,
		resolution: ConflictResolution,
	) -> Result<(), Error> {
		let buried_uuid = held.remote_uuid.ok_or_else(|| {
			Error::custom(
				ErrorKind::InvalidState,
				format!("the conflict held at {rel_path:?} names no buried version"),
			)
		})?;
		let local_root = Path::new(&record.local_root);
		match resolution {
			ConflictResolution::KeepLocal => {}
			ConflictResolution::KeepBoth => {
				let buried = self.client.get_file(buried_uuid).await?;
				let (aside, path) = apply::aside_target(local_root, rel_path)?;
				self.client
					.download_file_to_path(&buried, &path, None)
					.await?;
				tracing::debug!(
					"resolve_conflict[pair {pair}]: kept the buried version of {rel_path:?} as {aside:?}"
				);
			}
			ConflictResolution::KeepRemote => {
				// The head by LINEAGE, never the version chain's first entry: the chain is sorted by
				// each version's original upload time, to the second, so the two uploads of the race
				// that made this row tie and their order in the listing is arbitrary — half the time
				// its first entry would be the buried version itself, and the restore below would be
				// skipped as a no-op while the local copy went to the bin.
				let lineage = held.remote_stable_uuid.ok_or_else(|| {
					Error::custom(
						ErrorKind::InvalidState,
						format!("the conflict held at {rel_path:?} names no file to restore into"),
					)
				})?;
				let mut head = self.client.get_file_by_stable_uuid(lineage).await?;
				if head.uuid() != buried_uuid {
					let version = self
						.client
						.list_file_versions(&head)
						.await?
						.into_iter()
						.find(|version| version.uuid() == buried_uuid)
						.ok_or_else(|| {
							Error::custom(
								ErrorKind::InvalidState,
								format!(
									"the version buried at {rel_path:?} is no longer restorable"
								),
							)
						})?;
					self.client.restore_file_version(&mut head, version).await?;
				}
				// Our own copy goes to the bin rather than under the download: the restored version
				// is what this resolution asked for, and the bytes it replaces are not lost.
				apply::quarantine_local(local_root, rel_path)?;
				tracing::debug!(
					"resolve_conflict[pair {pair}]: restored the version buried at {rel_path:?} and quarantined the local copy"
				);
			}
		}
		// Either way the row has said all it has to say: dropping it lets the next pass reconcile
		// the path from what the two sides now actually hold.
		self.store
			.lock()
			.await
			.delete_entry(pair, rel_path)
			.map_err(|e| db_error(e, "resolving a conflict"))
	}

	/// Retire the confirmation gap this engine's own pushes leave, from the evidence a snapshot
	/// listing our version at its path ([`plan::confirm_agreed_content`]) cannot supply.
	///
	/// Two sources, cheapest first:
	/// - the cache's announcements. A version of ours that stood as the remote head for
	///   [`CONFIRM_TENURE`] before anything superseded it is content both sides held, whether or
	///   not a pass ever happened to run while it was the head. This is what confirms the backlog a
	///   PAUSED pair accumulated: no pass runs, so nothing else ever could.
	/// - the server's version chain, for a row the announcements say nothing about (this engine was
	///   restarted, or the announcement never arrived) whose path now carries a foreign version of
	///   the same file. The two versions' timestamps say how long ours stood. One lookup per such
	///   row, and only for a row that is actually about to be reconciled against a foreign version;
	///   a lookup that fails leaves the row unconfirmed, exactly as before.
	///
	/// Returns the rows that moved, for the caller to persist — and to retire their push records
	/// with ([`Observations::forget_pushes`]) once it has. Reading a verdict does not consume it:
	/// `prepare` runs for [`plan_pair`](Self::plan_pair) too, which writes nothing.
	///
	/// `raw_remote` must be the RAW snapshot: a view with this engine's own writes folded in shows
	/// our version at its own path and would confirm every push against itself.
	async fn confirm_pushes(
		&self,
		baseline: &mut HashMap<String, BaselineEntry>,
		raw_remote: &HashMap<String, RemoteNode>,
		files: &[CacheableFile<'static>],
	) -> Vec<BaselineEntry> {
		let (mut confirmed, ask_server) =
			observed_confirmations(&self.observed, baseline, raw_remote, Instant::now());

		for (rel_path, ours, foreign) in ask_server {
			let Some(cacheable) = files.iter().find(|f| f.uuid == foreign) else {
				continue;
			};
			let file = crate::io::RemoteFile::from(cacheable.clone());
			let versions = match self.client.list_file_versions(&file).await {
				Ok(versions) => versions,
				Err(error) => {
					// Offline, or the endpoint refused: the row stays unconfirmed, which is the
					// safe direction — the pass surfaces a conflict rather than burying our bytes.
					tracing::debug!(
						"sync_once: could not read the version chain of {rel_path:?} to date this engine's push — {error}"
					);
					continue;
				}
			};
			let chain: Vec<(Uuid, chrono::DateTime<Utc>)> = versions
				.iter()
				.map(|version| (version.uuid(), version.timestamp()))
				.collect();
			match plan::version_chain_verdict(&chain, ours, foreign, CONFIRM_TENURE) {
				Some(true) => {
					tracing::debug!(
						"sync_once: the server's version chain confirms this engine's push of {rel_path:?}"
					);
					confirmed.insert(ours);
				}
				Some(false) => tracing::debug!(
					"sync_once: {rel_path:?} — a foreign version landed on this engine's push inside the confirmation window"
				),
				None => tracing::debug!(
					"sync_once: {rel_path:?} — the version chain does not carry this engine's push, so it stays unconfirmed"
				),
			}
		}

		plan::confirm_agreed_pushes(baseline, &confirmed)
	}

	/// Ask the server which version is current at the path of a pending create whose snapshot shows
	/// a same-lineage STRANGER, and retire the record when ours is not.
	///
	/// [`fold_create`] paints our own version over such a stranger, and it is usually right to: the
	/// ordinary reason for one is a cache still listing the version our upload superseded, which
	/// the winner of a same-name race sees at its own path and must not conflict against. A resync
	/// that skipped straight past our version looks identical from here and is not — the stranger
	/// is then genuinely newer, and folding hides it until the 180 s grace ceiling retires the
	/// record. One lookup separates the two, and only in that state: no pending create, or a
	/// snapshot showing what we wrote or what we replaced, costs nothing at all.
	///
	/// Retiring the record is all this does. What the standing stranger MEANS — a pull, or a
	/// conflict against a push of ours that nothing confirmed — is the reconcile's to decide, from
	/// the same rules as any other foreign version.
	async fn retire_superseded_creates(
		&self,
		pair: PairId,
		baseline: &HashMap<String, BaselineEntry>,
		raw_remote: &HashMap<String, RemoteNode>,
		files: &[CacheableFile<'static>],
	) -> Result<(), Error> {
		let candidates = self.pending.strangers(pair, baseline, raw_remote);
		let mut retired = Vec::new();
		for (record, path, ours, stranger) in candidates {
			let Some(cacheable) = files.iter().find(|f| f.uuid == stranger) else {
				continue;
			};
			let file = crate::io::RemoteFile::from(cacheable.clone());
			// By LINEAGE, not by the version chain's order: the chain is sorted by original upload
			// time to the second, so a race leaves the two versions tied and its first entry is not
			// reliably the head. This asks the server outright, and in one call.
			let head = match self
				.client
				.get_file_by_stable_uuid(file.stable_uuid())
				.await
			{
				Ok(head) => Some(head.uuid()),
				Err(error) => {
					// No answer, so no reason to stop trusting our own write yet: the fold carries
					// on until the cache announces something or the grace ceiling runs out.
					tracing::debug!(
						"sync_once[pair {pair}]: could not check which version is current at {path:?} — {error}"
					);
					continue;
				}
			};
			if head.is_some_and(|head| head != ours) {
				tracing::debug!(
					"sync_once[pair {pair}]: the server says this engine's write at {path:?} was superseded — reconciling against what the snapshot shows"
				);
				retired.push(record);
			}
		}
		if !retired.is_empty() {
			// The store lock FIRST, and the in-memory retirement under it: a cancel dropping this
			// read between the two halves — the next candidate's lookup is a whole network call wide
			// — would leave the DB holding a record memory has already retired, and the next open
			// would fold this engine's write back over the version the server just said replaced it.
			let store = self.store.lock().await;
			for record in &retired {
				self.pending.retire(*record);
			}
			store
				.delete_pending(&retired)
				.map_err(|e| db_error(e, "retiring a superseded write"))?;
		}
		Ok(())
	}

	/// Advance what the cache's announcements have confirmed since the last pass, and persist it.
	///
	/// The announcement half of [`confirm_pushes`](Self::confirm_pushes) alone: with no snapshot
	/// there is no foreign version to date a push against, so nothing here talks to the server.
	/// A pass does this itself; this is for the stretches where none runs.
	async fn sweep_confirmations(&self, pair: PairId) -> Result<(), Error> {
		let mut baseline: HashMap<String, BaselineEntry> = {
			let store = self.store.lock().await;
			store
				.entries(pair)
				.map_err(|e| db_error(e, "loading the baseline"))?
				.into_iter()
				.map(|entry| (entry.rel_path.clone(), entry))
				.collect()
		};
		let advanced = self
			.confirm_pushes(&mut baseline, &HashMap::new(), &[])
			.await;
		if advanced.is_empty() {
			return Ok(());
		}
		{
			let store = self.store.lock().await;
			for entry in &advanced {
				store
					.upsert_entry(pair, entry)
					.map_err(|e| db_error(e, "recording the confirmed agreed content"))?;
			}
		}
		self.forget_settled_pushes(&advanced);
		Ok(())
	}

	/// Retire the push records of rows this caller has just PERSISTED as agreed — the ones a
	/// snapshot listed at their own path as well as the ones a tenure dated — plus any record no
	/// pass will ever read again (see [`PUSH_RECORD_TTL`]).
	///
	/// Only a caller that wrote the rows may do this. The record is the only evidence a push was
	/// ever confirmed, and dropping it without recording what it proved leaves the next pass
	/// reading the row as an unconfirmed push again — with a foreign edit on top, a false conflict.
	fn forget_settled_pushes(&self, persisted: &[BaselineEntry]) {
		let decided: Vec<Uuid> = persisted
			.iter()
			.filter_map(|entry| entry.remote_uuid)
			.collect();
		self.observed.forget_pushes(&decided, Instant::now());
	}

	/// Run the read-only half: load the baseline, scan (at `depth`), enumerate the remote, build the
	/// view.
	async fn prepare(&self, pair: PairId, depth: ScanDepth) -> Result<Prepared, Error> {
		let (record, baseline_entries, failures) = {
			let store = self.store.lock().await;
			let record = store
				.pair(pair)
				.map_err(|e| db_error(e, "loading the sync pair"))?
				.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?;
			let entries = store
				.entries(pair)
				.map_err(|e| db_error(e, "loading the baseline"))?;
			let now = Utc::now().timestamp_millis();
			let mut failures = store
				.failures(pair)
				.map_err(|e| db_error(e, "loading the per-path failure counts"))?;
			failures.retain(|_, failure| streak_blocks(failure, now));
			(record, entries, failures)
		};

		let mut baseline_map: HashMap<String, BaselineEntry> = baseline_entries
			.into_iter()
			.map(|entry| (entry.rel_path.clone(), entry))
			.collect();

		// Copied BEFORE the snapshot is read: an event the cache commits afterwards describes a
		// state this snapshot predates, so it must not retire a pending write this pass.
		let observed = self.observed.snapshot();
		let snapshot = self
			.client
			.enumerate_sync_root_snapshot(record.remote_root)
			.await?;
		let mut remote_view = plan::build_remote_view(
			record.remote_root,
			&snapshot.dirs,
			&snapshot.files,
			&snapshot.undecodable,
		);

		// Read the snapshot BEFORE the local scan, so the confirmation below runs against the RAW
		// view — before this engine's own writes are folded into it, and before the baseline is
		// shared (immutably) with the scan. A row the snapshot confirms is one both sides
		// demonstrably hold, which is what a later foreign edit is measured against.
		let mut confirmed = plan::confirm_agreed_content(&mut baseline_map, &remote_view.nodes);
		confirmed.extend(
			self.confirm_pushes(&mut baseline_map, &remote_view.nodes, &snapshot.files)
				.await,
		);
		let baseline: Arc<HashMap<String, BaselineEntry>> = Arc::new(baseline_map);
		let (unknown_remote, never_synced_remote) =
			plan::unknown_remote_paths(&baseline, &remote_view.skipped);

		let local_root = PathBuf::from(&record.local_root);
		let scan_baseline = Arc::clone(&baseline);
		let local_scan = tokio::task::spawn_blocking(move || {
			scan::scan_local(&local_root, &scan_baseline, depth)
		})
		.await
		.map_err(|e| Error::custom(ErrorKind::Internal, format!("local scan panicked: {e}")))?;

		let remote_emptied =
			remote_view.nodes.is_empty() && baseline.values().any(|e| e.remote_uuid.is_some());
		// The rows `settle` retires have to leave the DB too, or a restart would fold writes the
		// cache has demonstrably caught up to. Diffed around the call so `settle` itself stays a
		// pure in-memory operation — under a lock taken BEFORE it, because waiting for that lock is
		// the one await between the two halves and this read runs under the pass's cancel: dropped
		// there, the retirement would stand in memory and not in the DB.
		let mut holds = {
			let store = self.store.lock().await;
			let before = self.pending.uuids();
			let holds = self.pending.settle(pair, &observed, &remote_view.nodes);
			let retired: Vec<Uuid> = before.difference(&self.pending.uuids()).copied().collect();
			if !retired.is_empty() {
				store
					.delete_pending(&retired)
					.map_err(|e| db_error(e, "retiring pending writes"))?;
			}
			holds
		};
		// A create whose path shows another version of the same file is the one thing the fold
		// cannot settle on its own; ask the server before it paints over a stranger.
		self.retire_superseded_creates(pair, &baseline, &remote_view.nodes, &snapshot.files)
			.await?;
		// Correct the view with what this engine wrote and the cache has not shown yet, BEFORE
		// anything reconciles or detects moves against it.
		let folded = self
			.pending
			.fold_into(pair, &baseline, &mut remote_view.nodes);
		if folded > 0 {
			tracing::debug!(
				"sync_once[pair {pair}]: folding {folded} unacknowledged write(s) into the remote view"
			);
		}
		holds.held_remote = remote_view.held_paths.clone();
		self.observed.prune_before(self.pending.oldest_stamp());

		let mut prepared = Prepared {
			record,
			baseline,
			dir_moves: Vec::new(),
			local_scan,
			remote_view,
			remote_converged: snapshot.watermark.is_some(),
			remote_emptied,
			holds,
			failures,
			unknown_remote,
			never_synced_remote,
			confirmed,
			dirs: snapshot.dirs,
			files: snapshot.files,
		};
		// AFTER the pending-write fold: a move of ours the cache has not shown yet must read as done,
		// not as the remote moving the directory back.
		prepared.fold_dir_moves();
		Ok(prepared)
	}

	/// Reconcile + guard-screen a pass WITHOUT applying it: a dry run that reads both sides and
	/// reports what a [`sync_once`](Self::sync_once) would do, mutating neither tree nor the
	/// baseline. A pending deletion approval is neither consumed nor honoured here.
	///
	/// It takes no pass gate: it has no plan and no action for a pause to interrupt, and a dry run
	/// must not be something [`remove_pair`](Self::remove_pair) waits for. That is not the same as
	/// touching nothing — it runs the same [`prepare`](Self::prepare), so it makes that read's
	/// server calls and retires the journal records that read settles. What it does NOT do is
	/// persist the confirmations it observed. A [`paused`](Self::pause_pair) pair is planned like
	/// any other.
	pub async fn plan_pair(&self, pair: PairId) -> Result<PlanOutcome, Error> {
		let prep = self.prepare(pair, ScanDepth::Fast).await?;
		if let Some(reason) = refusal(&prep) {
			return Ok(PlanOutcome {
				refused: Some(reason),
				..PlanOutcome::default()
			});
		}
		let screened = reconcile_and_screen(&prep, screen_state(&prep));
		Ok(PlanOutcome {
			actions: prep.planned(&screened.decision.safe),
			held: prep.planned(&screened.decision.held),
			held_reason: screened.decision.reason,
			pass_token: screened.pass_token,
			conflicts: screened.conflicts,
			unsyncable: prep.unsyncable(),
			refused: None,
		})
	}

	/// Set `pair`'s mass-delete threshold, from the next pass on. Persisted with the pair, so it
	/// survives a restart; read it back on the pair's [`PairRecord`].
	///
	/// The threshold is the only part of the guard that is configurable. The other holds — an
	/// incomplete local scan, a first sync against a populated destination, a remote view that has
	/// never converged or came back wholly empty — are about whether this pass's evidence can be
	/// TRUSTED at all, and stay in force under every setting including
	/// [`DeleteGuard::unlimited`].
	pub async fn set_delete_guard(&self, pair: PairId, guard: DeleteGuard) -> Result<(), Error> {
		let changed = self
			.store
			.lock()
			.await
			.set_delete_guard(pair, guard)
			.map_err(|e| db_error(e, "setting the delete guard"))?;
		if changed == 0 {
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		Ok(())
	}

	/// Every sync pair this engine has registered, in registration order.
	pub async fn list_pairs(&self) -> Result<Vec<PairRecord>, Error> {
		self.store
			.lock()
			.await
			.list_pairs()
			.map_err(|e| db_error(e, "listing sync pairs"))
	}

	/// Forget `pair` and every baseline row under it.
	///
	/// This is a REGISTRY operation only: NO file is touched on either side. The local tree and the
	/// remote folder are left exactly as they are — a removed pair simply stops syncing. Adding the
	/// same roots again later starts from an empty baseline, i.e. with first-sync semantics.
	/// Removing an unknown pair is a no-op.
	///
	/// The pass in flight is CANCELLED rather than allowed to run on: the transfer running is
	/// dropped, the actions queued behind it are skipped, and a pass parked on a
	/// [`pause`](Self::pause_pair) is cut loose rather than left waiting for a resume that can no
	/// longer be asked for. Nothing an interrupted action did is recorded half-way (see
	/// [`pause_pair_with`](Self::pause_pair_with)), so the pair is retired mid-pass without leaving
	/// either side half-written. This call waits for that pass to end — up to
	/// [`REMOVE_CANCEL_GRACE`], after which the removal proceeds regardless and a straggler's write
	/// fails on its foreign key. A pass still READING the two sides is reached too, and waited for:
	/// it takes its gate before its scan, so the cancel drops the read where it stands and the pass
	/// ends without planning anything. A control verb made while this waits cannot take that cancel
	/// back: [`pause`](Self::pause_pair), [`resume`](Self::resume_pair) and
	/// [`cancel_paused_actions`](Self::cancel_paused_actions) are all REFUSED, rather than quietly
	/// accepted on a pair that is on its way out.
	///
	/// A watch running on the pair STOPS: its loop has nothing left to sync, and every pass it tried
	/// would fail against a pair that is gone. It ends and publishes a
	/// [`PairRemoved`](super::WatchState::PairRemoved) status; its
	/// [`WatchHandle`](super::WatchHandle) stays valid, so a caller holding one can still drop or
	/// stop it.
	pub async fn remove_pair(&self, pair: PairId) -> Result<(), Error> {
		// FIRST, before anything of the pair is retired: stop the pass in flight and wait for it to
		// end. Until it does it may still write — its rows name a pair id that still exists — so a
		// cancel after the delete would leave a completed action racing the cascade.
		self.cancel_pass_in_flight(pair).await;
		self.approvals.lock().await.remove(&pair);
		// Dropping the handle unsubscribes the pair's cache notifications.
		self.roots.lock().await.remove(&pair);
		// The pair's journal rows go with it (`ON DELETE CASCADE`); drop the in-memory copies too,
		// so nothing of the removed pair is left to be consulted or re-persisted.
		self.pending.forget_pair(pair);
		let store = self.store.lock().await;
		// Under the store lock, which is also the lock a watch reads the pair and subscribes under
		// (see `watchable_pair`): a watch setting up right now either registered before this and
		// gets tripped here, or reads the deleted row afterwards and is refused. Before the delete,
		// so a loop sitting between two passes learns about the removal at once rather than starting
		// one more against the rows this is about to take away.
		if let Some(signal) = self.removals.lock().await.remove(&pair) {
			let _ = signal.send(true);
		}
		store
			.delete_pair(pair)
			.map_err(|e| db_error(e, "removing a sync pair"))?;
		// Under the same lock as the delete, and as `set_control`'s own write: a pause that landed
		// in between would leave the map holding an id whose row is gone. The channel itself is
		// already `Retired` — that is what kept a pause or resume made while this WAITED for the
		// pass from taking the cancel back — so there is nothing left to say on it.
		self.paused.lock().await.remove(&pair);
		self.reading.lock().await.remove(&pair);
		Ok(())
	}

	/// Stop the pass in flight on `pair` and wait (up to [`REMOVE_CANCEL_GRACE`]) for it to end: a
	/// suspension becomes a cancel, the transfer running is dropped, and every action behind either
	/// is skipped, so the pass returns an [`interrupted`](SyncReport::interrupted) report. Waits on
	/// the pass's own gate going away, which is what tells the two apart — a pass that has ended
	/// from one that has merely been told to.
	///
	/// Holds NO other lock meanwhile: the pass it waits for takes the store lock on its way out, to
	/// record what it did and what failed.
	async fn cancel_pass_in_flight(&self, pair: PairId) {
		// Only a pass that has ASKED for its gate is reachable — and waited for — here, which a pass
		// does before it reads either side, so scanning and planning are covered as well as
		// applying. A pass that asks for its gate AFTER this is stopped by `pass_gate` refusing it,
		// under the same store lock this removal deletes the pair row under.
		let control = self.control_channel(pair).await;
		// `Retired`, not `Cancelled`: the cancel has to keep the last word for the whole wait. A
		// pause landing in it would otherwise re-park the pass — on a state nothing left in the
		// engine can change, holding the drive-write lock until the grace runs out — and a resume
		// would let it apply the rest of its plan against a pair being taken away.
		control.send_replace(PassControl::Retired);
		if tokio::time::timeout(REMOVE_CANCEL_GRACE, control.closed())
			.await
			.is_err()
		{
			tracing::warn!(
				"remove_pair[pair {pair}]: the pass in flight has not ended after \
				 {REMOVE_CANCEL_GRACE:?}; retiring the pair anyway"
			);
		}
	}

	/// Pause `pair`: its watch loop stops running passes, and [`sync_once`](Self::sync_once)
	/// returns a report marked [`paused`](SyncReport::paused) without scanning, planning or
	/// touching either side. Changes made meanwhile are not lost — the watch loop leaves its
	/// change signal pending, so the first pass after [`resume_pair`](Self::resume_pair) catches
	/// up on everything at once.
	///
	/// A pass that is ALREADY RUNNING is suspended: it parks before its next action and resumes
	/// exactly where it stopped, giving up its transfers only if the pause outlives
	/// [`DEFAULT_CANCEL_AFTER`](super::DEFAULT_CANCEL_AFTER) —
	/// [`pause_pair_with`](Self::pause_pair_with) is where that choice lives, and this is
	/// [`PauseOptions::default`].
	///
	/// A pause does NOT tear the pair down: its filesystem watcher and its cache sync-root
	/// subscription stay registered. Dropping the subscription would untrack the root in the cache,
	/// and re-registering it would then relist the whole root under the drive lock — a resync
	/// nobody asked for as the price of resuming.
	///
	/// The flag is persisted with the pair, so an engine reopened on the same baseline DB comes
	/// back paused (a suspended PASS is process-local: a restart has nothing to resume, and nothing
	/// was half-written). Pausing an already-paused pair re-applies the options; an unknown pair, and
	/// one whose [`removal`](Self::remove_pair) is already under way, are errors. A pair that is
	/// already paused cannot be WATCHED, either — [`watch`](Self::watch) refuses it rather than hand
	/// out a handle whose loop does nothing.
	pub async fn pause_pair(&self, pair: PairId) -> Result<(), Error> {
		self.pause_pair_with(pair, PauseOptions::default()).await
	}

	/// [`pause_pair`](Self::pause_pair), choosing what happens to the pass already in flight.
	///
	/// * [`PauseMode::Suspend`](super::PauseMode::Suspend) parks that pass between actions and
	///   keeps it alive: a resume continues the same pass, with the same plan, the same drive lock
	///   and everything it had already done. A transfer already running finishes — the native
	///   upload/download paths poll no pause signal of their own, so there is no mid-transfer park
	///   to take, and the parking happens at the next action boundary.
	/// * [`PauseMode::Cancel`](super::PauseMode::Cancel) drops the transfers in flight, skips the
	///   rest of the plan, and lets the pass return a report whose
	///   [`interrupted`](SyncReport::interrupted) counts what it did not do. An interrupted action
	///   records nothing: an interrupted download removes its temp file and puts back whatever it
	///   had stashed out of its way, an interrupted upload
	///   normally never reaches the `upload/done` that would make it a file, and a transfer that DID
	///   finish is never dropped between that and the baseline row recording it. The next pass
	///   re-plans the remainder — including an upload the server committed as it was dropped, which
	///   it re-uploads over the same name.
	///
	/// [`cancel_after`](PauseOptions::cancel_after) is what converts the first into the second, so a
	/// UI can offer one button: a short pause resumes its transfers where they were, a long one
	/// gives them up rather than sit on the drive-write lock. It is counted FROM THIS CALL — the
	/// deadline is stamped here and shared by every action of the pass, so a countdown shown next to
	/// the button is the one the pass keeps, whether it parks at once or is still finishing a 500 MB
	/// upload when the window runs out. Re-issuing the pause with new options restarts it. `None`
	/// never escalates — the pass then parks until [`resume_pair`](Self::resume_pair) or
	/// [`cancel_paused_actions`](Self::cancel_paused_actions), holding the drive lock throughout,
	/// and a [`sync_once`](Self::sync_once) call waiting on that pass waits with it.
	pub async fn pause_pair_with(&self, pair: PairId, options: PauseOptions) -> Result<(), Error> {
		self.set_control(pair, PassControl::pausing(options)).await
	}

	/// Turn a [`suspended`](super::PauseMode::Suspend) pause into a
	/// [`cancelling`](super::PauseMode::Cancel) one: the pass parked on this pair unwinds now
	/// instead of waiting out its [`cancel_after`](PauseOptions::cancel_after) window. The pair
	/// STAYS paused.
	///
	/// A pair that is not paused has no pass to unwind: that is an error, not a no-op — the caller
	/// asked to give up transfers nothing is holding back, which means it believes the pair to be in
	/// a state it is not (pause it first if that is what you meant). An unknown pair is an error
	/// too, and so is one whose [`removal`](Self::remove_pair) is already under way.
	pub async fn cancel_paused_actions(&self, pair: PairId) -> Result<(), Error> {
		let control = self.pair_control(pair).await?;
		let state = *control.borrow();
		// A pair on its way out reads as paused — `Retired` is every bit as not-`Run` as a pause is —
		// but it is not a pause anyone may act on: the removal's cancel is already final and the row
		// goes moments later, so this has nothing to convert and no pair to leave paused afterwards.
		// Refused with the same words the other control verbs use, before anything is written.
		if state == PassControl::Retired {
			return Err(being_removed(pair));
		}
		if !state.is_paused() {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!("sync pair {pair} is not paused"),
			));
		}
		cancel_suspension(&control);
		Ok(())
	}

	/// [`cancel_paused_actions`](Self::cancel_paused_actions) for a caller inside the engine, on a
	/// pair it need not have checked first: it cuts loose a pass parked on a suspension without
	/// touching the persisted paused flag. A watch being stopped uses it, so a loop whose pass is
	/// parked ends instead of waiting for a resume that whoever stopped it is not going to send.
	pub(super) async fn cancel_suspended_pass(&self, pair: PairId) {
		if let Some(control) = self.paused.lock().await.get(&pair) {
			cancel_suspension(control);
		}
	}

	/// Resume a [`paused`](Self::pause_pair) pair. Nothing is re-listed and no baseline is
	/// invalidated: whatever changed on either side while the pair was paused is reconciled by the
	/// next pass exactly as if it had changed a moment ago — and a pass SUSPENDED mid-way carries
	/// on from where it parked. Resuming a running pair is a no-op; an unknown pair, and one whose
	/// [`removal`](Self::remove_pair) is already under way, are errors.
	///
	/// A pause is also the one stretch where no pass runs to confirm the pushes made just before
	/// it, so the resume sweeps those (see [`CONFIRM_TENURE`]) — a `plan_pair` between the resume
	/// and the next pass then reads the same agreed content the pass will.
	pub async fn resume_pair(&self, pair: PairId) -> Result<(), Error> {
		self.set_control(pair, PassControl::Run).await?;
		self.sweep_confirmations(pair).await
	}

	/// Whether `pair` is currently [`paused`](Self::pause_pair) — in either flavour. An unknown
	/// pair reads as not paused (it has no state to be paused in).
	pub async fn is_paused(&self, pair: PairId) -> bool {
		self.paused
			.lock()
			.await
			.get(&pair)
			.is_some_and(|control| control.borrow().is_paused())
	}

	/// The checkpoint a pass of `pair` parks on, cloned per pass (see the pause module). Taken at the
	/// TOP of a pass, before either side is read, so a cancel covers the whole of it.
	///
	/// Taken under the store lock, and REFUSED for a pair the registry no longer knows: a pass that
	/// asks for its gate after [`remove_pair`](Self::remove_pair) has finished learns here that its
	/// pair is gone — before it reads or applies anything. Under the same lock as the delete, so the
	/// two cannot interleave: either the gate is in the map before the removal reads it, and the
	/// removal cancels it and waits for it, or the row is already gone and there is no pass to gate.
	async fn pass_gate(&self, pair: PairId) -> Result<PassGate, Error> {
		let store = self.store.lock().await;
		if store
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.is_none()
		{
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		Ok(PassGate::new(self.control_channel(pair).await))
	}

	/// `pair`'s reading lock (see the `reading` field), created if this is the first time anyone asked.
	async fn reading_lock(&self, pair: PairId) -> Arc<Mutex<()>> {
		Arc::clone(self.reading.lock().await.entry(pair).or_default())
	}

	/// `pair`'s control channel, created (running) if this is the first time anyone asked.
	async fn control_channel(&self, pair: PairId) -> tokio::sync::watch::Sender<PassControl> {
		self.paused
			.lock()
			.await
			.entry(pair)
			.or_insert_with(|| tokio::sync::watch::channel(PassControl::Run).0)
			.clone()
	}

	/// `pair`'s control channel, refusing a pair the registry does not know.
	async fn pair_control(
		&self,
		pair: PairId,
	) -> Result<tokio::sync::watch::Sender<PassControl>, Error> {
		let store = self.store.lock().await;
		if store
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.is_none()
		{
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		drop(store);
		Ok(self.control_channel(pair).await)
	}

	/// The pair a watch is about to start on, together with the signal that ends its loop.
	///
	/// Both come out of ONE store-lock acquisition, and [`remove_pair`](Self::remove_pair) trips the
	/// signal under that same lock, so a removal racing a watch's setup has exactly two outcomes:
	/// either it deleted the row first and this reports an unknown pair, or it finds the
	/// subscription already registered and trips it. Subscribing afterwards — the setup does real
	/// work, a cache registration and a filesystem watcher, in between — would let a removal fall
	/// into the gap and go unheard, leaving the loop retrying a pair that is gone.
	pub(super) async fn watchable_pair(
		&self,
		pair: PairId,
	) -> Result<(PairRecord, tokio::sync::watch::Receiver<bool>), Error> {
		let store = self.store.lock().await;
		let record = store
			.pair(pair)
			.map_err(|e| db_error(e, "loading pair"))?
			.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?;
		if record.paused {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!("sync pair {pair} is paused: resume it before starting a watch on it"),
			));
		}
		let removed = self.removal_signal(pair).await;
		drop(store);
		Ok((record, removed))
	}

	/// A signal for `pair`'s watch loop to stop on: it flips to `true` when the pair is removed (see
	/// [`remove_pair`](Self::remove_pair)). Watches on the same pair share one signal.
	pub(super) async fn removal_signal(&self, pair: PairId) -> tokio::sync::watch::Receiver<bool> {
		self.removals
			.lock()
			.await
			.entry(pair)
			.or_insert_with(|| tokio::sync::watch::channel(false).0)
			.subscribe()
	}

	async fn set_control(&self, pair: PairId, control: PassControl) -> Result<(), Error> {
		// Both writes happen under the store lock, so a concurrent `remove_pair` cannot land between
		// them and leave the map holding a pair it has already deleted.
		let store = self.store.lock().await;
		// A pair whose removal is under way is REFUSED before anything is written: persisting a flag
		// on a row that is deleted moments later describes nothing, and the caller would otherwise
		// be told a pause took that in truth changed neither the pass nor the pair. Read WITHOUT
		// creating the channel, so an unknown pair leaves no entry behind; only a pair a removal has
		// reached can be `Retired`.
		let retired = self
			.paused
			.lock()
			.await
			.get(&pair)
			.is_some_and(|state| *state.borrow() == PassControl::Retired);
		if retired {
			return Err(being_removed(pair));
		}
		// The persisted flag FIRST: an in-memory pause the DB never learned about would silently
		// un-pause on the next open, which is the one direction that loses data protection.
		let known = store
			.set_paused(pair, control.is_paused())
			.map_err(|e| db_error(e, "persisting a sync pair's paused flag"))?;
		if !known {
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		// A removal that started since the check above still keeps the last word — `Retired` is
		// final, and the flag just persisted goes with the row — so report it as the refusal it is.
		let applied = self
			.control_channel(pair)
			.await
			.send_if_modified(|state| match state {
				PassControl::Retired => false,
				state => {
					*state = control;
					true
				}
			});
		if !applied {
			return Err(being_removed(pair));
		}
		Ok(())
	}

	/// Approve the mass-delete batch the guard is currently holding for `pair`, naming it by the
	/// `pass_token` the hold reported ([`SyncReport::deletion_token`] or
	/// [`SyncEvent::DeletionsHeld`]).
	///
	/// The approval is ONE-SHOT and batch-specific: the next pass whose held deletions hash to the
	/// same token applies them; any other batch — different paths, or a different reason to hold —
	/// is held again under a fresh token, so an approval can never leak onto deletions the caller
	/// never saw.
	pub async fn approve_deletions(&self, pair: PairId, pass_token: &str) {
		self.approvals
			.lock()
			.await
			.insert(pair, pass_token.to_string());
	}

	/// Consume a pending approval for `pair` if it names exactly this batch.
	async fn take_approval(&self, pair: PairId, pass_token: &str) -> bool {
		let mut approvals = self.approvals.lock().await;
		if approvals.get(&pair).is_some_and(|held| held == pass_token) {
			approvals.remove(&pair);
			true
		} else {
			false
		}
	}

	/// Record what this pass's attempted paths did: a failure extends that path's streak, a success
	/// ends it. Once a streak reaches [`MAX_PATH_FAILURES`] the path stops being planned and is
	/// reported as [`UnsyncableReason::RepeatedFailure`] instead of failing on every pass forever —
	/// until [`PATH_FAILURE_RETRY_INTERVAL`] after its last failure, when it is tried once more.
	async fn note_path_outcomes(
		&self,
		pair: PairId,
		attempted: &[String],
		report: &mut SyncReport,
	) {
		if attempted.is_empty() {
			return;
		}
		let failed: HashMap<&str, &str> = report
			.failed_paths
			.iter()
			.map(|(path, error)| (path.as_str(), error.as_str()))
			.collect();
		let store = self.store.lock().await;
		let mut problems = Vec::new();
		let now = Utc::now().timestamp_millis();
		for (path, error) in &failed {
			if let Err(e) = store.record_failure(pair, path, error, now) {
				problems.push(format!("{path}: recording the failure count failed: {e}"));
			}
		}
		// A pass cut short by a cancel cannot tell which of its actions ran, and an action that
		// never ran proves nothing about the path: clearing its streak would hand a permanently
		// broken path a fresh set of retries every time someone pauses. Only the failures count. A
		// pass that found a side full held back transfers the same way.
		if report.interrupted == 0 && report.halted.is_none() {
			for path in attempted
				.iter()
				.filter(|p| !failed.contains_key(p.as_str()))
			{
				if let Err(e) = store.clear_failure(pair, path) {
					problems.push(format!("{path}: clearing the failure count failed: {e}"));
				}
			}
		}
		report.errors.extend(problems);
	}

	/// Plan `rel_path` again on the next pass, whatever its failure history: it clears the
	/// consecutive-failure count the engine stopped planning it on (see
	/// [`UnsyncableReason::RepeatedFailure`]). Without it the engine tries such a path once per
	/// [`PATH_FAILURE_RETRY_INTERVAL`] on its own.
	///
	/// Idempotent — a path with no failure streak is left alone rather than erroring, so a caller
	/// can retry a whole reported list without checking each entry first. Errors only if the pair
	/// is unknown.
	pub async fn retry_path(&self, pair: PairId, rel_path: &str) -> Result<(), Error> {
		let store = self.store.lock().await;
		if store
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.is_none()
		{
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		store
			.clear_failure(pair, rel_path)
			.map_err(|e| db_error(e, "clearing a path's failure count"))
	}

	/// The conflicts the engine is holding for `pair`, ordered by path: every conflict a pass
	/// reported in [`SyncReport::conflicts`] that [`resolve_conflict`](Self::resolve_conflict) has
	/// not resolved yet, with what each side held when it was recorded. This includes an upload of
	/// this engine's that went on top of a version it never saw.
	///
	/// Reads the persisted baseline only: no scan, no server call, nothing written, and it does not
	/// wait for a pass in flight. A conflict that pass has yet to record appears once it has.
	/// Errors only if the pair is unknown.
	pub async fn list_conflicts(&self, pair: PairId) -> Result<Vec<PlannedConflict>, Error> {
		let store = self.store.lock().await;
		if store
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.is_none()
		{
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		Ok(store
			.entries(pair)
			.map_err(|e| db_error(e, "loading the held conflicts"))?
			.into_iter()
			.filter(|entry| entry.state.is_conflict())
			.map(|entry| PlannedConflict {
				local: entry.local_kind.map(PlannedNodeKind::from),
				remote: entry.remote_kind.map(PlannedNodeKind::from),
				rel_path: entry.rel_path,
			})
			.collect())
	}

	/// Run one full sync pass: plan, screen, and apply against the remote and local tree. A
	/// [`paused`](Self::pause_pair) pair returns a report marked
	/// [`paused`](SyncReport::paused) instead, having read neither side.
	///
	/// A pause that lands WHILE this runs reaches into the pass (see
	/// [`pause_pair_with`](Self::pause_pair_with)) from its first step, the read of the two sides,
	/// to its last: a suspension parks it — this call keeps waiting, so whoever wants it back must
	/// resume the pair or cancel its actions — and a cancel returns the pass's report early, with
	/// [`interrupted`](SyncReport::interrupted) counting what it did not do. A cancel that lands
	/// before there is a plan to count returns the [`paused`](SyncReport::paused) report instead,
	/// having read one side and applied nothing; if the pair was being REMOVED, that is an error.
	pub async fn sync_once(&self, pair: PairId) -> Result<SyncReport, Error> {
		self.sync_once_observed(pair, &mut |_| {}).await
	}

	/// Like [`sync_once`](Self::sync_once), but reports live progress: `observer` is invoked with
	/// each [`SyncEvent`] as the pass plans and applies its actions (see [`SyncEvent`] for the
	/// event order). The observer is called synchronously between async steps, so keep it quick.
	///
	/// A [`paused`](Self::pause_pair) pair emits NO events at all — there was no pass to report on
	/// — and returns a report marked [`paused`](SyncReport::paused). A pass that returns an error
	/// reports it as [`SyncEvent::PassFailed`] first.
	///
	/// A panic inside `observer` is caught and logged once; the observer receives nothing more for
	/// the rest of the pass, and the pass carries on (see [`SyncObserver`](super::SyncObserver)).
	pub async fn sync_once_observed(
		&self,
		pair: PairId,
		observer: &mut (dyn FnMut(SyncEvent) + Send),
	) -> Result<SyncReport, Error> {
		self.sync_pass(pair, ScanDepth::Fast, observer).await
	}

	/// [`sync_once_observed`](Self::sync_once_observed) with the local scan at `depth`. A
	/// [`Deep`](ScanDepth::Deep) pass re-hashes every local file, so a same-size edit that kept its
	/// mtime is planned as the edit it is — and a download planned over such a file stashes it first
	/// (see `apply::stash_local_target`). The watch runs one on its deep-scan tick.
	pub(super) async fn sync_pass(
		&self,
		pair: PairId,
		depth: ScanDepth,
		observer: &mut (dyn FnMut(SyncEvent) + Send),
	) -> Result<SyncReport, Error> {
		let mut contained = super::events::contain_panics(observer);
		let observer: &mut (dyn FnMut(SyncEvent) + Send) = &mut contained;
		let result = self.run_pass(pair, depth, observer).await;
		if let Err(error) = &result {
			observer(SyncEvent::PassFailed {
				error: error.to_string(),
			});
		}
		result
	}

	/// The body of [`sync_pass`](Self::sync_pass), reporting to an observer whose panics are
	/// already contained.
	async fn run_pass(
		&self,
		pair: PairId,
		depth: ScanDepth,
		observer: &mut (dyn FnMut(SyncEvent) + Send),
	) -> Result<SyncReport, Error> {
		if self.is_paused(pair).await {
			tracing::debug!("sync_once[pair {pair}]: paused — neither side was read");
			return Ok(SyncReport {
				paused: true,
				..SyncReport::default()
			});
		}

		// The gate BEFORE either side is read, not before the first action: reading two trees takes
		// as long as writing them, and a pass holding no gate is one a cancel can neither reach nor
		// wait for — `remove_pair` would delete the rows out from under it and only learn of it when
		// a straggler's write failed. Held from here, a cancel covers the scan, the plan and the
		// apply alike. `plan_pair` deliberately takes none: it has no plan and no action to
		// interrupt, and nothing for a removal to wait on.
		let gate = self.pass_gate(pair).await?;
		// Held until this pass has recorded the conflicts it reads, so `resolve_conflict` cannot
		// land in between and be overwritten by a row written from a read that predates it. Taken
		// under the gate: a cancel reaches a pass waiting for it as well as one reading.
		let reading = self.reading_lock(pair).await;
		let Some((recording, prepared)) = gate
			.guard(async move {
				let recording = reading.lock_owned().await;
				(recording, self.prepare(pair, depth).await)
			})
			.await
		else {
			// Cancelled while still reading: no plan was made, so nothing was applied and no
			// baseline row written — the journal records the read had already retired stay retired,
			// in memory and in the DB alike, which is what keeps the two halves in step. A removal
			// took the pair with it and is said so; a plain cancel leaves the pair paused, which is
			// the report a pause made a moment earlier would have produced.
			if gate.retired() {
				return Err(being_removed(pair));
			}
			tracing::debug!("sync_once[pair {pair}]: cancelled while reading the two sides");
			return Ok(SyncReport {
				paused: true,
				..SyncReport::default()
			});
		};
		let prep = prepared?;
		// Persist what this pass confirmed. Only a real pass writes it: `plan_pair` stays a pure
		// read, so a dry run inside the confirmation window just leaves it for the next pass — and
		// leaves the evidence with it, which is why the records are retired HERE and not in the
		// reading step.
		if !prep.confirmed.is_empty() {
			let store = self.store.lock().await;
			for entry in &prep.confirmed {
				store
					.upsert_entry(pair, entry)
					.map_err(|e| db_error(e, "recording the confirmed agreed content"))?;
			}
		}
		self.forget_settled_pushes(&prep.confirmed);
		let mut report = SyncReport::default();

		tracing::debug!(
			"sync_once[pair {pair}]: mode {:?} — local scan {} node(s) (complete={}), remote view {} node(s) (converged={})",
			prep.record.mode,
			prep.local_scan.nodes.len(),
			prep.local_scan.complete,
			prep.remote_view.nodes.len(),
			prep.remote_converged,
		);
		observer(SyncEvent::PassStarted {
			mode: prep.record.mode,
		});

		report.unsyncable = prep.unsyncable();
		// Before the refusal check, so a refused pass still says what the scan could not read.
		report.errors.extend(prep.local_scan.reported_errors());

		if let Some(refusal) = refusal(&prep) {
			tracing::debug!("sync_once[pair {pair}]: refused — {refusal:?}");
			let reason = refusal.to_string();
			report.refused = Some(refusal);
			observer(SyncEvent::Refused {
				reason: reason.clone(),
			});
			report
				.errors
				.push(format!("refused: {reason}; resolve it and retry"));
			observer(SyncEvent::PassCompleted {
				report: report.clone(),
			});
			return Ok(report);
		}

		let state = screen_state(&prep);
		let mut screened = reconcile_and_screen(&prep, state);
		// A one-shot approval releases the held batch it names — and only that batch.
		if let Some(token) = screened.pass_token.clone()
			&& self.take_approval(pair, &token).await
		{
			tracing::debug!(
				"sync_once[pair {pair}]: approval {token} matched — applying {} held deletion(s)",
				screened.decision.held.len(),
			);
			screened.decision = guard::GuardDecision {
				safe: std::mem::take(&mut screened.all),
				held: Vec::new(),
				reason: None,
			};
			screened.pass_token = None;
		}
		let decision = screened.decision;
		report.conflicts = screened.conflicts;
		// `held` can also carry the create half of a held type flip; `held_deletions()` counts only
		// the deletions.
		report.held = prep.planned(&decision.held);
		report.guard = decision.reason.clone();
		report.deletion_token = screened.pass_token;
		report.deferred_paths = screened.deferred_paths;

		for conflict in &report.conflicts {
			let rel_path = &conflict.rel_path;
			// A row held because THIS engine's push buried a concurrent edit is the one kind the
			// current view cannot describe: the remote head is our own upload, and the version the
			// row is holding for the caller only exists in the file's history. Re-recording it from
			// the view would put our own copy on both sides of the conflict and lose the only
			// reference to the buried one.
			if prep
				.baseline
				.get(rel_path)
				.is_some_and(|row| row.state == BaselineState::Overwritten)
			{
				observer(SyncEvent::Conflict {
					rel_path: rel_path.clone(),
				});
				continue;
			}
			// HOLD the conflict in the baseline: the path (and its subtree) is excluded from
			// planning until `resolve_conflict` picks a winner, instead of being re-surfaced,
			// unresolvable, on every pass.
			if let Err(error) = apply::record_conflict(
				&self.store,
				pair,
				rel_path,
				prep.local_scan.nodes.get(rel_path),
				prep.remote_view.nodes.get(rel_path),
			)
			.await
			{
				report.errors.push(format!("{rel_path}: {error}"));
			}
			observer(SyncEvent::Conflict {
				rel_path: rel_path.clone(),
			});
		}
		// Every conflict row this pass will write is written: a resolution may land from here on.
		drop(recording);
		if report.held_deletions() > 0 {
			observer(SyncEvent::DeletionsHeld {
				count: report.held_deletions(),
				reason: report
					.guard
					.as_ref()
					.map(GuardReason::to_string)
					.unwrap_or_default(),
				pass_token: report.deletion_token.clone().unwrap_or_default(),
			});
		}
		observer(SyncEvent::Planned {
			actions: decision.safe.len(),
		});

		if decision.safe.is_empty() {
			tracing::debug!(
				"sync_once[pair {pair}]: nothing to apply ({} deletion(s) held, {} conflict(s), {} path(s) deferred)",
				report.held_deletions(),
				report.conflicts.len(),
				report.deferred_paths,
			);
			observer(SyncEvent::PassCompleted {
				report: report.clone(),
			});
			return Ok(report);
		}

		// Resolve the sync root to a remote directory (every top-level parent). Errors if the root
		// is not a reachable subfolder.
		let root_remote = self.client.get_dir(prep.record.remote_root).await?;

		let local_root = PathBuf::from(&prep.record.local_root);
		let ctx = ApplyContext {
			client: &self.client,
			local_root: &local_root,
			pair,
			mode: prep.record.mode,
			store: &self.store,
			local: &prep.local_scan.nodes,
			baseline: &prep.baseline,
			remote: &prep.remote_view.nodes,
			root_remote,
			absence_trusted: state.absence_trusted(),
			dirs: &prep.dirs,
			files: &prep.files,
			pending: &self.pending,
			observed: &self.observed,
			gate: &gate,
			lock_budget: self.lock_budget,
		};
		let attempted: Vec<String> = decision
			.safe
			.iter()
			.map(|action| action.rel_path().to_string())
			.collect();
		apply::apply(ctx, decision.safe, &mut report, observer).await;
		self.note_path_outcomes(pair, &attempted, &mut report).await;
		tracing::debug!(
			"sync_once[pair {pair}]: done — {} uploaded, {} downloaded, {} remote dir(s), {} local dir(s), {} trashed, {} locally deleted, {} moved remote, {} moved local, {} conflict(s), {} held, {} deferred, {} interrupted, {} error(s)",
			report.uploaded,
			report.downloaded,
			report.remote_dirs_created,
			report.local_dirs_created,
			report.remotely_trashed,
			report.locally_deleted,
			report.moved_remote,
			report.moved_local,
			report.conflicts.len(),
			report.held_deletions(),
			report.deferred_paths,
			report.interrupted,
			report.errors.len(),
		);
		observer(SyncEvent::PassCompleted {
			report: report.clone(),
		});
		Ok(report)
	}
}

/// A name collision (local or remote) makes a 1:1 mapping impossible — refuse the pass.
fn refusal(prep: &Prepared) -> Option<RefuseReason> {
	if prep
		.local_scan
		.errors
		.iter()
		.any(|e| matches!(e, ScanError::DuplicateName { .. }))
	{
		return Some(RefuseReason::LocalCollision);
	}
	if prep.remote_view.has_collisions {
		return Some(RefuseReason::RemoteCollision);
	}
	None
}

/// Why a [`Backlog::AdoptDestination`] switch must not act on the read it just made — `None` when
/// it can. Decided purely (no I/O) so the policy is unit-testable.
///
/// The bar is the pass's own: a 1:1 path mapping (no name collision on either side) plus
/// [`absence_trusted`](guard::ScreenState::absence_trusted), since the adoption is entirely a
/// judgement about which paths one side no longer has.
fn adoption_refusal(refused: Option<RefuseReason>, state: &guard::ScreenState) -> Option<String> {
	if let Some(reason) = refused {
		return Some(reason.to_string());
	}
	if state.absence_trusted() {
		return None;
	}
	Some(
		if !state.scan_complete {
			"the local scan is incomplete"
		} else if !state.remote_converged {
			"the remote view has never converged"
		} else {
			"the remote view came back wholly empty"
		}
		.to_string(),
	)
}

/// A reconciled, guard-screened pass.
struct Screened {
	/// Paths surfaced as two-way conflicts (held, never applied).
	conflicts: Vec<PlannedConflict>,
	/// Every executable action in apply order — what an APPROVED pass runs, held deletions and all.
	all: Vec<SyncAction>,
	decision: guard::GuardDecision,
	/// Identifies the held batch, when the guard held one.
	pass_token: Option<String>,
	/// Paths this pass deliberately left alone (see [`SyncReport::deferred_paths`]).
	deferred_paths: usize,
}

/// Reconcile the prepared inputs and screen deletions through the guard, splitting out conflicts.
fn reconcile_and_screen(prep: &Prepared, state: guard::ScreenState) -> Screened {
	let plan = plan::reconcile(
		prep.record.mode,
		&prep.baseline,
		&prep.local_scan.nodes,
		&prep.remote_view.nodes,
		&prep.holds,
	);
	let deferred_paths = plan.deferred_paths;
	// The directory moves run before everything else in the plan, which already names their
	// subtrees by the paths they move to. Copied: the dry run plans from the same borrowed `Prepared`.
	let mut actions = prep.dir_moves.clone();
	actions.extend(plan.actions);
	let actions = drop_blocked(actions, &prep.blocked_paths());
	let actions =
		withhold_deletions_over_unreachable(actions, &prep.unknown_remote, &prep.holds.held_remote);
	let actions = creates_before_dir_moves(actions);
	let (conflict_actions, executable): (Vec<_>, Vec<_>) = actions
		.into_iter()
		.partition(|a| matches!(a, SyncAction::Conflict { .. }));
	let conflicts = conflict_actions
		.iter()
		.map(|a| {
			planned_conflict(
				a.rel_path(),
				&prep.local_scan.nodes,
				&prep.remote_view.nodes,
			)
		})
		.collect();
	// The unscreened list is only needed to release an approved batch, which cannot happen unless
	// the pass plans a deletion at all — so a pure-transfer pass (a first sync, say) never pays for
	// the copy.
	let all = if executable.iter().any(SyncAction::is_delete) {
		executable.clone()
	} else {
		Vec::new()
	};
	let decision = guard::screen(executable, state, prep.record.delete_guard);
	let pass_token = (!decision.held.is_empty()).then(|| deletion_batch_token(&decision.held));
	Screened {
		conflicts,
		all,
		decision,
		pass_token,
		deferred_paths,
	}
}

/// Drop every action at a blocked path, and everything under it.
///
/// A path is blocked when the remote would reject its name, when its failure streak ran out (and
/// stays blocked until a [`retry_path`](SyncEngine::retry_path) or a rename clears it, or
/// [`PATH_FAILURE_RETRY_INTERVAL`] passes and it is tried once more), or when it was synced and its
/// remote item is still there but out of the view (see [`plan::unknown_remote_paths`]). The SUBTREE
/// goes with it every time: a name the remote refuses can hold no remote children, the failures
/// that get this far are structural — a directory that cannot be created can hold no children, a
/// local tree that cannot be written to cannot take a file — so planning the descendants would just
/// start the same streak one level down, and what sits under a directory the view cannot see is as
/// unknown as the directory.
///
/// Dropping the destination half of a MOVE takes the deletions that would strand its source with it
/// (see below): an unrelocatable item must not be deleted from the side that still holds it.
fn drop_blocked(actions: Vec<SyncAction>, blocked: &BTreeSet<String>) -> Vec<SyncAction> {
	if blocked.is_empty() {
		return actions;
	}
	// The sources of the moves this dropped: their content is staying exactly where it is.
	let mut stranded: Vec<String> = Vec::new();
	let kept: Vec<SyncAction> = actions
		.into_iter()
		.filter(|action| {
			let path = action.rel_path();
			if !blocked.contains(path) && !blocked.iter().any(|root| plan::is_under(path, root)) {
				return true;
			}
			tracing::debug!(
				"reconcile: skipping {} — its path is blocked (a name the remote rejects, or \
				 {MAX_PATH_FAILURES} consecutive failures)",
				action.describe()
			);
			if let SyncAction::MoveRemote { from_path, .. }
			| SyncAction::MoveLocal { from_path, .. } = action
			{
				stranded.push(from_path.clone());
			}
			false
		})
		.collect();
	if stranded.is_empty() {
		return kept;
	}
	// A move the pass could not make leaves its source in place — and the source reads as "gone" on
	// the other side, which is what planned the deletion in the first place. Deleting it now (or
	// deleting the directory above it, which is recursive) would destroy the only copy of content
	// this pass just refused to relocate. Renaming a synced item into a name the remote rejects is
	// exactly that shape.
	kept.into_iter()
		.filter(|action| {
			let path = action.rel_path();
			let strands = action.is_delete()
				&& stranded
					.iter()
					.any(|from| from == path || plan::is_under(from, path));
			if strands {
				tracing::debug!(
					"reconcile: skipping {} — it still holds content a blocked move could not relocate",
					action.describe()
				);
			}
			!strands
		})
		.collect()
}

/// Drop a deletion of a directory above a path whose remote item still exists where this pass cannot
/// act on it, and the create a type flip pairs with it at that path (whose own stash or overwrite
/// would take the directory just the same). Deletions are recursive; this is the per-path plan of a
/// directory move the fold could not make.
///
/// - Under a path the cache is holding mid-transition (`held`), both sides wait: the reconcile
///   deferred that path, and the next pass reads it settled.
/// - Under a synced path whose remote item is out of the view (`unknown`), only a LOCAL deletion
///   waits, for as long as the item stays out of reach: the local copy is the only readable one. A
///   remote trash there still runs — the local side already let go of its copy, and a trash is
///   recoverable.
///
/// A deletion around a path blocked for any other reason (a rejected name, an alias, a park) runs as
/// it did, into the recoverable quarantine or trash.
fn withhold_deletions_over_unreachable(
	actions: Vec<SyncAction>,
	unknown: &BTreeMap<String, UnsyncableReason>,
	held: &HashSet<String>,
) -> Vec<SyncAction> {
	let withheld: BTreeSet<String> = actions
		.iter()
		.filter(|action| {
			let dir = action.rel_path();
			let above_held = held.iter().any(|path| plan::is_under(path, dir));
			match action {
				SyncAction::DeleteLocal { .. } => {
					above_held || unknown.keys().any(|path| plan::is_under(path, dir))
				}
				SyncAction::TrashRemote { .. } => above_held,
				_ => false,
			}
		})
		.map(|action| action.rel_path().to_string())
		.collect();
	if withheld.is_empty() {
		return actions;
	}
	actions
		.into_iter()
		.filter(|action| {
			let waits =
				(action.is_delete() || action.is_create()) && withheld.contains(action.rel_path());
			if waits {
				tracing::debug!(
					"reconcile: skipping {} — the directory holds an item the remote side cannot be \
					 acted on for yet",
					action.describe()
				);
			}
			!waits
		})
		.collect()
}

/// Run each directory move right after the creates of the new directories above its destination, on
/// the side it moves (see `plan::parents_ready`); everything else keeps its order. Those creates
/// are planned with the rest of the pass, which runs after the moves.
fn creates_before_dir_moves(actions: Vec<SyncAction>) -> Vec<SyncAction> {
	let (moves, mut rest): (Vec<_>, Vec<_>) = actions.into_iter().partition(|action| {
		matches!(
			action,
			SyncAction::MoveLocal {
				kind: NodeKind::Dir,
				..
			} | SyncAction::MoveRemote {
				kind: NodeKind::Dir,
				..
			}
		)
	});
	let mut ordered = Vec::with_capacity(moves.len() + rest.len());
	for action in moves {
		let local = matches!(action, SyncAction::MoveLocal { .. });
		let to = action.rel_path();
		ordered.extend(rest.extract_if(.., |create| match create {
			SyncAction::CreateLocalDir { rel_path } => local && plan::is_under(to, rel_path),
			SyncAction::CreateRemoteDir { rel_path } => !local && plan::is_under(to, rel_path),
			_ => false,
		}));
		ordered.push(action);
	}
	ordered.extend(rest);
	ordered
}

/// A stable identifier for one held deletion batch: the sorted `(side, path)` lines hashed. The
/// same set of held deletions always yields the same token, and adding, dropping or re-siding a
/// single deletion yields a different one — which is what makes an approval batch-specific.
fn deletion_batch_token(held: &[SyncAction]) -> String {
	let mut lines: Vec<String> = held
		.iter()
		.map(|action| match action {
			SyncAction::DeleteLocal { rel_path, .. } => format!("local\t{rel_path}"),
			SyncAction::TrashRemote { rel_path, .. } => format!("remote\t{rel_path}"),
			other => format!("other\t{}", other.rel_path()),
		})
		.collect();
	lines.sort_unstable();
	let mut hasher = blake3::Hasher::new();
	for line in &lines {
		hasher.update(line.as_bytes());
		hasher.update(b"\n");
	}
	hasher.finalize().to_hex()[..16].to_string()
}

/// How trustworthy this pass's inputs are, for the guard.
fn screen_state(prep: &Prepared) -> guard::ScreenState {
	guard::ScreenState {
		scan_complete: prep.local_scan.complete,
		remote_converged: prep.remote_converged,
		// A wholly empty remote snapshot while the baseline still tracks remote items: what a
		// transient backend/cache fault looks like, and it would otherwise delete the whole pair.
		remote_emptied: prep.remote_emptied,
		first_sync: prep.baseline.is_empty(),
		tracked: prep.baseline.len(),
	}
}

fn overlap_error(overlap: PairOverlap) -> Error {
	Error::custom_with_source(
		ErrorKind::InvalidState,
		overlap,
		Some("registering a sync pair whose roots overlap an existing one".to_string()),
	)
}

fn db_error(error: rusqlite::Error, context: &str) -> Error {
	Error::custom_with_source(ErrorKind::Internal, error, Some(context.to_string()))
}

/// The refusal every control verb gets once [`SyncEngine::remove_pair`] has started on the pair: its
/// row is deleted moments later, so there is nothing left for a pause or a resume to describe — and
/// either would take back the cancel the removal is waiting on — and nothing for a cancel of the
/// paused actions to convert, the removal's own cancel being final.
fn being_removed(pair: PairId) -> Error {
	Error::custom(
		ErrorKind::InvalidState,
		format!("sync pair {pair} is being removed"),
	)
}

#[cfg(test)]
mod tests {
	use std::collections::HashSet;

	use base64::{Engine as _, prelude::BASE64_STANDARD};
	use filen_types::{crypto::Blake3Hash, fs::StableUuid};
	use rsa::{RsaPrivateKey, pkcs8::EncodePrivateKey};

	use super::*;
	use crate::{
		auth::{StringifiedClient, http::ClientConfig, unauth::UnauthClient},
		sync::lock::MAX_SLEEP_TIME_DEFAULT,
		sync_engine::{
			PauseMode,
			baseline::{BaselineChange, NodeKind},
			plan::RemoteNode,
			scan::LocalNode,
		},
	};

	/// The pair whose writes the pending-write tests record.
	const PAIR: PairId = 1;

	fn upload(rel: &str) -> SyncAction {
		SyncAction::UploadFile {
			rel_path: rel.to_string(),
		}
	}

	#[test]
	fn a_blocked_path_takes_its_subtree_but_not_its_lookalikes_out_of_the_plan() {
		let actions = vec![
			upload("ok.txt"),
			upload("bad"),
			upload("bad/inner.txt"),
			upload("bad/deep/x.txt"),
			// A sibling whose name merely STARTS with the blocked one: a different path entirely.
			upload("badly.txt"),
			upload("badge/y.txt"),
		];
		let blocked = BTreeSet::from(["bad".to_string()]);
		let kept: Vec<String> = drop_blocked(actions.clone(), &blocked)
			.iter()
			.map(|a| a.rel_path().to_string())
			.collect();
		assert_eq!(kept, vec!["ok.txt", "badly.txt", "badge/y.txt"]);

		// Nothing blocked -> the plan is untouched.
		assert_eq!(drop_blocked(actions.clone(), &BTreeSet::new()), actions);
	}

	/// A synced file whose remote item dropped out of the view (its metadata stopped decoding) reads
	/// to the reconciler exactly like a remote deletion, and one deletion is far under the guard's
	/// floor, so the guard lets it through. The block on the unknown path is what keeps the local
	/// copy.
	#[test]
	fn a_synced_path_whose_remote_item_left_the_view_is_blocked_not_deleted() {
		let root = Uuid::new_v4();
		let uuid = Uuid::new_v4();
		let hash = Blake3Hash::from([3; 32]);
		let baseline = HashMap::from([(
			"doc.txt".to_string(),
			BaselineEntry {
				remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
				agreed_hash: Some(hash),
				..synced_file("doc.txt", uuid, hash, 4)
			},
		)]);
		let local = HashMap::from([(
			"doc.txt".to_string(),
			LocalNode {
				rel_path: "doc.txt".to_string(),
				kind: NodeKind::File,
				size: 4,
				mtime_millis: 0,
				content_hash: Some(hash),
			},
		)]);
		let view = plan::build_remote_view(
			root,
			&[],
			&[],
			&[crate::cache::UndecodableItem {
				uuid,
				parent: root,
				stable_uuid: Some(StableUuid::new_for_test(uuid)),
			}],
		);
		let reconciled = plan::reconcile(
			SyncMode::TwoWay,
			&baseline,
			&local,
			&view.nodes,
			&plan::PassHolds::default(),
		);
		assert_eq!(
			reconciled.actions,
			vec![SyncAction::DeleteLocal {
				rel_path: "doc.txt".to_string(),
				kind: NodeKind::File,
			}],
			"unblocked, the vanished item plans a local delete"
		);
		let healthy = guard::ScreenState {
			scan_complete: true,
			remote_converged: true,
			remote_emptied: false,
			first_sync: false,
			tracked: 1,
		};
		assert!(
			guard::screen(reconciled.actions.clone(), healthy, DeleteGuard::default())
				.held
				.is_empty(),
			"the guard alone lets that one delete through"
		);

		let (unknown, never_synced) = plan::unknown_remote_paths(&baseline, &view.skipped);
		assert_eq!(
			unknown,
			BTreeMap::from([("doc.txt".to_string(), UnsyncableReason::RemoteUndecodable)])
		);
		assert!(never_synced.is_empty(), "{never_synced:?}");
		assert!(
			drop_blocked(reconciled.actions, &unknown.keys().cloned().collect()).is_empty(),
			"the unknown path plans nothing"
		);
	}

	/// What a pass plans and reports once both sides are read: folded and screened exactly as a
	/// pass folds and screens them, with a guard that holds nothing back.
	fn folded_pass(
		mode: SyncMode,
		baseline: HashMap<String, BaselineEntry>,
		local: Vec<LocalNode>,
		remote_view: RemoteView,
		failures: HashMap<String, PathFailure>,
	) -> (Vec<SyncAction>, Vec<UnsyncablePath>) {
		run_prepared(prepared(mode, baseline, local, remote_view, failures))
	}

	/// Fold and screen `prep` as a pass does.
	fn run_prepared(mut prep: Prepared) -> (Vec<SyncAction>, Vec<UnsyncablePath>) {
		prep.fold_dir_moves();
		let screened = reconcile_and_screen(&prep, screen_state(&prep));
		let mut actions = screened.decision.safe;
		actions.extend(screened.decision.held);
		(actions, prep.unsyncable())
	}

	/// A pass's inputs before the fold, with a complete scan that found nothing it could not sync.
	fn prepared(
		mode: SyncMode,
		baseline: HashMap<String, BaselineEntry>,
		local: Vec<LocalNode>,
		remote_view: RemoteView,
		failures: HashMap<String, PathFailure>,
	) -> Prepared {
		let (unknown_remote, never_synced_remote) =
			plan::unknown_remote_paths(&baseline, &remote_view.skipped);
		Prepared {
			record: PairRecord {
				id: PAIR,
				local_root: String::new(),
				remote_root: Uuid::nil(),
				mode,
				delete_guard: DeleteGuard::unlimited(),
				paused: false,
			},
			baseline: Arc::new(baseline),
			dir_moves: Vec::new(),
			local_scan: LocalScan {
				nodes: local
					.into_iter()
					.map(|node| (node.rel_path.clone(), node))
					.collect(),
				complete: true,
				errors: Vec::new(),
				invalid_names: BTreeMap::new(),
				aliased_dirs: BTreeMap::new(),
			},
			holds: plan::PassHolds {
				trashed: HashSet::new(),
				held_remote: remote_view.held_paths.clone(),
			},
			remote_view,
			remote_converged: true,
			remote_emptied: false,
			failures,
			unknown_remote,
			never_synced_remote,
			confirmed: Vec::new(),
			dirs: Vec::new(),
			files: Vec::new(),
		}
	}

	fn dir_row(rel: &str, uuid: Uuid) -> BaselineEntry {
		BaselineEntry {
			kind: NodeKind::Dir,
			remote_uuid: Some(uuid),
			..synced_shell(rel)
		}
	}

	/// A synced file in its steady state: both sides agreed on `hash`, lineage id = uuid.
	fn file_row(rel: &str, uuid: Uuid, hash: Blake3Hash) -> BaselineEntry {
		BaselineEntry {
			remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
			agreed_hash: Some(hash),
			..synced_file(rel, uuid, hash, 4)
		}
	}

	fn remote_dir(rel: &str, uuid: Uuid) -> RemoteNode {
		RemoteNode {
			rel_path: rel.to_string(),
			kind: NodeKind::Dir,
			remote_uuid: uuid,
			stable_uuid: None,
			content_hash: None,
			size: 0,
			modified_millis: 0,
		}
	}

	fn local_dir(rel: &str) -> LocalNode {
		LocalNode {
			rel_path: rel.to_string(),
			kind: NodeKind::Dir,
			size: 0,
			mtime_millis: 0,
			content_hash: None,
		}
	}

	fn local_file(rel: &str, hash: Blake3Hash) -> LocalNode {
		LocalNode {
			rel_path: rel.to_string(),
			kind: NodeKind::File,
			size: 4,
			mtime_millis: 0,
			content_hash: Some(hash),
		}
	}

	fn view(
		nodes: Vec<RemoteNode>,
		skipped: Vec<plan::SkippedRemote>,
		held: &[&str],
	) -> RemoteView {
		RemoteView {
			nodes: nodes
				.into_iter()
				.map(|node| (node.rel_path.clone(), node))
				.collect(),
			has_collisions: false,
			held_paths: held.iter().map(|path| path.to_string()).collect(),
			skipped,
		}
	}

	/// Whether `action` deletes `path`, directly or with a directory above it.
	fn deletes(action: &SyncAction, path: &str) -> bool {
		action.is_delete() && (action.rel_path() == path || plan::is_under(path, action.rel_path()))
	}

	/// The uuids of a synced `docs/` holding `a.txt` and `x.bin`, and their contents.
	struct Docs {
		dir: Uuid,
		a: Uuid,
		x: Uuid,
		a_hash: Blake3Hash,
		x_hash: Blake3Hash,
	}

	impl Docs {
		fn new() -> Self {
			Self {
				dir: Uuid::new_v4(),
				a: Uuid::new_v4(),
				x: Uuid::new_v4(),
				a_hash: Blake3Hash::from([1; 32]),
				x_hash: Blake3Hash::from([2; 32]),
			}
		}

		fn baseline(&self) -> HashMap<String, BaselineEntry> {
			HashMap::from([
				("docs".to_string(), dir_row("docs", self.dir)),
				(
					"docs/a.txt".to_string(),
					file_row("docs/a.txt", self.a, self.a_hash),
				),
				(
					"docs/x.bin".to_string(),
					file_row("docs/x.bin", self.x, self.x_hash),
				),
			])
		}

		fn local(&self, at: &str) -> Vec<LocalNode> {
			vec![
				local_dir(at),
				local_file(&format!("{at}/a.txt"), self.a_hash),
				local_file(&format!("{at}/x.bin"), self.x_hash),
			]
		}

		/// `x.bin` undecodable, so only the directory and `a.txt` are placed.
		fn remote_with_undecodable_x(&self, at: &str) -> RemoteView {
			view(
				vec![
					remote_dir(at, self.dir),
					remote_file(&format!("{at}/a.txt"), self.a, self.a_hash, 4),
				],
				vec![plan::SkippedRemote {
					remote_uuid: self.x,
					stable_uuid: Some(StableUuid::new_for_test(self.x)),
					rel_path: at.to_string(),
					reason: UnsyncableReason::RemoteUndecodable,
				}],
				&[],
			)
		}
	}

	/// A synced directory the remote renamed, holding a child whose remote item stopped decoding, is
	/// still one local rename: the block on the child follows it into the renamed directory, so the
	/// child is neither quarantined as a remote deletion nor pushed back over the item, and it is
	/// reported where it now is on both sides.
	#[test]
	fn a_remote_dir_rename_carries_the_block_on_an_undecodable_child() {
		for mode in [SyncMode::TwoWay, SyncMode::RemoteToLocal] {
			let docs = Docs::new();
			let (actions, unsyncable) = folded_pass(
				mode,
				docs.baseline(),
				docs.local("docs"),
				docs.remote_with_undecodable_x("documents"),
				HashMap::new(),
			);
			assert_eq!(
				actions,
				vec![SyncAction::MoveLocal {
					from_path: "docs".to_string(),
					to_path: "documents".to_string(),
					kind: NodeKind::Dir,
				}],
				"{mode:?}"
			);
			assert_eq!(
				unsyncable,
				vec![UnsyncablePath {
					rel_path: "documents/x.bin".to_string(),
					reason: UnsyncableReason::RemoteUndecodable,
				}],
				"{mode:?}"
			);
		}
	}

	/// A remote move into a directory the local side does not have yet is still one move: the pass
	/// creates the new parent first, and the undecodable child moves with its directory.
	#[test]
	fn a_remote_dir_move_into_a_new_parent_creates_it_first() {
		for mode in [SyncMode::TwoWay, SyncMode::RemoteToLocal] {
			let docs = Docs::new();
			let mut remote = docs.remote_with_undecodable_x("fresh/documents");
			remote
				.nodes
				.insert("fresh".to_string(), remote_dir("fresh", Uuid::new_v4()));
			let (actions, unsyncable) = folded_pass(
				mode,
				docs.baseline(),
				docs.local("docs"),
				remote,
				HashMap::new(),
			);
			assert_eq!(
				actions,
				vec![
					SyncAction::CreateLocalDir {
						rel_path: "fresh".to_string(),
					},
					SyncAction::MoveLocal {
						from_path: "docs".to_string(),
						to_path: "fresh/documents".to_string(),
						kind: NodeKind::Dir,
					},
				],
				"{mode:?}"
			);
			assert_eq!(
				unsyncable,
				vec![UnsyncablePath {
					rel_path: "fresh/documents/x.bin".to_string(),
					reason: UnsyncableReason::RemoteUndecodable,
				}],
				"{mode:?}"
			);
		}
	}

	/// The push-side mirror: `mkdir fresh; mv docs fresh/documents` with an undecodable child is one
	/// remote move after the create of its new parent, so the child's local copy is not uploaded as a
	/// second, readable item.
	#[test]
	fn a_local_dir_move_into_a_new_parent_creates_it_first() {
		for mode in [SyncMode::TwoWay, SyncMode::LocalToRemote] {
			let docs = Docs::new();
			let mut local = docs.local("fresh/documents");
			local.push(local_dir("fresh"));
			let (actions, unsyncable) = folded_pass(
				mode,
				docs.baseline(),
				local,
				docs.remote_with_undecodable_x("docs"),
				HashMap::new(),
			);
			assert_eq!(
				actions,
				vec![
					SyncAction::CreateRemoteDir {
						rel_path: "fresh".to_string(),
					},
					SyncAction::MoveRemote {
						from_path: "docs".to_string(),
						to_path: "fresh/documents".to_string(),
						kind: NodeKind::Dir,
						remote_uuid: docs.dir,
					},
				],
				"{mode:?}"
			);
			assert_eq!(
				unsyncable,
				vec![UnsyncablePath {
					rel_path: "fresh/documents/x.bin".to_string(),
					reason: UnsyncableReason::RemoteUndecodable,
				}],
				"{mode:?}"
			);
		}
	}

	/// The same remote move where the fold cannot run (a new local directory already sits at the
	/// destination): the per-path plan quarantines the old directory, and that must not take the
	/// undecodable child's local copy with it.
	#[test]
	fn a_directory_delete_never_takes_a_blocked_child_with_it() {
		for mode in [SyncMode::TwoWay, SyncMode::RemoteToLocal] {
			let docs = Docs::new();
			let mut local = docs.local("docs");
			local.push(local_dir("documents"));
			let (actions, unsyncable) = folded_pass(
				mode,
				docs.baseline(),
				local,
				docs.remote_with_undecodable_x("documents"),
				HashMap::new(),
			);
			assert!(
				!actions.iter().any(|action| deletes(action, "docs/x.bin")),
				"{mode:?}: {actions:?}"
			);
			assert_eq!(
				unsyncable,
				vec![UnsyncablePath {
					rel_path: "docs/x.bin".to_string(),
					reason: UnsyncableReason::RemoteUndecodable,
				}],
				"{mode:?}"
			);
		}
	}

	/// A local rename of a directory holding a parked child is one remote move, and for the pass that
	/// makes it the child stays parked and reported at its new path. The streak itself stays at the
	/// old path in the store, so from the next pass the child is planned again, as after any rename.
	#[test]
	fn a_local_dir_rename_carries_the_park_on_a_failing_child() {
		let docs = Docs::new();
		let remote = view(
			vec![
				remote_dir("docs", docs.dir),
				remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
				// A later version of x.bin, the download that keeps failing.
				RemoteNode {
					stable_uuid: Some(StableUuid::new_for_test(docs.x)),
					..remote_file("docs/x.bin", Uuid::new_v4(), Blake3Hash::from([3; 32]), 4)
				},
			],
			Vec::new(),
			&[],
		);
		let now = Utc::now().timestamp_millis();
		let (actions, unsyncable) = folded_pass(
			SyncMode::TwoWay,
			docs.baseline(),
			docs.local("documents"),
			remote,
			HashMap::from([("docs/x.bin".to_string(), failure(MAX_PATH_FAILURES, now))]),
		);
		assert_eq!(
			actions,
			vec![SyncAction::MoveRemote {
				from_path: "docs".to_string(),
				to_path: "documents".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: docs.dir,
			}]
		);
		assert_eq!(
			unsyncable,
			vec![UnsyncablePath {
				rel_path: "documents/x.bin".to_string(),
				reason: UnsyncableReason::RepeatedFailure {
					attempts: MAX_PATH_FAILURES,
					last_error: "boom".to_string(),
				},
			}]
		);

		// The next pass: the move committed the rows at the new path, but the stored streak is still
		// keyed at the old one, so the child is planned again instead of being reported as parked. That
		// stale streak is still reported at the old path until `retry_path` clears it.
		let baseline = docs
			.baseline()
			.into_values()
			.map(|mut row| {
				row.rel_path = row.rel_path.replacen("docs", "documents", 1);
				(row.rel_path.clone(), row)
			})
			.collect();
		let remote = view(
			vec![
				remote_dir("documents", docs.dir),
				remote_file("documents/a.txt", docs.a, docs.a_hash, 4),
				RemoteNode {
					stable_uuid: Some(StableUuid::new_for_test(docs.x)),
					..remote_file(
						"documents/x.bin",
						Uuid::new_v4(),
						Blake3Hash::from([3; 32]),
						4,
					)
				},
			],
			Vec::new(),
			&[],
		);
		let (actions, unsyncable) = folded_pass(
			SyncMode::TwoWay,
			baseline,
			docs.local("documents"),
			remote,
			HashMap::from([("docs/x.bin".to_string(), failure(MAX_PATH_FAILURES, now))]),
		);
		assert!(
			actions
				.iter()
				.any(|action| action.rel_path() == "documents/x.bin"),
			"{actions:?}"
		);
		assert_eq!(
			unsyncable,
			vec![UnsyncablePath {
				rel_path: "docs/x.bin".to_string(),
				reason: UnsyncableReason::RepeatedFailure {
					attempts: MAX_PATH_FAILURES,
					last_error: "boom".to_string(),
				},
			}],
			"the streak stays reported where the store keeps it"
		);
	}

	/// A remote rename of `docs/` to `new/` with `b/` moved into it, while a child of `docs/` stopped
	/// decoding, folds into both moves in order, so the child follows its directory instead of
	/// holding back a deletion of `docs/` the user never made. The same holds for the local mirror.
	#[test]
	fn a_dir_moved_into_another_moved_dir_carries_an_undecodable_child() {
		let b = Uuid::new_v4();
		let b_file = Uuid::new_v4();
		let b_hash = Blake3Hash::from([7; 32]);
		let b_rows = |at: &str| {
			[
				(at.to_string(), dir_row(at, b)),
				(
					format!("{at}/1.txt"),
					file_row(&format!("{at}/1.txt"), b_file, b_hash),
				),
			]
		};
		let expected_unsyncable = vec![UnsyncablePath {
			rel_path: "new/x.bin".to_string(),
			reason: UnsyncableReason::RemoteUndecodable,
		}];

		for mode in [SyncMode::TwoWay, SyncMode::RemoteToLocal] {
			let docs = Docs::new();
			let mut baseline = docs.baseline();
			baseline.extend(b_rows("b"));
			let mut local = docs.local("docs");
			local.extend([local_dir("b"), local_file("b/1.txt", b_hash)]);
			let mut remote = docs.remote_with_undecodable_x("new");
			remote.nodes.extend([
				("new/b".to_string(), remote_dir("new/b", b)),
				(
					"new/b/1.txt".to_string(),
					remote_file("new/b/1.txt", b_file, b_hash, 4),
				),
			]);
			let (actions, unsyncable) = folded_pass(mode, baseline, local, remote, HashMap::new());
			assert_eq!(
				actions,
				vec![
					SyncAction::MoveLocal {
						from_path: "docs".to_string(),
						to_path: "new".to_string(),
						kind: NodeKind::Dir,
					},
					SyncAction::MoveLocal {
						from_path: "b".to_string(),
						to_path: "new/b".to_string(),
						kind: NodeKind::Dir,
					},
				],
				"pull, {mode:?}"
			);
			assert_eq!(unsyncable, expected_unsyncable, "pull, {mode:?}");
		}

		for mode in [SyncMode::TwoWay, SyncMode::LocalToRemote] {
			let docs = Docs::new();
			let mut baseline = docs.baseline();
			baseline.extend(b_rows("b"));
			let mut local = docs.local("new");
			local.extend([local_dir("new/b"), local_file("new/b/1.txt", b_hash)]);
			let mut remote = docs.remote_with_undecodable_x("docs");
			remote.nodes.extend([
				("b".to_string(), remote_dir("b", b)),
				(
					"b/1.txt".to_string(),
					remote_file("b/1.txt", b_file, b_hash, 4),
				),
			]);
			let (actions, unsyncable) = folded_pass(mode, baseline, local, remote, HashMap::new());
			assert_eq!(
				actions,
				vec![
					SyncAction::MoveRemote {
						from_path: "docs".to_string(),
						to_path: "new".to_string(),
						kind: NodeKind::Dir,
						remote_uuid: docs.dir,
					},
					SyncAction::MoveRemote {
						from_path: "b".to_string(),
						to_path: "new/b".to_string(),
						kind: NodeKind::Dir,
						remote_uuid: b,
					},
				],
				"push, {mode:?}"
			);
			assert_eq!(unsyncable, expected_unsyncable, "push, {mode:?}");
		}
	}

	/// A directory renamed only by case while the cache lists a child of it twice is not renamed
	/// this pass, as a move to another path is not, and the directory at the old spelling is not
	/// trashed with the held child in it.
	#[test]
	fn a_case_only_dir_rename_waits_out_a_held_child() {
		let docs = Docs::new();
		let mut baseline = docs.baseline();
		baseline.remove("docs/x.bin");
		let baseline = baseline
			.into_values()
			.map(|mut row| {
				row.rel_path = row.rel_path.replacen("docs", "Docs", 1);
				(row.rel_path.clone(), row)
			})
			.collect();
		let (actions, _) = folded_pass(
			SyncMode::TwoWay,
			baseline,
			vec![local_dir("docs"), local_file("docs/a.txt", docs.a_hash)],
			view(
				vec![
					remote_dir("Docs", docs.dir),
					remote_file("Docs/a.txt", docs.a, docs.a_hash, 4),
				],
				Vec::new(),
				&["Docs/a.txt"],
			),
			HashMap::new(),
		);
		assert!(
			!actions.iter().any(|action| matches!(
				action,
				SyncAction::MoveRemote {
					kind: NodeKind::Dir,
					..
				} | SyncAction::MoveLocal {
					kind: NodeKind::Dir,
					..
				}
			)),
			"{actions:?}"
		);
		assert!(
			!actions.iter().any(|action| deletes(action, "Docs/a.txt")),
			"{actions:?}"
		);
	}

	/// Only a LOCAL deletion waits for a child whose remote item is out of reach, because only there
	/// is the local copy the one readable copy left. A local `rm -r` still trashes the remote
	/// directory (recoverably) around an undecodable child, and a remote deletion around a rejected
	/// local name still quarantines the local directory (recoverably) as before.
	#[test]
	fn only_a_local_deletion_waits_for_an_unreachable_child() {
		let docs = Docs::new();
		let (actions, _) = folded_pass(
			SyncMode::TwoWay,
			docs.baseline(),
			Vec::new(),
			docs.remote_with_undecodable_x("docs"),
			HashMap::new(),
		);
		assert!(
			actions.iter().any(|action| matches!(
				action,
				SyncAction::TrashRemote { rel_path, .. } if rel_path == "docs"
			)),
			"{actions:?}"
		);

		let mut baseline = docs.baseline();
		baseline.remove("docs/x.bin");
		let mut prep = prepared(
			SyncMode::TwoWay,
			baseline,
			vec![local_dir("docs"), local_file("docs/a.txt", docs.a_hash)],
			view(Vec::new(), Vec::new(), &[]),
			HashMap::new(),
		);
		prep.local_scan
			.invalid_names
			.insert("docs/CON".to_string(), "reserved".to_string());
		let (actions, _) = run_prepared(prep);
		assert!(
			actions.iter().any(|action| matches!(
				action,
				SyncAction::DeleteLocal { rel_path, .. } if rel_path == "docs"
			)),
			"{actions:?}"
		);
	}

	/// The remote replaced a synced directory with a file while an undecodable child of it is still
	/// on the server. Withholding the local deletion alone would let the download's own stash take the
	/// directory, child and all, so the download waits with it.
	#[test]
	fn a_type_flip_waits_with_the_deletion_it_replaces() {
		let docs = Docs::new();
		let remote = view(
			vec![remote_file(
				"docs",
				Uuid::new_v4(),
				Blake3Hash::from([9; 32]),
				4,
			)],
			vec![plan::SkippedRemote {
				remote_uuid: docs.x,
				stable_uuid: Some(StableUuid::new_for_test(docs.x)),
				rel_path: "elsewhere".to_string(),
				reason: UnsyncableReason::RemoteBrokenParent,
			}],
			&[],
		);
		let (actions, _) = folded_pass(
			SyncMode::TwoWay,
			docs.baseline(),
			docs.local("docs"),
			remote,
			HashMap::new(),
		);
		assert!(
			!actions.iter().any(|action| action.rel_path() == "docs"),
			"{actions:?}"
		);
	}

	/// A park on the renamed directory itself is cleared by the rename, as any rename clears one: it
	/// does not follow the directory and hold back the move that carries it.
	#[test]
	fn a_park_on_a_renamed_directory_does_not_hold_back_its_move() {
		let docs = Docs::new();
		let remote = view(
			vec![
				remote_dir("documents", docs.dir),
				remote_file("documents/a.txt", docs.a, docs.a_hash, 4),
				remote_file("documents/x.bin", docs.x, docs.x_hash, 4),
			],
			Vec::new(),
			&[],
		);
		let now = Utc::now().timestamp_millis();
		let (actions, unsyncable) = folded_pass(
			SyncMode::TwoWay,
			docs.baseline(),
			docs.local("docs"),
			remote,
			HashMap::from([("docs".to_string(), failure(MAX_PATH_FAILURES, now))]),
		);
		assert_eq!(
			actions,
			vec![SyncAction::MoveLocal {
				from_path: "docs".to_string(),
				to_path: "documents".to_string(),
				kind: NodeKind::Dir,
			}]
		);
		assert!(unsyncable.is_empty(), "{unsyncable:?}");
	}

	/// A remote rename folded into a local move carries what the local scan could not sync under the
	/// directory — a rejected name, a symlink alias and its target — to the new path.
	#[test]
	fn a_local_move_carries_rejected_names_and_aliases() {
		let docs = Docs::new();
		let sub = Uuid::new_v4();
		let mut baseline = docs.baseline();
		baseline.insert("docs/sub".to_string(), dir_row("docs/sub", sub));
		let mut local = docs.local("docs");
		local.push(local_dir("docs/sub"));
		let remote = view(
			vec![
				remote_dir("documents", docs.dir),
				remote_dir("documents/sub", sub),
				remote_file("documents/a.txt", docs.a, docs.a_hash, 4),
				remote_file("documents/x.bin", docs.x, docs.x_hash, 4),
			],
			Vec::new(),
			&[],
		);
		let mut prep = prepared(SyncMode::TwoWay, baseline, local, remote, HashMap::new());
		prep.local_scan
			.invalid_names
			.insert("docs/CON".to_string(), "reserved".to_string());
		prep.local_scan
			.aliased_dirs
			.insert("docs/link".to_string(), "docs/sub".to_string());
		let (actions, unsyncable) = run_prepared(prep);
		assert_eq!(
			actions,
			vec![SyncAction::MoveLocal {
				from_path: "docs".to_string(),
				to_path: "documents".to_string(),
				kind: NodeKind::Dir,
			}]
		);
		assert_eq!(
			unsyncable,
			vec![
				UnsyncablePath {
					rel_path: "documents/CON".to_string(),
					reason: UnsyncableReason::InvalidName {
						detail: "reserved".to_string(),
					},
				},
				UnsyncablePath {
					rel_path: "documents/link".to_string(),
					reason: UnsyncableReason::LocalAlias {
						target: "documents/sub".to_string(),
					},
				},
			]
		);
	}

	/// A local rename folded into a remote move carries the report of a never-synced remote item
	/// under the directory to the new path.
	#[test]
	fn a_remote_move_carries_never_synced_remote_reports() {
		let docs = Docs::new();
		let remote = view(
			vec![
				remote_dir("docs", docs.dir),
				remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
				remote_file("docs/x.bin", docs.x, docs.x_hash, 4),
			],
			vec![plan::SkippedRemote {
				remote_uuid: Uuid::new_v4(),
				stable_uuid: None,
				rel_path: "docs".to_string(),
				reason: UnsyncableReason::RemoteUndecodable,
			}],
			&[],
		);
		let (actions, unsyncable) = folded_pass(
			SyncMode::TwoWay,
			docs.baseline(),
			docs.local("documents"),
			remote,
			HashMap::new(),
		);
		assert_eq!(
			actions,
			vec![SyncAction::MoveRemote {
				from_path: "docs".to_string(),
				to_path: "documents".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: docs.dir,
			}]
		);
		assert_eq!(
			unsyncable,
			vec![UnsyncablePath {
				rel_path: "documents".to_string(),
				reason: UnsyncableReason::RemoteUndecodable,
			}]
		);
	}

	/// A mode switch that adopts the destination's standing copies rewrites baseline rows for every
	/// path it reads as gone on the source side, so it may only run on evidence a pass would act on.
	/// Untrusted, the switch is worse than useless: it adopts nothing and then propagates the whole
	/// backlog the caller asked to keep, or adopts a whole pair from whichever side a transient fault
	/// left listing something.
	#[test]
	fn adopting_the_destination_is_refused_on_evidence_a_pass_would_not_act_on() {
		let healthy = guard::ScreenState {
			scan_complete: true,
			remote_converged: true,
			remote_emptied: false,
			first_sync: false,
			tracked: 5,
		};
		assert_eq!(adoption_refusal(None, &healthy), None);

		for (state, expected) in [
			(
				guard::ScreenState {
					scan_complete: false,
					..healthy
				},
				"the local scan is incomplete",
			),
			(
				guard::ScreenState {
					remote_converged: false,
					..healthy
				},
				"the remote view has never converged",
			),
			(
				guard::ScreenState {
					remote_emptied: true,
					..healthy
				},
				"the remote view came back wholly empty",
			),
		] {
			assert_eq!(adoption_refusal(None, &state).as_deref(), Some(expected));
		}

		// A name collision makes a 1:1 mapping impossible, so it refuses the switch for the same
		// reason it refuses a pass — even with everything else healthy.
		assert_eq!(
			adoption_refusal(Some(RefuseReason::LocalCollision), &healthy).as_deref(),
			Some(RefuseReason::LocalCollision.to_string().as_str())
		);
	}

	fn scan_root(tag: &str) -> PathBuf {
		let root = std::env::temp_dir().join(format!("filen_engine_{tag}_{}", Uuid::new_v4()));
		std::fs::create_dir_all(&root).unwrap();
		root
	}

	/// Reconcile a two-way pass over a real local scan, screened exactly as a pass screens it.
	fn planned_over_scan(
		root: &Path,
		baseline: &HashMap<String, BaselineEntry>,
		remote: &HashMap<String, RemoteNode>,
	) -> Vec<SyncAction> {
		let scan = scan::scan_local(root, &HashMap::new(), ScanDepth::Fast);
		let plan = plan::reconcile(
			SyncMode::TwoWay,
			baseline,
			&scan.nodes,
			remote,
			&plan::PassHolds::default(),
		);
		drop_blocked(plan.actions, &scan.blocked_paths().cloned().collect())
	}

	fn synced_file(rel: &str, uuid: Uuid, hash: Blake3Hash, size: u64) -> BaselineEntry {
		BaselineEntry {
			remote_uuid: Some(uuid),
			content_hash: Some(hash),
			size: Some(size),
			local_mtime: Some(0),
			..synced_shell(rel)
		}
	}

	fn remote_file(rel: &str, uuid: Uuid, hash: Blake3Hash, size: u64) -> RemoteNode {
		RemoteNode {
			rel_path: rel.to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid,
			stable_uuid: Some(StableUuid::new_for_test(uuid)),
			content_hash: Some(hash),
			size,
			modified_millis: 0,
		}
	}

	/// A link to a directory inside the root that an earlier pass synced as a directory of its own:
	/// its copy is not trashed, and nothing the remote holds under the link's name is downloaded
	/// through the link into the real directory.
	#[cfg(unix)]
	#[test]
	fn a_symlink_to_a_directory_inside_the_root_plans_nothing_under_the_link() {
		let root = scan_root("alias");
		std::fs::create_dir(root.join("real")).unwrap();
		std::fs::write(root.join("real").join("a.txt"), b"payload").unwrap();
		std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
		let hash: Blake3Hash = blake3::hash(b"payload").into();
		let other: Blake3Hash = blake3::hash(b"remote only").into();

		let mut baseline = HashMap::new();
		let mut remote = HashMap::new();
		for dir in ["real", "link"] {
			let uuid = Uuid::new_v4();
			baseline.insert(
				dir.to_string(),
				BaselineEntry {
					kind: NodeKind::Dir,
					remote_uuid: Some(uuid),
					..synced_shell(dir)
				},
			);
			remote.insert(
				dir.to_string(),
				RemoteNode {
					rel_path: dir.to_string(),
					kind: NodeKind::Dir,
					remote_uuid: uuid,
					stable_uuid: None,
					content_hash: None,
					size: 0,
					modified_millis: 0,
				},
			);
			let file = format!("{dir}/a.txt");
			let uuid = Uuid::new_v4();
			baseline.insert(file.clone(), synced_file(&file, uuid, hash, 7));
			remote.insert(file.clone(), remote_file(&file, uuid, hash, 7));
		}
		// Something the remote gained under the link's name since.
		remote.insert(
			"link/new.txt".to_string(),
			remote_file("link/new.txt", Uuid::new_v4(), other, 11),
		);

		let actions = planned_over_scan(&root, &baseline, &remote);
		assert!(
			actions.is_empty(),
			"no deletion of the link's synced copy, no download through the link: {actions:?}"
		);

		std::fs::remove_dir_all(&root).ok();
	}

	/// Renaming an already-SYNCED file to a name the remote would reject must not read as a local
	/// deletion. Nothing can be pushed under the new name — but the copy the remote already holds
	/// is the user's data, and a rename is not a request to destroy it.
	#[test]
	fn renaming_a_synced_file_into_a_rejected_name_leaves_the_remote_copy_alone() {
		let root = scan_root("rename_file");
		// `report.txt` was synced; the user has just renamed it to the reserved device name `CON`.
		std::fs::write(root.join("CON"), b"payload").unwrap();
		let hash: Blake3Hash = blake3::hash(b"payload").into();
		let uuid = Uuid::new_v4();

		let actions = planned_over_scan(
			&root,
			&HashMap::from([(
				"report.txt".to_string(),
				synced_file("report.txt", uuid, hash, 7),
			)]),
			&HashMap::from([(
				"report.txt".to_string(),
				remote_file("report.txt", uuid, hash, 7),
			)]),
		);
		assert!(
			actions.is_empty(),
			"a rename into a rejected name plans nothing at all — and above all no deletion of the \
			 remote copy: {actions:?}"
		);

		std::fs::remove_dir_all(&root).ok();
	}

	/// The same for a whole synced DIRECTORY: a remote directory deletion is recursive, so reading
	/// the rename as a deletion would take the entire remote subtree with it.
	#[test]
	fn renaming_a_synced_dir_into_a_rejected_name_leaves_its_remote_subtree_alone() {
		let root = scan_root("rename_dir");
		// `docs/` was synced; the user has just renamed it to `bad.` (a trailing dot the remote
		// rejects), carrying its contents along.
		std::fs::create_dir_all(root.join("bad.")).unwrap();
		std::fs::write(root.join("bad.").join("a.txt"), b"payload").unwrap();
		let hash: Blake3Hash = blake3::hash(b"payload").into();
		let (dir_uuid, file_uuid) = (Uuid::new_v4(), Uuid::new_v4());

		let actions = planned_over_scan(
			&root,
			&HashMap::from([
				(
					"docs".to_string(),
					BaselineEntry {
						kind: NodeKind::Dir,
						remote_uuid: Some(dir_uuid),
						..synced_shell("docs")
					},
				),
				(
					"docs/a.txt".to_string(),
					synced_file("docs/a.txt", file_uuid, hash, 7),
				),
			]),
			&HashMap::from([
				(
					"docs".to_string(),
					RemoteNode {
						rel_path: "docs".to_string(),
						kind: NodeKind::Dir,
						remote_uuid: dir_uuid,
						stable_uuid: None,
						content_hash: None,
						size: 0,
						modified_millis: 0,
					},
				),
				(
					"docs/a.txt".to_string(),
					remote_file("docs/a.txt", file_uuid, hash, 7),
				),
			]),
		);
		assert!(
			!actions.iter().any(SyncAction::is_delete),
			"the remote subtree of a locally-renamed directory must survive: {actions:?}"
		);

		std::fs::remove_dir_all(&root).ok();
	}

	/// The streak only blocks once it reaches the threshold. Below the threshold the path is still
	/// planned, so a transient failure is retried rather than parked.
	#[test]
	fn a_failure_streak_blocks_only_at_the_threshold() {
		const NOW: i64 = 1_800_000_000_000;
		for attempts in 0..MAX_PATH_FAILURES {
			assert!(
				!streak_blocks(&failure(attempts, NOW), NOW),
				"{attempts} failure(s) is under the threshold, so the path is still planned"
			);
		}
		assert!(streak_blocks(&failure(MAX_PATH_FAILURES, NOW), NOW));
	}

	/// An exhausted streak does not park its path for good: once its last failure is a retry
	/// interval old the path is planned again, and the failure that attempt records dates the
	/// streak anew, so it waits a whole interval more.
	#[test]
	fn an_exhausted_streak_is_retried_once_its_last_failure_is_an_interval_old() {
		const NOW: i64 = 1_800_000_000_000;
		let interval = PATH_FAILURE_RETRY_INTERVAL.as_millis() as i64;
		let exhausted_at = NOW - interval;

		assert!(
			streak_blocks(&failure(MAX_PATH_FAILURES, exhausted_at), NOW - 1),
			"a millisecond short of the interval, the path is still parked"
		);
		assert!(
			!streak_blocks(&failure(MAX_PATH_FAILURES, exhausted_at), NOW),
			"a whole interval after the last failure, the path is tried again"
		);
		assert!(
			!streak_blocks(&failure(MAX_PATH_FAILURES + 5, exhausted_at - 1), NOW),
			"however long the streak, it is the age of its last failure that releases it"
		);

		// That retry failed: one more attempt, dated now, parks it for another interval.
		let rearmed = failure(MAX_PATH_FAILURES + 1, NOW);
		assert!(streak_blocks(&rearmed, NOW));
		assert!(streak_blocks(&rearmed, NOW + interval - 1));
		assert!(!streak_blocks(&rearmed, NOW + interval));

		assert!(
			streak_blocks(&failure(MAX_PATH_FAILURES, NOW + interval * 10), NOW),
			"a last failure dated after now (the clock stepped back) must not release the path"
		);
	}

	fn failure(attempts: u32, last_failure_at: i64) -> PathFailure {
		PathFailure {
			attempts,
			last_error: "boom".to_string(),
			last_failure_at,
		}
	}

	#[test]
	fn local_roots_overlap_only_when_one_actually_contains_the_other() {
		let existing = Path::new("/sync/data");
		assert!(matches!(
			local_overlap(Path::new("/sync/data"), existing, 1),
			Some(PairOverlap::LocalRootInUse { pair: 1, .. })
		));
		assert!(matches!(
			local_overlap(Path::new("/sync/data/sub/deep"), existing, 1),
			Some(PairOverlap::LocalRootNested { pair: 1, .. })
		));
		assert!(matches!(
			local_overlap(Path::new("/sync"), existing, 1),
			Some(PairOverlap::LocalRootContains { pair: 1, .. })
		));
		// A sibling is disjoint, and so is a name that merely SHARES a prefix with the root: the
		// comparison is per path component, not per byte.
		assert!(local_overlap(Path::new("/sync/other"), existing, 1).is_none());
		assert!(local_overlap(Path::new("/sync/database"), existing, 1).is_none());
		assert!(local_overlap(Path::new("/elsewhere"), existing, 1).is_none());
	}

	/// The same relation between two REMOTE roots, decided from their ancestor chains. The chains
	/// are what a partial cache view used to get wrong: `docs` is an ancestor of `reports` whether
	/// or not anything has enumerated it, so the answer must come from a chain that reaches the
	/// account root and not from what happens to be cached.
	#[test]
	fn remote_roots_overlap_only_when_one_chain_carries_the_other_root() {
		let (root, docs, reports, photos) = (
			Uuid::new_v4(),
			Uuid::new_v4(),
			Uuid::new_v4(),
			Uuid::new_v4(),
		);
		// Each chain is the root itself, then every ancestor up to the account root.
		let docs_chain = [docs, root];
		let reports_chain = [reports, docs, root];
		let photos_chain = [photos, root];

		assert!(matches!(
			remote_overlap(docs, &docs_chain, docs, &docs_chain, 1),
			Some(PairOverlap::RemoteRootInUse { pair: 1, .. })
		));
		assert!(matches!(
			remote_overlap(reports, &reports_chain, docs, &docs_chain, 1),
			Some(PairOverlap::RemoteRootNested { pair: 1, .. })
		));
		assert!(matches!(
			remote_overlap(docs, &docs_chain, reports, &reports_chain, 1),
			Some(PairOverlap::RemoteRootContains { pair: 1, .. })
		));
		// Siblings under one parent are disjoint, and sharing an ancestor is not a relation.
		assert!(remote_overlap(photos, &photos_chain, docs, &docs_chain, 1).is_none());
		assert!(remote_overlap(photos, &photos_chain, reports, &reports_chain, 1).is_none());
	}

	/// The reconciler's own one-line rendering of an action — what these tests assert plans by.
	/// (The public [`PlannedAction`] rendering is pinned in `outcome.rs`.)
	fn describe(action: &SyncAction) -> String {
		action.describe()
	}

	/// A client with no account and no network behind it. [`SyncEngine::open`] only hands it to
	/// the cache to subscribe each pair's remote root, and a subscription the cache refuses leaves
	/// that pair on the grace window instead of failing the open (see
	/// [`SyncEngine::observe_pair`]) — which is exactly what an unconfigured cache answers.
	fn offline_client() -> Arc<Client> {
		let private_key = RsaPrivateKey::new(&mut old_rng::thread_rng(), 512).unwrap();
		let unauthed = UnauthClient::from_config(ClientConfig::default()).unwrap();
		Arc::new(
			unauthed
				.from_stringified(StringifiedClient {
					email: "sync-engine@example.invalid".to_string(),
					user_id: 1,
					root_uuid: Uuid::nil().to_string(),
					auth_info: "0".repeat(64),
					private_key: BASE64_STANDARD
						.encode(private_key.to_pkcs8_der().unwrap().as_bytes()),
					api_key: String::new(),
					auth_version: 2,
					max_parallel_requests: None,
					max_io_memory_usage: None,
				})
				.unwrap(),
		)
	}

	fn created(path: &str, replaced: Option<Uuid>) -> PendingKind {
		PendingKind::Created {
			path: path.to_string(),
			replaced,
		}
	}

	fn moved(from: &str, to: &str) -> PendingKind {
		PendingKind::Moved {
			from: from.to_string(),
			to: to.to_string(),
		}
	}

	/// A directory this engine renamed in place, which the cache still lists under its old
	/// spelling, is shown at the new one — children and all. Leaving the children behind would read
	/// them as untracked remote items beside tracked ones gone missing.
	#[test]
	fn folding_a_directory_rename_carries_its_subtree() {
		let (dir, child) = (Uuid::new_v4(), Uuid::new_v4());
		let node = |path: &str, uuid: Uuid, kind: NodeKind| RemoteNode {
			rel_path: path.to_string(),
			kind,
			remote_uuid: uuid,
			stable_uuid: None,
			content_hash: None,
			size: 0,
			modified_millis: 0,
		};
		let mut nodes = HashMap::from([
			("Docs".to_string(), node("Docs", dir, NodeKind::Dir)),
			(
				"Docs/a.txt".to_string(),
				node("Docs/a.txt", child, NodeKind::File),
			),
		]);
		let mut path_of: HashMap<Uuid, String> = nodes
			.iter()
			.map(|(path, node)| (node.remote_uuid, path.clone()))
			.collect();
		assert!(fold_move(
			&mut nodes,
			&mut path_of,
			&HashMap::new(),
			dir,
			"Docs",
			"docs"
		));
		let mut paths: Vec<&str> = nodes.keys().map(String::as_str).collect();
		paths.sort_unstable();
		assert_eq!(paths, ["docs", "docs/a.txt"]);
		assert_eq!(nodes["docs/a.txt"].rel_path, "docs/a.txt");
		assert_eq!(path_of[&child], "docs/a.txt");
		assert_eq!(path_of[&dir], "docs");
	}

	fn hash(byte: u8) -> Blake3Hash {
		Blake3Hash::from([byte; 32])
	}

	/// A held conflict row for `a.txt` whose two sides carry the same content evidence — what
	/// `record_conflict` writes once both sides have converged while the conflict was held.
	fn converged_conflict(uuid: Uuid) -> BaselineEntry {
		BaselineEntry {
			rel_path: "a.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: Some(uuid),
			content_hash: Some(hash(3)),
			size: Some(5),
			local_mtime: Some(111),
			remote_modified: Some(222),
			state: BaselineState::Conflicted,
			local_kind: Some(NodeKind::File),
			remote_kind: Some(NodeKind::File),
			remote_hash: Some(hash(3)),
			remote_size: Some(5),
			remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
			agreed_hash: None,
		}
	}

	fn local_map(hash: Blake3Hash) -> HashMap<String, LocalNode> {
		HashMap::from([(
			"a.txt".to_string(),
			LocalNode {
				rel_path: "a.txt".to_string(),
				kind: NodeKind::File,
				size: 5,
				mtime_millis: 111,
				content_hash: Some(hash),
			},
		)])
	}

	fn remote_map(uuid: Uuid, hash: Blake3Hash) -> HashMap<String, RemoteNode> {
		HashMap::from([(
			"a.txt".to_string(),
			RemoteNode {
				rel_path: "a.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: uuid,
				stable_uuid: Some(StableUuid::new_for_test(uuid)),
				content_hash: Some(hash),
				size: 5,
				modified_millis: 222,
			},
		)])
	}

	/// Both sides converged while the conflict was held: keeping the local copy has nothing left
	/// to push, so the resolution must record the whole synced state rather than a half row the
	/// next pass reads as a local modification.
	#[test]
	fn keeping_local_on_a_converged_conflict_records_a_clean_synced_row() {
		let uuid = Uuid::new_v4();
		let entry = resolution_entry(
			"a.txt",
			&converged_conflict(uuid),
			ConflictResolution::KeepLocal,
		)
		.expect("the remote side had a kind, so a row is written");

		assert_eq!(entry.state, BaselineState::Synced);
		assert_eq!(
			entry.content_hash,
			Some(hash(3)),
			"the converged content is recorded"
		);
		assert_eq!(entry.size, Some(5));
		assert_eq!(entry.local_mtime, Some(111));
		assert_eq!(entry.remote_uuid, Some(uuid));
		assert_eq!(
			entry.agreed_hash, entry.content_hash,
			"resolving records what the two sides now agree on"
		);

		let baseline = HashMap::from([("a.txt".to_string(), entry)]);
		assert!(
			plan::reconcile(
				SyncMode::TwoWay,
				&baseline,
				&local_map(hash(3)),
				&remote_map(uuid, hash(3)),
				&plan::PassHolds::default(),
			)
			.actions
			.is_empty(),
			"the resolved path is settled: the next pass plans nothing at all"
		);
	}

	/// The mirror image: keeping the REMOTE copy of a converged conflict also has nothing left to
	/// transfer, so it too must record the whole synced state instead of a half row.
	#[test]
	fn keeping_remote_on_a_converged_conflict_records_a_clean_synced_row() {
		let uuid = Uuid::new_v4();
		let entry = resolution_entry(
			"a.txt",
			&converged_conflict(uuid),
			ConflictResolution::KeepRemote,
		)
		.expect("the local side had a kind, so a row is written");

		assert_eq!(entry.state, BaselineState::Synced);
		assert_eq!(
			entry.remote_uuid,
			Some(uuid),
			"the converged remote version is recorded"
		);
		assert_eq!(entry.remote_modified, Some(222));
		assert_eq!(entry.content_hash, Some(hash(3)));
		assert_eq!(
			entry.agreed_hash, entry.content_hash,
			"resolving records what the two sides now agree on"
		);

		let baseline = HashMap::from([("a.txt".to_string(), entry)]);
		assert!(
			plan::reconcile(
				SyncMode::TwoWay,
				&baseline,
				&local_map(hash(3)),
				&remote_map(uuid, hash(3)),
				&plan::PassHolds::default(),
			)
			.actions
			.is_empty(),
			"the resolved path is settled: the next pass plans nothing at all"
		);
	}

	/// And the diverged case still pulls: with the sides genuinely different the row keeps no
	/// remote anchor, so the remote copy reads as the change.
	#[test]
	fn keeping_remote_on_a_diverged_conflict_still_pulls_the_remote_copy() {
		let uuid = Uuid::new_v4();
		let held = BaselineEntry {
			remote_hash: Some(hash(9)),
			..converged_conflict(uuid)
		};
		let entry = resolution_entry("a.txt", &held, ConflictResolution::KeepRemote)
			.expect("the local side had a kind, so a row is written");
		assert_eq!(
			entry.remote_uuid, None,
			"a diverged remote side must still read as changed"
		);
		assert_eq!(
			entry.agreed_hash,
			Some(hash(3)),
			"the resolution settles on the row's content, so the pull below is not re-read as a \
			 concurrent edit and the path does not conflict forever"
		);

		let baseline = HashMap::from([("a.txt".to_string(), entry)]);
		let actions = plan::reconcile(
			SyncMode::TwoWay,
			&baseline,
			&local_map(hash(3)),
			&remote_map(uuid, hash(9)),
			&plan::PassHolds::default(),
		)
		.actions;
		assert_eq!(
			actions.iter().map(describe).collect::<Vec<_>>(),
			vec!["download file \"a.txt\"".to_string()],
		);
	}

	/// The ordinary case is unchanged: the sides really do differ, so the row is anchored to the
	/// remote alone and the local copy is pushed on the next pass.
	#[test]
	fn keeping_local_on_a_diverged_conflict_still_pushes_the_local_copy() {
		let uuid = Uuid::new_v4();
		let held = BaselineEntry {
			remote_hash: Some(hash(9)),
			..converged_conflict(uuid)
		};
		let entry = resolution_entry("a.txt", &held, ConflictResolution::KeepLocal)
			.expect("the remote side had a kind, so a row is written");
		assert_eq!(
			entry.content_hash, None,
			"a diverged local side must still read as changed"
		);
		assert_eq!(
			entry.agreed_hash, None,
			"there is nothing agreed yet: the local copy has still to be pushed"
		);

		let baseline = HashMap::from([("a.txt".to_string(), entry)]);
		let actions = plan::reconcile(
			SyncMode::TwoWay,
			&baseline,
			&local_map(hash(3)),
			&remote_map(uuid, hash(9)),
			&plan::PassHolds::default(),
		)
		.actions;
		assert_eq!(
			actions.iter().map(describe).collect::<Vec<_>>(),
			vec!["upload file \"a.txt\"".to_string()],
		);
	}
	/// A remote node the snapshot holds at `path`.
	fn node_at(path: &str, uuid: Uuid) -> HashMap<String, RemoteNode> {
		HashMap::from([(
			path.to_string(),
			RemoteNode {
				rel_path: path.to_string(),
				kind: NodeKind::File,
				remote_uuid: uuid,
				stable_uuid: Some(StableUuid::new_for_test(uuid)),
				content_hash: None,
				size: 0,
				modified_millis: 0,
			},
		)])
	}

	/// A baseline row as the apply layer writes it straight after a remote write.
	fn written_at(
		path: &str,
		uuid: Uuid,
		kind: NodeKind,
		hash: Option<Blake3Hash>,
	) -> BaselineEntry {
		BaselineEntry {
			rel_path: path.to_string(),
			kind,
			remote_uuid: Some(uuid),
			content_hash: hash,
			size: Some(5),
			local_mtime: Some(111),
			remote_modified: Some(222),
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
			agreed_hash: None,
		}
	}

	/// The baseline an upload of `a.txt` leaves behind.
	fn written_row(uuid: Uuid, hash: Blake3Hash) -> HashMap<String, BaselineEntry> {
		HashMap::from([(
			"a.txt".to_string(),
			written_at("a.txt", uuid, NodeKind::File, Some(hash)),
		)])
	}

	/// The restart this journal exists for: the engine that made the write is gone, and the one
	/// that reopens its baseline DB seconds later has nothing in memory to fold. Loading the
	/// persisted journal is what keeps it from reading the just-uploaded file as absent and
	/// uploading it a second time.
	#[tokio::test]
	async fn a_create_a_previous_engine_journalled_is_folded_after_a_reopen() {
		let path = std::env::temp_dir().join(format!("filen_sync_journal_{}.db", Uuid::new_v4()));
		let uuid = Uuid::new_v4();
		let baseline = written_row(uuid, hash(1));
		let pair = {
			let store = BaselineStore::open(&path).unwrap();
			let (pair, _) = store
				.create_pair("/root", Uuid::new_v4(), SyncMode::LocalToRemote)
				.unwrap();
			store
				.record_pending(
					pair,
					uuid,
					&created("a.txt", None),
					Utc::now().timestamp_millis(),
					&[BaselineChange::Upsert(&baseline["a.txt"])],
				)
				.unwrap();
			pair
			// The engine "exits" here, taking its in-memory journal with it.
		};

		// The restart itself: a real `open` on the same DB, so what is under test is the wiring
		// the reopened engine actually runs — loading the journal and aging it — not a
		// hand-assembled stand-in for it.
		let engine = SyncEngine::open(offline_client(), path.clone())
			.await
			.unwrap();

		// The cache is still behind: its snapshot has nothing at all under the root yet.
		let mut remote = HashMap::new();
		let holds = engine
			.pending
			.settle(pair, &engine.observed.snapshot(), &remote);
		assert_eq!(
			engine.pending.fold_into(pair, &baseline, &mut remote),
			1,
			"the reopened engine folds the write its predecessor made"
		);

		let actions = plan::reconcile(
			SyncMode::LocalToRemote,
			&baseline,
			&local_map(hash(1)),
			&remote,
			&holds,
		)
		.actions;
		assert!(
			actions.is_empty(),
			"the restart re-does the pass's writes: {:?}",
			actions.iter().map(describe).collect::<Vec<_>>()
		);
		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// Watching a paused pair is refused rather than started idle: a handle whose loop runs nothing
	/// looks exactly like a broken watch, and the caller who paused the pair is the one who can say
	/// whether it should be running.
	#[tokio::test]
	async fn a_watch_on_a_paused_pair_is_refused() {
		let path =
			std::env::temp_dir().join(format!("filen_sync_watch_paused_{}.db", Uuid::new_v4()));
		let engine = Arc::new(
			SyncEngine::open(offline_client(), path.clone())
				.await
				.unwrap(),
		);
		let (pair, _) = engine
			.store
			.lock()
			.await
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		engine.pause_pair(pair).await.unwrap();

		let error = match Arc::clone(&engine).watch(pair).await {
			Ok(_) => panic!("a paused pair must not be watchable"),
			Err(error) => error.to_string(),
		};
		assert!(
			error.contains("paused") && error.contains("resume"),
			"the refusal must say what to do about it: {error}"
		);
		// An id nobody registered is still reported as unknown, not as paused.
		let unknown = match Arc::clone(&engine).watch(pair + 9_999).await {
			Ok(_) => panic!("an unknown pair must not be watchable"),
			Err(error) => error.to_string(),
		};
		assert!(unknown.contains("unknown sync pair"), "{unknown}");

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A watch starting on a pair while a removal of that same pair is in flight. Registering the
	/// watch's stop signal after the existence check — with a cache registration and a filesystem
	/// watcher in between — would let the removal land in the gap: it would find no signal to trip,
	/// delete the row, and the watch would then subscribe to a fresh signal nobody will ever trip,
	/// leaving a loop that fails every pass against a pair that is gone and never stops.
	#[tokio::test]
	async fn a_removal_racing_a_watchs_setup_is_never_lost() {
		let path =
			std::env::temp_dir().join(format!("filen_sync_watch_race_{}.db", Uuid::new_v4()));
		let engine = Arc::new(
			SyncEngine::open(offline_client(), path.clone())
				.await
				.unwrap(),
		);
		let (pair, _) = engine
			.store
			.lock()
			.await
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();

		// The store lock fixes the interleaving, as in the pause/removal race above: both verbs
		// queue on it while this guard is held, and tokio hands the mutex out in request order, so
		// the watch's setup runs first and the removal second — the order in which the removal has
		// nothing registered to trip yet.
		let guard = engine.store.lock().await;
		let watching = tokio::spawn({
			let engine = Arc::clone(&engine);
			async move { engine.watchable_pair(pair).await }
		});
		tokio::task::yield_now().await;
		let removing = tokio::spawn({
			let engine = Arc::clone(&engine);
			async move { engine.remove_pair(pair).await }
		});
		tokio::task::yield_now().await;
		drop(guard);

		removing.await.unwrap().unwrap();
		// The watch either never starts (the removal won the lock) or starts holding a signal the
		// removal has already tripped. What must not happen is a live signal for a dead pair.
		if let Ok((_, removed)) = watching.await.unwrap() {
			assert!(
				*removed.borrow(),
				"the watch subscribed after the removal had already passed by: its loop would \
				 retry a pair that no longer exists forever"
			);
		}

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A pause and a removal of the same pair, in flight at once from two callers of one engine. An
	/// id left behind in the paused set describes a pair that no longer exists: it makes
	/// [`SyncEngine::is_paused`] report a phantom, and the set grows for as long as the engine is
	/// open.
	#[tokio::test]
	async fn a_pause_racing_a_removal_leaves_no_paused_id_behind() {
		let path =
			std::env::temp_dir().join(format!("filen_sync_pause_race_{}.db", Uuid::new_v4()));
		let engine = Arc::new(
			SyncEngine::open(offline_client(), path.clone())
				.await
				.unwrap(),
		);
		let (pair, _) = engine
			.store
			.lock()
			.await
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();

		// The store lock fixes the interleaving: both verbs queue on it while this guard is held,
		// and tokio's mutex hands it out in request order, so the pause runs first and the removal
		// second — the order in which a pause that persisted its flag can land after the row it
		// describes is already gone.
		let guard = engine.store.lock().await;
		let pausing = tokio::spawn({
			let engine = Arc::clone(&engine);
			async move { engine.pause_pair(pair).await }
		});
		tokio::task::yield_now().await;
		let removing = tokio::spawn({
			let engine = Arc::clone(&engine);
			async move { engine.remove_pair(pair).await }
		});
		tokio::task::yield_now().await;
		drop(guard);

		// The pause itself may legitimately succeed or be refused, depending on which verb the
		// store lock served first; what must hold either way is that nothing stays paused.
		let _ = pausing.await.unwrap();
		removing.await.unwrap().unwrap();
		assert!(
			!engine.is_paused(pair).await,
			"the removed pair left its id in the paused set; the next pair sqlite gives that id \
			 would never sync"
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A fresh engine on a throwaway baseline DB, with one pair registered.
	async fn engine_with_pair(tag: &str) -> (SyncEngine, PairId, PathBuf) {
		let path = std::env::temp_dir().join(format!("filen_sync_{tag}_{}.db", Uuid::new_v4()));
		let engine = SyncEngine::open(offline_client(), path.clone())
			.await
			.unwrap();
		let (pair, _) = engine
			.store
			.lock()
			.await
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		(engine, pair, path)
	}

	/// `list_conflicts` returns exactly the rows held for resolution — both flavours, with what each
	/// side held — and nothing from a synced row; an unknown pair is an error, not an empty list.
	#[tokio::test]
	async fn list_conflicts_reads_the_held_rows_only() {
		let (engine, pair, path) = engine_with_pair("list_conflicts").await;
		assert!(engine.list_conflicts(pair).await.unwrap().is_empty());
		let hash = Blake3Hash::from([7; 32]);
		{
			let store = engine.store.lock().await;
			store
				.upsert_entry(pair, &synced_file("synced.txt", Uuid::new_v4(), hash, 1))
				.unwrap();
			store
				.upsert_entry(
					pair,
					&BaselineEntry {
						state: BaselineState::Conflicted,
						local_kind: Some(NodeKind::File),
						remote_kind: None,
						..synced_file("b/edited.txt", Uuid::new_v4(), hash, 2)
					},
				)
				.unwrap();
			store
				.upsert_entry(
					pair,
					&BaselineEntry {
						state: BaselineState::Overwritten,
						local_kind: Some(NodeKind::File),
						remote_kind: Some(NodeKind::File),
						..synced_file("a.txt", Uuid::new_v4(), hash, 3)
					},
				)
				.unwrap();
		}
		assert_eq!(
			engine.list_conflicts(pair).await.unwrap(),
			vec![
				PlannedConflict {
					rel_path: "a.txt".to_string(),
					local: Some(PlannedNodeKind::File),
					remote: Some(PlannedNodeKind::File),
				},
				PlannedConflict {
					rel_path: "b/edited.txt".to_string(),
					local: Some(PlannedNodeKind::File),
					remote: None,
				},
			]
		);
		assert!(engine.list_conflicts(pair + 1000).await.is_err());

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A pass that fails outright tells its observer, with the error the call returns. Here the pair
	/// is gone, so the pass fails before it reads either side and that is its only event.
	#[tokio::test]
	async fn a_pass_that_fails_outright_reports_pass_failed() {
		let (engine, pair, path) = engine_with_pair("pass_failed").await;
		engine.remove_pair(pair).await.unwrap();

		let mut events = Vec::new();
		let error = engine
			.sync_once_observed(pair, &mut |event| events.push(event))
			.await
			.unwrap_err();
		assert_eq!(
			events,
			vec![SyncEvent::PassFailed {
				error: error.to_string()
			}]
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A stand-in for the pass in flight, shaped like the action loop in [`apply`]: the checkpoint
	/// before an action, a transfer that never finishes on its own, the row it would record once it
	/// did, and the checkpoint of the action behind it. It resolves to what it got done — which,
	/// once the pair it syncs is removed, must be nothing at all.
	fn pass_in_flight(gate: PassGate) -> tokio::task::JoinHandle<Vec<&'static str>> {
		tokio::spawn(async move {
			let mut done = Vec::new();
			if !gate.wait_to_start().await {
				return done;
			}
			if gate.guard(std::future::pending::<()>()).await.is_some() {
				done.push("the transfer's baseline row");
			}
			if gate.wait_to_start().await {
				done.push("the action behind it");
			}
			done
		})
	}

	/// Removing a pair stops the pass in flight instead of letting it run on: the transfer is
	/// dropped where it stands, it records nothing, the actions queued behind it never start — and
	/// the removal does not return until that has happened, so a completed action is not left
	/// racing the delete of the rows it would write into.
	#[tokio::test]
	async fn removing_a_pair_drops_the_transfer_in_flight_and_waits_for_the_pass() {
		let (engine, pair, path) = engine_with_pair("remove_mid_pass").await;
		let pass = pass_in_flight(engine.pass_gate(pair).await.unwrap());
		tokio::task::yield_now().await;
		assert!(
			!pass.is_finished(),
			"the stand-in pass was supposed to be stuck in its transfer"
		);

		engine.remove_pair(pair).await.unwrap();
		assert!(
			pass.is_finished(),
			"remove_pair returned with the pass it retired the pair under still running"
		);
		assert_eq!(
			pass.await.unwrap(),
			Vec::<&str>::new(),
			"the pass kept working after its pair was removed"
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// The same, for a pass PARKED on the pair's pause: the removal converts that suspension into a
	/// cancel, so the parked action never starts. Nobody is left to resume it — the pair it would
	/// sync is gone.
	#[tokio::test]
	async fn removing_a_paused_pair_never_lets_its_parked_action_start() {
		let (engine, pair, path) = engine_with_pair("remove_parked").await;
		engine.pause_pair(pair).await.unwrap();
		let pass = pass_in_flight(engine.pass_gate(pair).await.unwrap());
		tokio::task::yield_now().await;
		assert!(!pass.is_finished(), "a suspended pass must park, not end");

		engine.remove_pair(pair).await.unwrap();
		assert!(
			pass.is_finished(),
			"remove_pair returned with a pass still parked on the pair it removed"
		);
		assert_eq!(
			pass.await.unwrap(),
			Vec::<&str>::new(),
			"the parked action started after its pair was removed"
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// The two things that cut a pass loose — a watch being stopped (whose engine-side half is
	/// [`SyncEngine::cancel_suspended_pass`], polled by the loop's unparking for as long as it
	/// lives) and the pair being removed — both wait for something, and neither may end up waiting
	/// on the other. Nor may a pause that never escalates park the removal.
	#[tokio::test]
	async fn a_stop_and_a_removal_of_the_same_pair_never_wait_on_each_other() {
		// STOP first: the watch is already gone and has cut the parked pass loose; the removal that
		// follows finds a pass that is cancelled but has not unwound yet.
		let (engine, pair, path) = engine_with_pair("stop_then_remove").await;
		engine.pause_pair(pair).await.unwrap();
		let pass = pass_in_flight(engine.pass_gate(pair).await.unwrap());
		tokio::task::yield_now().await;
		engine.cancel_suspended_pass(pair).await;
		engine.remove_pair(pair).await.unwrap();
		assert!(
			pass.is_finished(),
			"the removal left the stopped pass behind"
		);
		assert_eq!(pass.await.unwrap(), Vec::<&str>::new());
		drop(engine);
		std::fs::remove_file(&path).ok();

		// REMOVE first: the stopped watch keeps unparking a pair the engine has already forgotten,
		// which must neither block nor resurrect the control state the removal took away.
		let (engine, pair, path) = engine_with_pair("remove_then_stop").await;
		engine.pause_pair(pair).await.unwrap();
		let pass = pass_in_flight(engine.pass_gate(pair).await.unwrap());
		tokio::task::yield_now().await;
		engine.remove_pair(pair).await.unwrap();
		engine.cancel_suspended_pass(pair).await;
		assert!(
			pass.is_finished(),
			"the removal did not end the parked pass"
		);
		assert_eq!(pass.await.unwrap(), Vec::<&str>::new());
		assert!(
			!engine.is_paused(pair).await,
			"the removed pair kept its control state"
		);
		drop(engine);
		std::fs::remove_file(&path).ok();

		// PAUSE first, with no escalation window at all: nothing but the removal can end this pass,
		// so a removal that waited for the pause to be reversed would wait for ever.
		let (engine, pair, path) = engine_with_pair("pause_then_remove").await;
		let pass = pass_in_flight(engine.pass_gate(pair).await.unwrap());
		tokio::task::yield_now().await;
		engine
			.pause_pair_with(
				pair,
				PauseOptions {
					mode: PauseMode::Suspend,
					cancel_after: None,
				},
			)
			.await
			.unwrap();
		engine.remove_pair(pair).await.unwrap();
		assert!(
			pass.is_finished(),
			"the removal returned while a pass sat on a pause nothing else can end"
		);
		assert_eq!(pass.await.unwrap(), Vec::<&str>::new());
		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// The window the tests below aim at: the removal has cancelled the pass and is waiting for
	/// it to end, and a control call lands in the middle of that wait. Reports what the pass got
	/// done and how long the removal took.
	async fn a_removal_raced_by<F, Fut>(
		engine: &Arc<SyncEngine>,
		pair: PairId,
		interfere: F,
	) -> (Vec<&'static str>, Duration)
	where
		F: FnOnce() -> Fut,
		Fut: std::future::Future<Output = ()>,
	{
		let gate = engine.pass_gate(pair).await.unwrap();
		let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
		let (release_tx, release_rx) = tokio::sync::oneshot::channel();
		// The pass in flight, with its bookkeeping held up the way a sibling transfer contending for
		// the store lock holds it up: that is the window the removal spends waiting for it.
		let pass = tokio::spawn(async move {
			let mut done = Vec::new();
			if gate.guard(std::future::pending::<()>()).await.is_some() {
				done.push("the transfer's baseline row");
			}
			dropped_tx.send(()).unwrap();
			release_rx.await.unwrap();
			if gate.wait_to_start().await {
				done.push("the action behind it");
			}
			done
		});

		let started = tokio::time::Instant::now();
		let removing = tokio::spawn({
			let engine = Arc::clone(engine);
			async move { engine.remove_pair(pair).await }
		});
		// The transfer was dropped, so the removal has cancelled the pass and is now waiting for it.
		dropped_rx.await.unwrap();
		interfere().await;
		release_tx.send(()).unwrap();

		removing.await.unwrap().unwrap();
		let elapsed = started.elapsed();
		let done = tokio::time::timeout(Duration::from_secs(60), pass)
			.await
			.expect("the removal left the pass parked on a pair that is gone")
			.unwrap();
		(done, elapsed)
	}

	/// A pause landing while the removal waits for the pass it cancelled must not put that pass back
	/// to sleep. Nothing left in the engine could ever wake it again — the persisted flag, the
	/// control channel and the map entry all go with the pair row — so it would park for ever,
	/// holding the drive-write lock every other pair's pass queues on. The caller is told so, rather
	/// than handed an `Ok` for a pause that took on nothing.
	#[tokio::test(start_paused = true)]
	async fn a_pause_landing_while_a_removal_waits_cannot_re_park_the_pass() {
		let (engine, pair, path) = engine_with_pair("pause_during_removal").await;
		let engine = Arc::new(engine);
		let (done, elapsed) = a_removal_raced_by(&engine, pair, || async {
			let refused = engine
				.pause_pair_with(
					pair,
					PauseOptions {
						mode: PauseMode::Suspend,
						// Nothing but the removal can end this pass: a suspension that never
						// escalates.
						cancel_after: None,
					},
				)
				.await
				.expect_err("a pause during a removal must not report success");
			assert!(
				refused.to_string().contains("is being removed"),
				"the refusal must say why: {refused}"
			);
		})
		.await;
		assert_eq!(
			done,
			Vec::<&str>::new(),
			"the pass kept working after its pair was removed"
		);
		// The cancel has to keep the last word THROUGHOUT the wait, not merely be re-sent once the
		// grace elapsed: a pass re-parked for those ten seconds sits on the drive-write lock every
		// other pair queues on, and the removal blocks the caller for as long.
		assert!(
			elapsed < REMOVE_CANCEL_GRACE,
			"the removal took {elapsed:?}: the pause re-parked the pass and only the grace \
			 timeout got it out"
		);
		assert!(
			!engine.is_paused(pair).await,
			"the removed pair kept its control state"
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// The other half: a RESUME landing in the same window must not restart the pass the removal
	/// cancelled. It would apply the rest of its plan — uploads, remote trashes, local deletions —
	/// against a pair whose rows are deleted moments later. It is refused, for the same reason the
	/// pause above is.
	#[tokio::test(start_paused = true)]
	async fn a_resume_landing_while_a_removal_waits_cannot_restart_the_pass() {
		let (engine, pair, path) = engine_with_pair("resume_during_removal").await;
		let engine = Arc::new(engine);
		let (done, elapsed) = a_removal_raced_by(&engine, pair, || async {
			let refused = engine
				.resume_pair(pair)
				.await
				.expect_err("a resume during a removal must not report success");
			assert!(
				refused.to_string().contains("is being removed"),
				"the refusal must say why: {refused}"
			);
		})
		.await;
		assert_eq!(
			done,
			Vec::<&str>::new(),
			"the resume let the cancelled pass carry on against a pair being removed"
		);
		assert!(
			elapsed < REMOVE_CANCEL_GRACE,
			"the removal took {elapsed:?} instead of ending with the pass it cancelled"
		);
		assert!(
			!engine.is_paused(pair).await,
			"the removed pair kept its control state"
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// And the third verb: cancelling the paused actions of a pair whose removal is waiting for the
	/// pass it cancelled. The pair reads as paused — `Retired` is not `Run` — but there is no pause
	/// to convert and no pair to leave paused, so an `Ok` here tells the caller its suspension was
	/// given up when nothing of the kind happened. It is refused exactly as the pause and the resume
	/// above are.
	#[tokio::test(start_paused = true)]
	async fn cancelling_the_paused_actions_while_a_removal_waits_is_refused() {
		let (engine, pair, path) = engine_with_pair("cancel_during_removal").await;
		let engine = Arc::new(engine);
		let (done, elapsed) = a_removal_raced_by(&engine, pair, || async {
			let refused = engine
				.cancel_paused_actions(pair)
				.await
				.expect_err("a cancel during a removal must not report success");
			assert!(
				refused.to_string().contains("is being removed"),
				"the refusal must say why, not read as an ordinary pause: {refused}"
			);
		})
		.await;
		assert_eq!(
			done,
			Vec::<&str>::new(),
			"the pass kept working after its pair was removed"
		);
		assert!(
			elapsed < REMOVE_CANCEL_GRACE,
			"the removal took {elapsed:?} instead of ending with the pass it cancelled"
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A real pass registers its control channel BEFORE it reads either side: a pass that has read
	/// its pair row but holds no gate is one a cancel can neither reach nor wait for, and reading
	/// two trees takes as long as writing them, so that window is not a corner case.
	#[tokio::test]
	async fn a_pass_takes_its_gate_before_it_reads_either_side() {
		let (engine, pair, path) = engine_with_pair("gate_before_read").await;
		// Offline, so the pass fails in its remote enumeration. That it registered a control channel
		// before getting that far is what says the gate came first.
		assert!(
			engine.sync_once(pair).await.is_err(),
			"the offline stand-in was supposed to fail in its remote enumeration"
		);
		assert!(
			engine.paused.lock().await.contains_key(&pair),
			"the pass read the two sides before taking its gate: a cancel landing there would \
			 neither reach it nor wait for it"
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A pass that could not take the drive lock ran none of its plan, so it proves nothing about any
	/// path it planned: every failure streak must come through it untouched. Clearing them would
	/// hand a permanently broken path a fresh set of retries each time the lock is contended.
	#[tokio::test]
	async fn a_pass_that_could_not_take_the_drive_lock_leaves_every_failure_streak_alone() {
		let (engine, pair, path) = engine_with_pair("lock_failure_streaks").await;
		{
			let store = engine.store.lock().await;
			let now = Utc::now().timestamp_millis();
			store
				.record_failure(pair, "broken.txt", "boom", now)
				.unwrap();
			store
				.record_failure(pair, "broken.txt", "boom", now)
				.unwrap();
		}
		let before = engine.store.lock().await.failures(pair).unwrap();

		let attempted = vec!["broken.txt".to_string(), "fine.txt".to_string()];
		let mut report = SyncReport::default();
		let mut events = Vec::new();
		apply::note_lock_failure(
			&mut report,
			attempted.len(),
			&Error::custom(ErrorKind::RetryFailed, "the drive lock is held elsewhere"),
			&mut |event| events.push(event),
		);
		engine
			.note_path_outcomes(pair, &attempted, &mut report)
			.await;

		assert_eq!(
			engine.store.lock().await.failures(pair).unwrap(),
			before,
			"a pass that applied nothing touched a path's failure streak"
		);
		assert_eq!(
			report.interrupted,
			attempted.len(),
			"the pass owes every action it planned"
		);
		assert_eq!(
			events,
			vec![SyncEvent::Interrupted {
				actions: attempted.len()
			}]
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A REAL pass, parked in its read half: the remote enumeration is the first thing that wants
	/// the client's cache slot, so holding that slot stops `sync_once` inside `prepare` — under the
	/// gate it took first, which is the window the two tests below land a control call in.
	fn a_pass_parked_in_its_read(
		engine: &Arc<SyncEngine>,
		pair: PairId,
	) -> tokio::task::JoinHandle<Result<SyncReport, Error>> {
		let engine = Arc::clone(engine);
		tokio::spawn(async move { engine.sync_once(pair).await })
	}

	/// Wait for that pass to reach its gate — it registers the pair's control channel before it
	/// reads either side — and then to park in the read behind it.
	async fn parked(engine: &SyncEngine, pair: PairId) {
		for _ in 0..100 {
			if engine.paused.lock().await.contains_key(&pair) {
				tokio::task::yield_now().await;
				return;
			}
			tokio::task::yield_now().await;
		}
		panic!("the pass never took its gate");
	}

	/// Let a task a control call just woke actually run: on the current-thread test runtime the
	/// caller has to yield for the pass it cancelled to be polled at all.
	async fn ended<T>(pass: &tokio::task::JoinHandle<T>) -> bool {
		for _ in 0..100 {
			if pass.is_finished() {
				return true;
			}
			tokio::task::yield_now().await;
		}
		false
	}

	/// And a removal landing in that window reaches it: the read is dropped where it stands, the
	/// pass ends without planning anything, and the removal waits for it instead of deleting the
	/// rows out from under a pass it never saw.
	#[tokio::test(start_paused = true)]
	async fn a_removal_during_the_read_half_ends_the_pass_before_it_plans() {
		let (engine, pair, path) = engine_with_pair("remove_during_prepare").await;
		let engine = Arc::new(engine);
		let slot = engine.client.cache_slot.lock().await;
		let pass = a_pass_parked_in_its_read(&engine, pair);
		parked(&engine, pair).await;
		assert!(
			!pass.is_finished(),
			"the pass was supposed to be stuck in its read"
		);

		let started = tokio::time::Instant::now();
		engine.remove_pair(pair).await.unwrap();
		let elapsed = started.elapsed();
		assert!(
			pass.is_finished(),
			"remove_pair returned with a pass still reading the pair it removed"
		);
		// Waited for the pass rather than giving up on a read it could not reach.
		assert!(
			elapsed < REMOVE_CANCEL_GRACE,
			"the removal took {elapsed:?}: the cancel never reached the pass in its read"
		);

		// Only now, and there is nothing left to let through: the read was dropped where it stood.
		drop(slot);
		let refused = pass.await.unwrap().expect_err(
			"the read survived the removal, so the pass planned against rows that are gone",
		);
		assert!(
			refused.to_string().contains("is being removed"),
			"a pass dropped by a removal must say so, not report a read that failed: {refused}"
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// The same window, CANCELLED rather than removed: the pair is still there, so the pass reports
	/// the pause instead of an error — the very report a pause landing a moment earlier would have
	/// produced, with nothing planned, applied or recorded.
	#[tokio::test(start_paused = true)]
	async fn a_cancel_during_the_read_half_ends_the_pass_with_the_paused_report() {
		let (engine, pair, path) = engine_with_pair("cancel_during_prepare").await;
		let engine = Arc::new(engine);
		let slot = engine.client.cache_slot.lock().await;
		let pass = a_pass_parked_in_its_read(&engine, pair);
		parked(&engine, pair).await;
		assert!(
			!pass.is_finished(),
			"the pass was supposed to be stuck in its read"
		);

		engine
			.pause_pair_with(
				pair,
				PauseOptions {
					mode: PauseMode::Cancel,
					cancel_after: None,
				},
			)
			.await
			.unwrap();
		assert!(
			ended(&pass).await,
			"the cancel never reached the pass reading the two sides"
		);

		drop(slot);
		let report = pass
			.await
			.unwrap()
			.expect("a cancelled read is not a failure: the pair is still there to be reported on");
		assert_eq!(
			report,
			SyncReport {
				paused: true,
				..SyncReport::default()
			},
			"a pass dropped in its read must report the pause and nothing else"
		);
		assert!(
			engine.store.lock().await.entries(pair).unwrap().is_empty(),
			"a pass that never had a plan wrote a baseline row"
		);
		assert!(
			engine.is_paused(pair).await,
			"the cancel left the pair running"
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A resolution landing while a pass reads the pair waits for that pass: the pass records its
	/// conflict rows from what it read, so a resolution written in between would be put back to
	/// `Conflicted` with nobody told — and, for `KeepBoth`, with the local copy already renamed.
	#[tokio::test(start_paused = true)]
	async fn a_resolution_waits_for_the_pass_reading_the_pair() {
		let (engine, pair, path) = engine_with_pair("resolve_during_prepare").await;
		let engine = Arc::new(engine);
		engine
			.store
			.lock()
			.await
			.upsert_entry(pair, &converged_conflict(Uuid::new_v4()))
			.unwrap();
		let slot = engine.client.cache_slot.lock().await;
		let pass = a_pass_parked_in_its_read(&engine, pair);
		parked(&engine, pair).await;

		let resolving = tokio::spawn({
			let engine = Arc::clone(&engine);
			async move {
				engine
					.resolve_conflict(pair, "a.txt", ConflictResolution::KeepLocal)
					.await
			}
		});
		assert!(
			!ended(&resolving).await,
			"the resolution landed while a pass was still reading the pair it would re-record"
		);
		assert!(
			engine
				.store
				.lock()
				.await
				.entry(pair, "a.txt")
				.unwrap()
				.is_some_and(|row| row.state == BaselineState::Conflicted),
			"the resolution wrote its row under the pass"
		);

		// The offline pass fails in its read once it gets the slot, and lets the resolution through.
		drop(slot);
		assert!(pass.await.unwrap().is_err());
		resolving.await.unwrap().unwrap();
		assert_eq!(
			engine
				.store
				.lock()
				.await
				.entry(pair, "a.txt")
				.unwrap()
				.map(|row| row.state),
			Some(BaselineState::Synced)
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A paused pair holds its conflicts like any other, and the caller must be able to settle them
	/// before resuming: the pause does not make the resolution wait.
	#[tokio::test]
	async fn a_paused_pair_is_still_resolvable() {
		let (engine, pair, path) = engine_with_pair("resolve_paused").await;
		engine
			.pause_pair_with(
				pair,
				PauseOptions {
					mode: PauseMode::Suspend,
					cancel_after: None,
				},
			)
			.await
			.unwrap();
		engine
			.store
			.lock()
			.await
			.upsert_entry(pair, &converged_conflict(Uuid::new_v4()))
			.unwrap();
		engine
			.resolve_conflict(pair, "a.txt", ConflictResolution::KeepLocal)
			.await
			.expect("a paused pair must stay resolvable");

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// And the last control verb: a resolution landing while a removal waits for its pass is refused
	/// with the words the others use, rather than writing a row the removal is about to take away.
	#[tokio::test(start_paused = true)]
	async fn resolving_a_conflict_while_a_removal_waits_is_refused() {
		let (engine, pair, path) = engine_with_pair("resolve_during_removal").await;
		let engine = Arc::new(engine);
		let (done, _) = a_removal_raced_by(&engine, pair, || async {
			let refused = engine
				.resolve_conflict(pair, "a.txt", ConflictResolution::KeepBoth)
				.await
				.expect_err("a resolution during a removal must not report success");
			assert!(
				refused.to_string().contains("is being removed"),
				"the refusal must say why: {refused}"
			);
		})
		.await;
		assert_eq!(done, Vec::<&str>::new());

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// Keeping the remote side of an ordinary conflict moves the losing local edit to the bin. That
	/// edit was never uploaded, so the download that follows would otherwise destroy the only copy.
	/// A local copy that already holds the remote's content has nothing to lose and stays put.
	#[tokio::test]
	async fn keeping_remote_quarantines_a_local_edit_the_remote_never_had() {
		let path =
			std::env::temp_dir().join(format!("filen_sync_keep_remote_{}.db", Uuid::new_v4()));
		let root = std::env::temp_dir().join(format!("filen_sync_keep_remote_{}", Uuid::new_v4()));
		std::fs::create_dir_all(&root).unwrap();
		let engine = SyncEngine::open(offline_client(), path.clone())
			.await
			.unwrap();
		let (pair, _) = engine
			.store
			.lock()
			.await
			.create_pair(root.to_str().unwrap(), Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();

		// Diverged: the local copy's content is not the remote head's.
		std::fs::write(root.join("a.txt"), b"LOCAL").unwrap();
		let diverged = BaselineEntry {
			remote_hash: Some(hash(4)),
			..converged_conflict(Uuid::new_v4())
		};
		engine
			.store
			.lock()
			.await
			.upsert_entry(pair, &diverged)
			.unwrap();
		engine
			.resolve_conflict(pair, "a.txt", ConflictResolution::KeepRemote)
			.await
			.unwrap();
		assert_eq!(
			std::fs::read(root.join(scan::QUARANTINE_DIR).join("a.txt")).unwrap(),
			b"LOCAL",
			"the losing local edit must be recoverable from the bin"
		);
		assert!(!root.join("a.txt").exists());
		assert_eq!(
			engine.store.lock().await.entry(pair, "a.txt").unwrap(),
			None,
			"with the local side moved away, the remote copy must read as a fresh create"
		);

		// Converged: nothing to lose, nothing moved.
		std::fs::write(root.join("b.txt"), b"SAME").unwrap();
		let converged = BaselineEntry {
			rel_path: "b.txt".to_string(),
			..converged_conflict(Uuid::new_v4())
		};
		engine
			.store
			.lock()
			.await
			.upsert_entry(pair, &converged)
			.unwrap();
		engine
			.resolve_conflict(pair, "b.txt", ConflictResolution::KeepRemote)
			.await
			.unwrap();
		assert!(root.join("b.txt").exists());
		assert!(!root.join(scan::QUARANTINE_DIR).join("b.txt").exists());

		drop(engine);
		std::fs::remove_file(&path).ok();
		std::fs::remove_dir_all(&root).ok();
	}

	/// A dry run stays a pure read: it takes NO pass gate, so it neither leaves control state behind
	/// for a pair nobody paused nor makes a removal wait for a call that writes nothing.
	#[tokio::test]
	async fn a_dry_run_takes_no_pass_gate() {
		let (engine, pair, path) = engine_with_pair("plan_no_gate").await;
		// Offline, so the remote enumeration inside fails; the gate would have been taken before
		// that, which is what this is about.
		let _ = engine.plan_pair(pair).await;
		assert!(
			!engine.paused.lock().await.contains_key(&pair),
			"a dry run took a pass gate"
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A control verb refuses the state it cannot act on rather than returning `Ok` for nothing:
	/// cancelling the transfers of a pair nobody paused has no parked pass to unwind, and an `Ok`
	/// there reads as "the transfers are being given up" while they run happily on.
	#[tokio::test]
	async fn cancelling_the_actions_of_a_pair_that_is_not_paused_is_refused() {
		let (engine, pair, path) = engine_with_pair("cancel_not_paused").await;

		let running = engine
			.cancel_paused_actions(pair)
			.await
			.expect_err("a pair nobody paused has no actions to cancel");
		assert!(
			running.to_string().contains("is not paused"),
			"the refusal must say what state the pair is in: {running}"
		);
		let unknown = engine
			.cancel_paused_actions(pair + 9_999)
			.await
			.expect_err("an unknown pair must not be cancellable");
		assert!(
			unknown.to_string().contains("unknown sync pair"),
			"an unknown id must still read as unknown, not as unpaused: {unknown}"
		);

		// Paused, it is exactly what the verb is for — and it leaves the pair paused, so a repeat is
		// idempotent rather than a second refusal.
		engine.pause_pair(pair).await.unwrap();
		engine.cancel_paused_actions(pair).await.unwrap();
		assert!(
			engine.is_paused(pair).await,
			"cancelling the paused actions un-paused the pair"
		);
		engine.cancel_paused_actions(pair).await.unwrap();

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A pass that was still SCANNING when its pair was removed holds no control channel, so the
	/// removal neither reaches it nor waits for it. Asking for its gate afterwards is where it has
	/// to learn the pair is gone — otherwise it is handed a fresh RUNNING gate and applies its whole
	/// plan (uploads, remote trashes, local deletions) against rows the removal has taken away.
	#[tokio::test]
	async fn a_pass_that_asks_for_its_gate_after_the_removal_is_refused() {
		let (engine, pair, path) = engine_with_pair("gate_after_removal").await;
		engine.remove_pair(pair).await.unwrap();

		assert!(
			engine.pass_gate(pair).await.is_err(),
			"the pass got a gate for a pair that no longer exists"
		);
		assert!(
			!engine.paused.lock().await.contains_key(&pair),
			"asking for the gate left a control entry behind for a pair that is gone"
		);

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// The everyday complaint: a second edit within a few seconds of the first. The cache has not
	/// listed our upload yet, but the engine knows exactly what it wrote there — so the pass must
	/// push the new content rather than leave the path alone until the cache agrees.
	#[test]
	fn a_local_edit_at_a_path_whose_create_is_pending_is_pushed() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, created("a.txt", None));

		let baseline = written_row(uuid, hash(1));
		// The cache is behind: its snapshot has nothing at all under the root yet.
		let mut remote = HashMap::new();
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		pending.fold_into(PAIR, &baseline, &mut remote);

		let actions = plan::reconcile(
			SyncMode::LocalToRemote,
			&baseline,
			&local_map(hash(2)),
			&remote,
			&holds,
		)
		.actions;
		assert_eq!(
			actions.iter().map(describe).collect::<Vec<_>>(),
			vec!["upload file \"a.txt\"".to_string()],
			"the edit is pushed against what we wrote, not deferred until the cache catches up"
		);
	}

	/// The everyday case the snapshot alone cannot settle: the file we uploaded is gone again —
	/// trashed by the user, or superseded by another client's re-upload — so its uuid will NEVER
	/// appear in a snapshot. Waiting for it to appear froze the path for the whole grace window
	/// and every pass in between reported an all-zero no-op. The cache announcing the uuid is the
	/// evidence that it has caught up.
	#[test]
	fn a_write_the_cache_has_announced_retires_even_though_the_item_is_gone() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, created("a.txt", None));

		observations.note_uuids([uuid]);
		let observed = observations.snapshot();

		let mut remote = HashMap::new();
		pending.settle(PAIR, &observed, &remote);
		assert_eq!(
			pending.fold_into(PAIR, &written_row(uuid, hash(1)), &mut remote),
			0,
			"an announced uuid retires the write even with nothing left at the path"
		);
		assert!(
			remote.is_empty(),
			"and the snapshot is believed again: the item really is gone"
		);
	}

	/// The race the pre-snapshot copy exists for: an event committed AFTER the snapshot was read
	/// describes a state that snapshot predates. Retiring on it would let this very pass go on to
	/// read the just-written item as a remote-side deletion.
	#[test]
	fn an_announcement_made_after_the_snapshot_was_read_does_not_retire_the_write() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, created("a.txt", None));

		// The pass copies the observations, THEN reads the snapshot; the event lands in between.
		let observed = observations.snapshot();
		observations.note_uuids([uuid]);

		let mut remote = HashMap::new();
		pending.settle(PAIR, &observed, &remote);
		assert_eq!(
			pending.fold_into(PAIR, &written_row(uuid, hash(1)), &mut remote),
			1,
			"only what was known before the snapshot may retire a write against it"
		);
		assert_eq!(remote["a.txt"].remote_uuid, uuid);
	}

	/// A move's uuid was necessarily announced earlier, when the item was created. Only an
	/// announcement made after the move itself proves the cache applied the move.
	#[test]
	fn an_announcement_predating_a_move_does_not_retire_it() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let uuid = Uuid::new_v4();

		observations.note_uuids([uuid]);
		pending.record(&observations, PAIR, uuid, moved("a.txt", "b.txt"));

		let baseline = HashMap::from([(
			"b.txt".to_string(),
			written_at("b.txt", uuid, NodeKind::File, Some(hash(1))),
		)]);
		let mut remote = node_at("a.txt", uuid);
		pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(
			pending.fold_into(PAIR, &baseline, &mut remote),
			1,
			"the snapshot still shows the pre-move path and nothing new has been announced"
		);

		observations.note_uuids([uuid]);
		let mut remote = node_at("a.txt", uuid);
		pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(
			pending.fold_into(PAIR, &baseline, &mut remote),
			0,
			"an announcement made after the move retires it"
		);
	}

	/// The pass moved the file on; the cache still lists it where it was. The view has to show it
	/// where we put it, or the pass reads the move as a remote-side delete plus a local create.
	#[test]
	fn a_pending_move_is_folded_to_its_destination() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, moved("a.txt", "b.txt"));

		let baseline = HashMap::from([(
			"b.txt".to_string(),
			written_at("b.txt", uuid, NodeKind::File, Some(hash(3))),
		)]);
		let mut remote = node_at("a.txt", uuid);
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(pending.fold_into(PAIR, &baseline, &mut remote), 1);

		assert!(
			!remote.contains_key("a.txt"),
			"the pre-move path is vacated"
		);
		assert_eq!(remote["b.txt"].remote_uuid, uuid);
		let local = HashMap::from([(
			"b.txt".to_string(),
			LocalNode {
				rel_path: "b.txt".to_string(),
				kind: NodeKind::File,
				size: 5,
				mtime_millis: 111,
				content_hash: Some(hash(3)),
			},
		)]);
		assert!(
			plan::reconcile(SyncMode::TwoWay, &baseline, &local, &remote, &holds)
				.actions
				.is_empty(),
			"both sides agree once the move is folded in: the pass has nothing to do"
		);
	}

	/// The destination was taken while our move was in flight (another client uploaded over it,
	/// and the cache learned of that before it learned of the move). We cannot claim `b.txt` — but
	/// we know for certain the file is no longer at `a.txt`, and leaving it there hands the
	/// reconciler an untracked remote item to trash: the very file we just moved.
	#[test]
	fn a_move_whose_destination_was_taken_still_vacates_the_pre_move_path() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (uuid, foreign) = (Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, uuid, moved("a.txt", "b.txt"));

		let baseline = HashMap::from([(
			"b.txt".to_string(),
			written_at("b.txt", uuid, NodeKind::File, Some(hash(3))),
		)]);
		let mut remote = node_at("a.txt", uuid);
		remote.extend(node_at("b.txt", foreign));
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(pending.fold_into(PAIR, &baseline, &mut remote), 1);

		assert!(
			!remote.contains_key("a.txt"),
			"the pre-move path is vacated even though the destination is somebody else's"
		);
		assert_eq!(
			remote["b.txt"].remote_uuid, foreign,
			"and the destination is left showing what really sits there"
		);
		let local = HashMap::from([(
			"b.txt".to_string(),
			LocalNode {
				rel_path: "b.txt".to_string(),
				kind: NodeKind::File,
				size: 5,
				mtime_millis: 111,
				content_hash: Some(hash(3)),
			},
		)]);
		let actions =
			plan::reconcile(SyncMode::LocalToRemote, &baseline, &local, &remote, &holds).actions;
		assert!(
			!actions
				.iter()
				.any(|action| describe(action).contains("\"a.txt\"")),
			"the file we moved is not trashed at the path we moved it off: {:?}",
			actions.iter().map(describe).collect::<Vec<_>>()
		);
	}

	/// A trash the cache has not applied reads as an untracked remote item with nothing local —
	/// a deletion to make all over again. Folding takes it out of the view instead, subtree and
	/// all, since the server trashes a directory whole.
	#[test]
	fn a_pending_trash_takes_the_item_out_of_the_view() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (dir, child) = (Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, dir, PendingKind::Trashed);

		let mut remote = HashMap::from([
			(
				"d".to_string(),
				RemoteNode {
					rel_path: "d".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: dir,
					stable_uuid: None,
					content_hash: None,
					size: 0,
					modified_millis: 0,
				},
			),
			(
				"d/x.txt".to_string(),
				RemoteNode {
					rel_path: "d/x.txt".to_string(),
					kind: NodeKind::File,
					remote_uuid: child,
					stable_uuid: Some(StableUuid::new_for_test(child)),
					content_hash: Some(hash(4)),
					size: 5,
					modified_millis: 0,
				},
			),
		]);
		// Trashing dropped both baseline rows, and the local side is gone too.
		let (baseline, local) = (HashMap::new(), HashMap::new());
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(pending.fold_into(PAIR, &baseline, &mut remote), 1);
		assert!(
			remote.is_empty(),
			"the trashed directory takes its subtree with it"
		);
		assert!(
			holds.trashed.contains(&dir),
			"the deletion of that uuid is still suppressed, whatever a view shows"
		);
		assert!(
			plan::reconcile(SyncMode::LocalToRemote, &baseline, &local, &remote, &holds)
				.actions
				.is_empty(),
			"nothing is trashed a second time"
		);
	}

	/// The upload replaced `old` at the path: the cache is simply behind on us, so the view has to
	/// show OUR item there — the one the next edit is measured against.
	#[test]
	fn a_pending_create_is_folded_over_the_uuid_it_replaced() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (old, new) = (Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, new, created("a.txt", Some(old)));

		let baseline = written_row(new, hash(1));
		let mut remote = node_at("a.txt", old);
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(pending.fold_into(PAIR, &baseline, &mut remote), 1);
		assert_eq!(
			remote["a.txt"].remote_uuid, new,
			"the pre-write occupant is what a lagging cache shows; ours supersedes it"
		);
		assert_eq!(remote.len(), 1, "and does not leave the old one behind");

		// A further local edit is then pushed rather than left alone.
		let actions = plan::reconcile(
			SyncMode::LocalToRemote,
			&baseline,
			&local_map(hash(2)),
			&remote,
			&holds,
		)
		.actions;
		assert_eq!(
			actions.iter().map(describe).collect::<Vec<_>>(),
			vec!["upload file \"a.txt\"".to_string()],
		);
	}

	/// Two uploads to one path inside the window: the second's record is retired by the very lag
	/// the fold exists for (the cache still shows the version the FIRST one replaced, which reads
	/// as foreign to it), so the view has to be corrected from the surviving record. Left to the
	/// snapshot, a two-way pass reads both sides as changed and raises a conflict nobody caused.
	#[test]
	fn two_writes_to_one_path_inside_the_window_show_the_latest() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (old, first, second) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, first, created("a.txt", Some(old)));
		pending.record(&observations, PAIR, second, created("a.txt", Some(first)));

		let baseline = written_row(second, hash(2));
		let mut remote = node_at("a.txt", old);
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(pending.fold_into(PAIR, &baseline, &mut remote), 1);
		assert_eq!(
			remote["a.txt"].remote_uuid, second,
			"the latest write is what the path holds"
		);
		assert!(
			plan::reconcile(
				SyncMode::TwoWay,
				&baseline,
				&local_map(hash(2)),
				&remote,
				&holds
			)
			.actions
			.is_empty(),
			"and both sides agree, so there is no conflict to raise"
		);
	}

	/// The other side of the same-round race, on the client that WON it. Its upload is the remote
	/// head, but the cache applied the version that upload superseded and has not announced ours
	/// yet — so the path shows a foreign uuid of our own lineage. Reading that as somebody else's
	/// write left the winner reconciling against the version it had already replaced: a foreign
	/// edit over an unconfirmed push, i.e. a persisted conflict on the client whose copy IS the
	/// head. Our write has to stand in the view instead.
	#[test]
	fn the_version_our_own_upload_superseded_does_not_read_as_a_foreign_write() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (base, ours, theirs) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, ours, created("a.txt", Some(base)));

		// The row our upload left: our bytes under our uuid, the file's lineage, and the shared
		// base still the last content both sides were known to hold.
		let lineage = StableUuid::new_for_test(base);
		let mut baseline = written_row(ours, hash(2));
		let row = baseline.get_mut("a.txt").unwrap();
		row.remote_stable_uuid = Some(lineage);
		row.agreed_hash = Some(hash(0));

		// What the lagging cache shows: the other client's version of the SAME file, which landed
		// first and which our upload went on top of.
		let mut remote = node_at("a.txt", theirs);
		let node = remote.get_mut("a.txt").unwrap();
		node.stable_uuid = Some(lineage);
		node.content_hash = Some(hash(1));

		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(
			pending.fold_into(PAIR, &baseline, &mut remote),
			1,
			"the cache has announced nothing of ours, so what it shows is the version we replaced"
		);
		assert_eq!(remote["a.txt"].remote_uuid, ours);
		assert!(
			plan::reconcile(
				SyncMode::TwoWay,
				&baseline,
				&local_map(hash(2)),
				&remote,
				&holds
			)
			.actions
			.is_empty(),
			"the client holding the head must not conflict with the version it superseded"
		);
	}

	/// The state above is also the ONE the record cannot settle by itself — a version of the same
	/// file that is neither ours nor the one we replaced reads the same whether the cache is
	/// behind on our write or skipped straight past it. That is what the server is asked about;
	/// nothing else is.
	#[test]
	fn only_a_same_lineage_stranger_at_a_pending_creates_path_is_worth_a_lookup() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (base, ours, theirs) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, ours, created("a.txt", Some(base)));
		let lineage = StableUuid::new_for_test(base);
		let mut baseline = written_row(ours, hash(2));
		baseline.get_mut("a.txt").unwrap().remote_stable_uuid = Some(lineage);

		let same_lineage = |uuid: Uuid| {
			let mut nodes = node_at("a.txt", uuid);
			nodes.get_mut("a.txt").unwrap().stable_uuid = Some(lineage);
			nodes
		};
		for settled in [
			// The cache has caught up to our own write.
			same_lineage(ours),
			// It is showing the version our upload superseded.
			same_lineage(base),
			// A different FILE has taken the path over — the fold refuses it outright.
			node_at("a.txt", Uuid::new_v4()),
			// Nothing at the path at all.
			HashMap::new(),
		] {
			assert!(
				pending.strangers(PAIR, &baseline, &settled).is_empty(),
				"the record answers for this state on its own"
			);
		}

		assert_eq!(
			pending.strangers(PAIR, &baseline, &same_lineage(theirs)),
			vec![("a.txt".to_string(), ours, theirs)]
				.into_iter()
				.map(|(path, ours_uuid, stranger)| (ours, path, ours_uuid, stranger))
				.collect::<Vec<_>>()
		);
	}

	/// What the lookup's answer does: with the record retired, the stranger stands and the pass
	/// reconciles against it instead of waiting out the grace window.
	#[test]
	fn retiring_a_superseded_create_lets_the_stranger_stand() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (base, ours, theirs) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, ours, created("a.txt", Some(base)));
		let lineage = StableUuid::new_for_test(base);
		let mut baseline = written_row(ours, hash(2));
		baseline.get_mut("a.txt").unwrap().remote_stable_uuid = Some(lineage);
		let mut remote = node_at("a.txt", theirs);
		remote.get_mut("a.txt").unwrap().stable_uuid = Some(lineage);

		pending.retire(ours);
		assert_eq!(
			pending.fold_into(PAIR, &baseline, &mut remote),
			0,
			"with the record gone there is nothing left to paint over the snapshot"
		);
		assert_eq!(remote["a.txt"].remote_uuid, theirs);
	}

	/// A third uuid at the path is not our write lagging: somebody else wrote there after us, and
	/// the reconciler has to see that immediately rather than wait out the grace window.
	#[test]
	fn a_foreign_uuid_at_the_path_is_never_painted_over() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (old, new) = (Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, new, created("a.txt", Some(old)));

		let foreign = Uuid::new_v4();
		let mut remote = node_at("a.txt", foreign);
		pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(
			pending.fold_into(PAIR, &written_row(new, hash(1)), &mut remote),
			0,
			"a uuid that is neither ours nor the one we replaced is a foreign write"
		);
		assert_eq!(
			remote["a.txt"].remote_uuid, foreign,
			"and it must stand, so the pass reconciles against it at once"
		);
	}

	/// A path held in conflict is re-reported every pass until the caller resolves it, and its
	/// subtree stays suppressed — including when this engine has just written there.
	#[test]
	fn a_conflicted_path_is_re_reported_even_with_a_write_folded_at_it() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, created("d", None));

		let baseline = HashMap::from([(
			"d".to_string(),
			BaselineEntry {
				state: BaselineState::Conflicted,
				..written_at("d", uuid, NodeKind::Dir, None)
			},
		)]);
		// A new local file under the held path: an upload the pass must not attempt while the
		// name itself is unresolved.
		let local = HashMap::from([(
			"d/x.txt".to_string(),
			LocalNode {
				rel_path: "d/x.txt".to_string(),
				kind: NodeKind::File,
				size: 5,
				mtime_millis: 111,
				content_hash: Some(hash(2)),
			},
		)]);

		let mut remote = HashMap::new();
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(pending.fold_into(PAIR, &baseline, &mut remote), 1);
		let actions = plan::reconcile(SyncMode::TwoWay, &baseline, &local, &remote, &holds).actions;
		assert_eq!(
			actions.iter().map(describe).collect::<Vec<_>>(),
			vec!["conflict \"d\"".to_string()],
			"the held conflict is still reported, and nothing under it is acted on"
		);
	}

	/// Each pair enumerates its OWN remote subtree, so another pair's snapshot is silent about a
	/// write it never covers. Reading that silence as evidence retired the record — and a retired
	/// trash is trashed a second time by the owning pair's next pass.
	#[test]
	fn another_pairs_pass_leaves_a_write_it_cannot_see_alone() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (trashed, created) = (Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, trashed, PendingKind::Trashed);
		pending.record(
			&observations,
			PAIR,
			created,
			PendingKind::Created {
				path: "note.txt".to_string(),
				replaced: None,
			},
		);
		// The other pair happens to hold a path of the same name — its own file, under its own
		// root — and never lists the trashed uuid at all.
		let foreign = node_at("note.txt", Uuid::new_v4());

		let holds = pending.settle(PAIR + 1, &observations.snapshot(), &foreign);
		assert!(
			holds.trashed.is_empty(),
			"a pass holds nothing on behalf of another pair"
		);
		assert_eq!(
			pending.fold_into(PAIR + 1, &HashMap::new(), &mut foreign.clone()),
			0,
			"nor does it fold another pair's writes into its own view"
		);

		let mut remote = node_at("gone.txt", trashed);
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(
			holds.trashed,
			HashSet::from([trashed]),
			"the owning pair's snapshot still shows the item untrashed"
		);
		let baseline = HashMap::from([(
			"note.txt".to_string(),
			written_at("note.txt", created, NodeKind::File, Some(hash(1))),
		)]);
		assert_eq!(
			pending.fold_into(PAIR, &baseline, &mut remote),
			2,
			"and still has not caught up to either write"
		);
		assert!(!remote.contains_key("gone.txt"), "the trash is folded out");
		assert_eq!(remote["note.txt"].remote_uuid, created);
	}

	/// The oldest surviving write's stamp bounds what still has to be remembered; with nothing
	/// pending, nothing does.
	#[test]
	fn observations_are_pruned_to_the_oldest_live_write() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		observations.note_uuids([Uuid::new_v4(), Uuid::new_v4()]);

		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, created("a.txt", None));
		observations.prune_before(pending.oldest_stamp());
		assert!(
			observations.snapshot().is_empty(),
			"observations older than every live write are unreachable"
		);

		observations.note_uuids([uuid]);
		pending.settle(PAIR, &observations.snapshot(), &HashMap::new());
		observations.prune_before(pending.oldest_stamp());
		assert!(
			observations.snapshot().is_empty(),
			"with no pending write left there is nothing to remember at all"
		);
	}

	/// Comfortably PAST the confirmation window, and comfortably INSIDE it. Both are expressed in
	/// terms of [`CONFIRM_TENURE`] so retuning the constant cannot leave a fixture on the wrong side
	/// of it while the test still passes for the wrong reason.
	fn past_window() -> Duration {
		CONFIRM_TENURE * 2
	}

	fn inside_window() -> Duration {
		CONFIRM_TENURE / 2
	}

	/// One push of ours, announced `announced_ago` before "now" and superseded (if at all) that
	/// long after the announcement.
	fn tenure(
		announced_ago: Option<Duration>,
		superseded_after: Option<Duration>,
	) -> (PushTenure, Instant) {
		// Built forwards from a real `Instant`, never backwards: subtracting from `Instant::now()`
		// underflows on a machine that has been up for less than the offset.
		let announced = Instant::now();
		let now = announced + announced_ago.unwrap_or_default();
		(
			PushTenure {
				lineage: None,
				recorded: announced,
				announced: announced_ago.map(|_| announced),
				superseded: superseded_after.map(|after| announced + after),
			},
			now,
		)
	}

	/// The pass's drive-lock ceiling earns its place only by staying inside the confirmation window:
	/// a pass that missed another client's release has to poll again, take the lock and upload while
	/// the round it queued behind is still a race by the engine's own measure. Two sleeps' worth of
	/// room is the whole of it — there is no third poll left inside the window.
	#[test]
	fn the_pass_lock_ceiling_polls_again_inside_the_confirmation_window() {
		assert!(
			PASS_LOCK_MAX_SLEEP * 2 <= CONFIRM_TENURE,
			"a missed release costs up to {PASS_LOCK_MAX_SLEEP:?}, which has to leave room inside \
			 CONFIRM_TENURE ({CONFIRM_TENURE:?}) for the round it lets through"
		);
		assert!(
			PASS_LOCK_MAX_SLEEP < MAX_SLEEP_TIME_DEFAULT,
			"the point of the engine's own ceiling is that it is shorter than the shared default \
			 ({MAX_SLEEP_TIME_DEFAULT:?}), which is past the window on its own"
		);
	}

	/// A pass that lets the drive lock go has to stay away from it long enough for a client on the
	/// default ladder to poll once, or the release frees nothing: the pass would take the lock back
	/// before anyone else asked.
	#[test]
	fn a_released_drive_lock_stays_free_past_one_default_poll() {
		assert!(
			PASS_LOCK_YIELD > MAX_SLEEP_TIME_DEFAULT,
			"a pass asking again after {PASS_LOCK_YIELD:?} can beat a client sleeping out \
			 {MAX_SLEEP_TIME_DEFAULT:?} every time"
		);
		assert!(
			PASS_LOCK_MAX_HOLD > PASS_LOCK_YIELD,
			"a pass that stays away longer than it holds the lock spends most of a large push idle"
		);
	}

	#[test]
	fn a_push_that_stood_as_the_head_long_enough_is_confirmed() {
		let (push, now) = tenure(Some(past_window()), None);
		assert_eq!(push.verdict(now), PushVerdict::Confirmed);
	}

	#[test]
	fn a_push_superseded_inside_the_window_is_not_confirmed() {
		let (push, now) = tenure(Some(past_window()), Some(inside_window()));
		assert_eq!(
			push.verdict(now),
			PushVerdict::Unconfirmed,
			"another client was editing at the same time; its version is not an edit made after ours"
		);
	}

	#[test]
	fn a_push_superseded_after_the_window_is_still_confirmed() {
		let (push, now) = tenure(Some(past_window() * 2), Some(past_window()));
		assert_eq!(
			push.verdict(now),
			PushVerdict::Confirmed,
			"our version stood for the full window before anything landed on it"
		);
	}

	#[test]
	fn a_push_that_is_still_ripening_is_not_confirmed_yet() {
		let (push, now) = tenure(Some(inside_window()), None);
		assert_eq!(push.verdict(now), PushVerdict::Unconfirmed);
	}

	#[test]
	fn a_push_the_cache_never_announced_has_no_verdict() {
		let (push, now) = tenure(None, None);
		assert_eq!(
			push.verdict(now),
			PushVerdict::Unknown,
			"with no observation the server's version chain is the only evidence left"
		);
	}

	#[test]
	fn watching_a_push_picks_up_an_announcement_that_beat_it() {
		let observations = Observations::default();
		let uuid = Uuid::new_v4();
		// The cache can commit our own upload's event before the upload call has returned.
		observations.note_uuids([uuid]);
		observations.watch_push(uuid, None, None);
		assert_eq!(
			observations.push_verdict(uuid, Instant::now() + past_window()),
			PushVerdict::Confirmed,
			"the announcement that arrived first still dates the push"
		);
	}

	#[test]
	fn a_push_forgotten_after_a_pass_read_it_has_no_verdict_left() {
		let observations = Observations::default();
		let uuid = Uuid::new_v4();
		observations.note_uuids([uuid]);
		observations.watch_push(uuid, None, None);
		observations.forget_pushes(&[uuid], Instant::now());
		assert_eq!(
			observations.push_verdict(uuid, Instant::now() + past_window()),
			PushVerdict::Unknown
		);
	}

	/// Reading the verdicts is what `plan_pair` does too, and it persists nothing. If the read
	/// retired the records, a dry run inside the confirmation window would leave the pass after it
	/// with an unconfirmed row and no evidence left to confirm it — a false conflict against an
	/// edit made long after ours.
	#[test]
	fn reading_the_announcements_leaves_them_for_the_pass_that_persists() {
		let observations = Observations::default();
		let uuid = Uuid::new_v4();
		observations.note_uuids([uuid]);
		observations.watch_push(uuid, None, None);
		let baseline = written_row(uuid, hash(1));
		let ripe = Instant::now() + past_window();

		let read = || observed_confirmations(&observations, &baseline, &HashMap::new(), ripe).0;
		assert_eq!(read(), std::collections::HashSet::from([uuid]));
		assert_eq!(
			read(),
			std::collections::HashSet::from([uuid]),
			"a dry run that wrote nothing must not have consumed the evidence"
		);

		// The pass that persisted the row is the one that retires it.
		observations.forget_pushes(&[uuid], Instant::now());
		assert!(read().is_empty());
	}

	/// What the version chain cannot say: which versions came out of THIS engine. Two uploads a
	/// second apart carry the same server stamp, so without this the previous one reads as a
	/// stranger's edit that our push buried.
	#[test]
	fn a_version_this_engine_uploaded_is_remembered_as_its_own() {
		let observations = Observations::default();
		let (ours, theirs) = (Uuid::new_v4(), Uuid::new_v4());
		observations.watch_push(ours, None, None);

		assert!(observations.minted(ours));
		assert!(!observations.minted(theirs), "another client's version");

		// Confirming the push retires its tenure record, not the fact that we made it: the pass after
		// it still has to recognise the version as ours.
		observations.forget_pushes(&[ours], Instant::now());
		assert!(observations.minted(ours));

		// Past the window a pass reasons about, it goes.
		observations.forget_pushes(&[], Instant::now() + MINTED_TTL);
		assert!(!observations.minted(ours));
	}

	/// A push is superseded through the lineage index, so the index has to hold exactly the records
	/// being watched: an entry that outlives its record would keep a retired push reachable, and a
	/// record missing from it would never learn it was superseded — a push confirmed by a tenure it
	/// did not have, which pulls another client's concurrent edit over ours without a word.
	#[test]
	fn another_version_of_a_file_we_pushed_supersedes_it_through_the_lineage_index() {
		let mut state = ObservationState::default();
		let lineage = StableUuid::new_for_test(Uuid::new_v4());
		let (ours, replaced) = (Uuid::new_v4(), Uuid::new_v4());
		let now = Instant::now();
		let watched = || PushTenure {
			lineage: Some(lineage),
			recorded: now,
			announced: Some(now),
			superseded: None,
		};
		state.insert_push(replaced, watched());
		state.insert_push(ours, watched());

		// The version our own upload replaced is not watched any more, so nothing about the file
		// speaks for it.
		state.drop_push(replaced);
		state.supersede_lineage(lineage, now + inside_window());
		let ripe = now + past_window();
		assert_eq!(
			state.pushes[&ours].verdict(ripe),
			PushVerdict::Unconfirmed,
			"another client's version landed on ours inside the window"
		);

		state.drop_push(ours);
		assert!(
			state.pushes_by_lineage.is_empty(),
			"the index outlived the records it points at"
		);
	}

	#[test]
	fn a_re_push_of_the_same_path_retires_the_version_it_replaced() {
		let observations = Observations::default();
		let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
		observations.note_uuids([first]);
		observations.watch_push(first, None, None);
		observations.watch_push(second, None, Some(first));
		assert_eq!(
			observations.push_verdict(first, Instant::now() + past_window()),
			PushVerdict::Unknown,
			"the row moved on to the new version, so the old record answers for nothing"
		);
	}
}
