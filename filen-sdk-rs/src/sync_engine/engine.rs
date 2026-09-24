//! The engine orchestration: register pairs, plan a pass (read-only), and run one (plan + apply).
//!
//! `prepare` runs the read-only half — load the baseline, scan the local tree (fast-path),
//! enumerate the remote subtree from the cache, build the remote view — shared by `plan_pair` (a
//! dry run) and `sync_once` (plan + guard + apply + baseline advance).

use std::{
	borrow::Cow,
	collections::{BTreeMap, BTreeSet, HashMap},
	mem,
	path::{Path, PathBuf},
	sync::{Arc, MutexGuard, PoisonError},
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
		BaselineChange, BaselineEntry, BaselineState, BaselineStore, NodeKind, PairId, PairRecord,
		PathFailure, PendingRow,
	},
	changes::{FullPassReason, PairChanges, PassScope, RemoteDeltaEntry},
	derive::{self, Derived},
	facts::{self, PairFacts},
	guard::{self, DeleteGuard, GuardReason},
	ignore::{
		IgnoreDecision, IgnoreLevel, IgnoreSource, IgnoredPath, Origin, RemoteRules,
		RuleCandidates, load_remote_rules, parse_user_ignore, rule_file_dir, rule_file_paths,
	},
	observe::{self, LocalObservation, LocalObservations},
	outcome::{
		PlanOutcome, PlannedAction, PlannedConflict, PlannedNodeKind, RefuseReason, UnsyncablePath,
		UnsyncableReason, planned_action, planned_conflict,
	},
	pause::{PassControl, PassGate, PauseOptions, cancel_suspension},
	plan::{self, RemoteNode, RemoteView, SyncAction},
	remote::{RemoteObserved, cache_ancestry, observe_remote},
	scan::{self, LocalScan, RuleFiles, ScanError},
	side::{Nodes, NodesAt, Side},
	tree::Baseline,
};
use crate::{
	Error, ErrorKind,
	auth::Client,
	cache::{
		CacheEvent, CacheEventType, DirEvent, FileEvent, SearchResult, SyncRootCallback,
		SyncRootHandle,
	},
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
	/// An item this engine sent to the remote trash, and the path it emptied. Its baseline row is
	/// gone, so a snapshot that has not applied the trash yet reads as "present remotely,
	/// untracked, absent locally" — a deletion to make, which would trash it a second time.
	///
	/// `path` is carried for the same reason a move carries `from`: it is where a cache that has
	/// not applied the trash still lists the item, so the fold can find it without indexing the
	/// whole view (see [`ViewIndex`]).
	Trashed { path: String },
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
/// cache batch — the evidence that retires a pending write — and the batch's changes as the pair's
/// remote changelist. It runs on the cache worker thread, so it never awaits and never touches the
/// database.
///
/// The events are borrowed for the length of the call and cannot be read twice, so they are
/// collected into one `Vec` of references (pointers, not payloads) and handed to both records.
fn observation_callback(
	observations: Arc<Observations>,
	changes: Arc<PairChanges>,
) -> SyncRootCallback {
	Box::new(move |events| {
		let batch: Vec<&CacheEvent<'_>> = events.collect();
		changes.note_remote_batch(&mut batch.iter().copied());
		observations.note(&mut batch.iter().copied());
	})
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
	baseline: &Baseline,
	raw_remote: &impl NodesAt<Node = RemoteNode>,
	now: Instant,
) -> (std::collections::HashSet<Uuid>, Vec<(String, Uuid, Uuid)>) {
	let mut confirmed = std::collections::HashSet::new();
	// rel_path is only for the log; the lookup runs on the foreign version sitting at it.
	let mut ask_server = Vec::new();
	for entry in baseline.unconfirmed() {
		let rel_path = &entry.rel_path;
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
				if let Some(node) = raw_remote.at(rel_path)
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

	/// The uuids currently journalled, for diffing what a [`settle`](Self::settle) retired — and for
	/// the probe, which has to show that the record it is timing a fold over is actually there.
	pub(super) fn uuids(&self) -> std::collections::HashSet<Uuid> {
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
		remote: &impl Nodes<Node = RemoteNode>,
	) -> plan::PassHolds {
		let now = Instant::now();
		let mut map = self.map();
		// Only this pair's OWN records consult the snapshot, and indexing every remote node for a
		// pass that journalled none is the whole cost of this call on a large tree. The two rules
		// above the pair check read the record by itself, so they still run — from an empty index —
		// and still retire what the cache has demonstrably caught up to, whoever wrote it.
		let snapshot_path: HashMap<Uuid, Cow<'_, str>> =
			if map.values().any(|write| write.pair == pair) {
				remote
					.iter()
					.map(|(path, node)| (node.remote_uuid, path))
					.collect()
			} else {
				HashMap::new()
			};
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
					.is_none_or(|path| path.as_ref() == from.as_str()),
				PendingKind::Trashed { .. } => snapshot_path.contains_key(uuid),
			}
		});
		plan::PassHolds {
			trashed: map
				.iter()
				.filter(|(_, write)| {
					write.pair == pair && matches!(write.kind, PendingKind::Trashed { .. })
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
		baseline: &Baseline,
		nodes: &impl NodesAt<Node = RemoteNode>,
	) -> Vec<(Uuid, String, Uuid, Uuid)> {
		self.map()
			.iter()
			.filter(|(_, write)| write.pair == pair)
			.filter_map(|(uuid, write)| {
				let PendingKind::Created { path, replaced } = &write.kind else {
					return None;
				};
				let ours = written_node(baseline.get(path).as_ref())?;
				let current = nodes.at(path)?;
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
	///
	/// Nothing about the view is indexed up front: every record names where it expects its item to
	/// be, and [`ViewIndex`] falls back to the whole-view index only for the one record shape that
	/// cannot be answered that way.
	pub(super) fn fold_into(
		&self,
		pair: PairId,
		baseline: &Baseline,
		nodes: &mut Side<RemoteNode>,
		changed: &mut BTreeSet<String>,
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
		let mut path_of = ViewIndex::default();
		let mut folded = 0;
		for (uuid, write) in writes {
			let applied = match &write.kind {
				PendingKind::Created { path, replaced } => fold_create(
					nodes,
					&mut path_of,
					baseline,
					*uuid,
					path,
					*replaced,
					changed,
				),
				PendingKind::Moved { from, to } => {
					fold_move(nodes, &mut path_of, baseline, *uuid, from, to, changed)
				}
				PendingKind::Trashed { path } => {
					fold_trash(nodes, baseline, &mut path_of, *uuid, path, changed)
				}
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
pub(super) fn written_node(entry: Option<&BaselineEntry>) -> Option<RemoteNode> {
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

/// Where the view holds each uuid — built only when a record cannot be answered from the paths it
/// names itself.
///
/// Every record says where it expects its item to be: a move its two ends, a trash the path it
/// emptied, a create its own path (which it looks up directly, and never asks this). That is where
/// a cache merely lagging shows the item, so in the ordinary case this stays empty. Indexing the
/// view up front is one `String` clone and one hash per node of the WHOLE tree, and a pass pays the
/// fold every time the previous one pushed anything.
///
/// Only a foreign edit puts the item on a third path. Then the index is built, once, and kept in
/// step by [`place_node`] and by the removals for the rest of the fold — a scan per record would be
/// quadratic on a large tree right after a large pass.
#[derive(Default)]
struct ViewIndex(Option<HashMap<Uuid, String>>);

impl ViewIndex {
	/// Where `nodes` holds `uuid`, building the index if it is not built yet.
	fn at(&mut self, nodes: &Side<RemoteNode>, baseline: &Baseline, uuid: Uuid) -> Option<String> {
		self.0
			.get_or_insert_with(|| {
				nodes
					.of(baseline)
					.iter()
					.map(|(path, node)| (node.remote_uuid, path.into_owned()))
					.collect()
			})
			.get(&uuid)
			.cloned()
	}

	fn placed(&mut self, uuid: Uuid, path: &str) {
		if let Some(index) = self.0.as_mut() {
			index.insert(uuid, path.to_owned());
		}
	}

	fn gone(&mut self, uuid: Uuid) {
		if let Some(index) = self.0.as_mut() {
			index.remove(&uuid);
		}
	}
}

/// Where the view holds `uuid`, asked of the paths the record names before the view is indexed.
///
/// A hint hits whenever the cache is simply behind on our write or has caught up with it, which is
/// every record but the one a foreign edit has moved out from under. `nodes` is read directly, so a
/// hint is always current however much the fold has already edited.
fn held_at(
	nodes: &Side<RemoteNode>,
	baseline: &Baseline,
	path_of: &mut ViewIndex,
	uuid: Uuid,
	hints: &[&str],
) -> Option<String> {
	for hint in hints {
		if nodes
			.of(baseline)
			.at(hint)
			.is_some_and(|node| node.remote_uuid == uuid)
		{
			return Some((*hint).to_owned());
		}
	}
	path_of.at(nodes, baseline, uuid)
}

/// Put `node` at `path`, keeping the uuid index in step with whatever it displaces.
fn place_node(
	nodes: &mut Side<RemoteNode>,
	baseline: &Baseline,
	path_of: &mut ViewIndex,
	path: String,
	node: RemoteNode,
) {
	let uuid = node.remote_uuid;
	let displaced = nodes.of(baseline).at(&path).map(|node| node.remote_uuid);
	if let Some(previous) = displaced {
		path_of.gone(previous);
	}
	nodes.insert(path.clone(), node);
	path_of.placed(uuid, &path);
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
	nodes: &mut Side<RemoteNode>,
	path_of: &mut ViewIndex,
	baseline: &Baseline,
	uuid: Uuid,
	path: &str,
	replaced: Option<Uuid>,
	changed: &mut BTreeSet<String>,
) -> bool {
	let Some(node) = written_node(baseline.get(path).as_ref()) else {
		return false;
	};
	match nodes.of(baseline).at(path) {
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
	changed.insert(path.to_string());
	place_node(nodes, baseline, path_of, path.to_string(), node);
	true
}

/// Show the item we moved at its destination rather than where the cache still lists it. A
/// directory — see [`plan::fold_dir_moves`] — carries the subtree the cache still lists under its
/// old path along with it.
fn fold_move(
	nodes: &mut Side<RemoteNode>,
	path_of: &mut ViewIndex,
	baseline: &Baseline,
	uuid: Uuid,
	from: &str,
	to: &str,
	changed: &mut BTreeSet<String>,
) -> bool {
	let vacated = match held_at(nodes, baseline, path_of, uuid, &[to, from]).as_deref() {
		// Already where we moved it.
		Some(at) if at == to => return false,
		// The pre-move path the cache still shows: take the node off it, whatever the destination
		// turns out to hold — the one thing this move makes certain is that the item is not here.
		Some(at) if at == from => {
			path_of.gone(uuid);
			changed.insert(from.to_string());
			nodes.remove(baseline, from)
		}
		// Somewhere else entirely — not our move lagging, so the snapshot stands.
		Some(_) => return false,
		None => None,
	};
	// Somebody else's item holds the destination: it superseded ours there while the move was in
	// flight, so leave the path showing what really sits on it. Vacating the pre-move path was
	// still right — without it the reconciler reads the item we moved as an untracked remote entry
	// and trashes it.
	if nodes
		.of(baseline)
		.at(to)
		.is_some_and(|node| node.remote_uuid != uuid)
	{
		return vacated.is_some();
	}
	let carries_subtree = vacated
		.as_ref()
		.is_some_and(|node| node.kind == NodeKind::Dir);
	let node = vacated.or_else(|| {
		written_node(baseline.get(to).as_ref()).filter(|node| node.remote_uuid == uuid)
	});
	let Some(mut node) = node else {
		return false;
	};
	node.rel_path = to.to_string();
	changed.insert(to.to_string());
	place_node(nodes, baseline, path_of, to.to_string(), node);
	if carries_subtree {
		let children = nodes.subtree_paths(baseline, from);
		for old in children {
			let Some(mut child) = nodes.remove(baseline, &old) else {
				continue;
			};
			let new = format!("{to}{}", &old[from.len()..]);
			child.rel_path = new.clone();
			changed.insert(old);
			changed.insert(new.clone());
			place_node(nodes, baseline, path_of, new, child);
		}
	}
	true
}

/// Take the item we trashed out of the view — and, for a directory, everything under it, which the
/// server trashed with it.
fn fold_trash(
	nodes: &mut Side<RemoteNode>,
	baseline: &Baseline,
	path_of: &mut ViewIndex,
	uuid: Uuid,
	at: &str,
	changed: &mut BTreeSet<String>,
) -> bool {
	let Some(path) = held_at(nodes, baseline, path_of, uuid, &[at]) else {
		return false;
	};
	path_of.gone(uuid);
	let Some(node) = nodes.remove(baseline, &path) else {
		return false;
	};
	changed.insert(path.clone());
	if node.kind == NodeKind::Dir {
		// The subtree NAMED rather than filtered out of the whole side: a `retain` asks about
		// every node, which on a carried side is a walk of the tree to trash one directory.
		for key in nodes.subtree_paths(baseline, &path) {
			let Some(child) = nodes.remove(baseline, &key) else {
				continue;
			};
			path_of.gone(child.remote_uuid);
			changed.insert(key);
		}
	}
	true
}

/// One open connection to the baseline DB, shared with the blocking threads its whole-tree work
/// runs on.
///
/// A `std` mutex rather than tokio's, because the two kinds of work this store does want opposite
/// things. The writes a pass makes per action are single statements — 13 µs under this store's
/// pragmas — so a thread hop costs more than the wait it saves, and they take this lock on the
/// caller's thread ([`locked`]). The reads and writes whose cost scales with the tree do not run
/// on a runtime thread at all ([`off_store`]). A `std` guard cannot be held across an `await`, so
/// the compiler is what keeps a long hold from ever landing on a runtime thread.
pub(super) type SharedStore = Arc<std::sync::Mutex<BaselineStore>>;

/// Take a store for work that stays on the caller's thread: a point read, a single-row write.
///
/// A poisoned mutex is recovered rather than propagated. What poisons it is a panic inside
/// rusqlite, which leaves no half-applied transaction behind — every write here is one statement
/// or one explicit transaction — so the connection is still good, and refusing every later pass
/// because one read panicked would be the larger failure.
pub(super) fn locked(store: &SharedStore) -> MutexGuard<'_, BaselineStore> {
	store.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run `work` against `store` on a blocking thread, for everything whose cost scales with the
/// tree: the baseline read, the untrack delete, a subtree move, the conflict read, the failure
/// read.
///
/// [`block_in_place`](tokio::task::block_in_place) would be shorter and is not an option: it
/// panics on a `current_thread` runtime, and only [`watch`](SyncEngine::watch) documents a
/// multi-threaded one.
pub(super) async fn off_store<T, F>(store: &SharedStore, work: F) -> Result<T, Error>
where
	F: FnOnce(&BaselineStore) -> T + Send + 'static,
	T: Send + 'static,
{
	let store = Arc::clone(store);
	tokio::task::spawn_blocking(move || work(&locked(&store)))
		.await
		.map_err(|e| {
			Error::custom(
				ErrorKind::Internal,
				format!("a baseline store task panicked: {e}"),
			)
		})
}

/// A configured sync engine: an `Arc<Client>` (whose cache supplies the remote view) plus one
/// baseline connection per pair.
pub struct SyncEngine {
	pub(super) client: Arc<Client>,
	/// Where the baseline DB lives, so a pair can be given its own connection to it on first use.
	db_path: PathBuf,
	/// The connection the ENGINE-WIDE rows go through: the pair registry, the device-wide user
	/// ignore patterns, and the pending-write journal read back at open. A single pair's journal
	/// rows are written through that pair's own connection instead, because they commit in the
	/// same transaction as the baseline rows they describe.
	///
	/// Every statement it runs is a point read or a single-row write, so waiting for it is never
	/// waiting for a pass.
	control: SharedStore,
	/// One connection per pair, opened on that pair's first use and dropped with the pair. A pass's
	/// whole-tree work holds its own pair's lock and no other, which is what keeps a control verb
	/// on one pair from waiting out a pass on another.
	stores: Mutex<HashMap<PairId, SharedStore>>,
	/// Serializes the registry read-then-act sequences that used to ride on the single store
	/// mutex, and which mean "this pair's registration is not changing under me":
	/// [`pass_gate`](SyncEngine::pass_gate) and [`watchable_pair`](SyncEngine::watchable_pair)
	/// against [`remove_pair`](SyncEngine::remove_pair), and the persisted paused flag against
	/// both. Held for the whole of each such sequence and across no network call, so a pass start
	/// waits at most for another verb's registry work.
	registry: Mutex<()>,
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
	/// Each pair's remote `.filenignore` bodies by remote uuid, as its last pass read them (see
	/// [`load_remote_rules`]). `Arc`: a pass takes the bodies it reuses out from under the lock.
	/// Bounded by the pair count; an entry goes with its pair.
	remote_rule_bodies: Mutex<HashMap<PairId, HashMap<Uuid, Arc<str>>>>,
	/// Each pair's changelists — what its filesystem watcher and the cache have announced since its
	/// last pass — and the reasons its next pass must read both sides whole (see [`PairChanges`]).
	/// `Arc`: the watcher's own thread and the cache's worker thread each hold one, outside this
	/// map's lock. Bounded by the pair count; an entry goes with its pair.
	changes: Mutex<HashMap<PairId, Arc<PairChanges>>>,
	/// What each pair's last pass left for the next one to read instead of deriving it again (see
	/// [`PairCarry`]). Written only by a pass that got as far as a plan, so a dry run can neither
	/// refresh it nor corrupt it. Bounded by the pair count and, within a pair, by the paths it
	/// blocks rather than by the tree.
	carried: Mutex<HashMap<PairId, PairCarry>>,
	/// Bumped once [`set_user_ignore`](SyncEngine::set_user_ignore) has COMMITTED new patterns, so
	/// every running watch loop runs a pass with them instead of waiting out its safety net. A
	/// [`watch`](tokio::sync::watch) channel rather than a [`Notify`](tokio::sync::Notify): every
	/// loop must see the change (a `Notify` wakes one waiter), a burst of setter calls coalesces
	/// into one wake-up, and a version nobody is subscribed to is still a valid send.
	user_ignore_changed: tokio::sync::watch::Sender<u64>,
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

/// What a benchmarked pass did, for the harness that timed it (see [`bench`](super::bench)).
///
/// Deliberately not a [`Prepared`]: a benchmark needs to know WHICH pass ran and what it decided,
/// and handing out the pass's internals would make the harness able to assemble one, which is the
/// drift this seam exists to make impossible.
#[cfg(feature = "bench-internals")]
pub(super) struct BenchPass {
	/// Whether the pass narrowed its read. A benchmark that believed it was timing a change-scoped
	/// pass while the engine fell back to a whole one would report a plausible figure for the wrong
	/// function, which has happened here before.
	pub(super) scoped: bool,
	/// Why it read both sides whole, when it did.
	pub(super) full_reason: Option<FullPassReason>,
	/// What an approved pass would execute — the guard-screened plan, as [`Self::plan_pair`]
	/// reports it.
	pub(super) actions: usize,
	pub(super) rows: usize,
}

/// The seam the benchmark harness drives a REAL pass through.
///
/// Everything here is either a local DB write the harness cannot reach (`self.control` and
/// `self.carried` are private to this module) or a wrapper around [`Self::prepare`]. What it
/// deliberately does NOT offer is a way to assemble a pass out of pieces: the harness can register
/// a pair, seed its rows and its carried state, and then only ASK the engine to run, so a step the
/// engine stops running stops being timed and a step it gains is timed the day it lands.
#[cfg(feature = "bench-internals")]
impl SyncEngine {
	/// Register a pair by writing its registry row, with none of [`Self::add_pair`]'s overlap
	/// checks — those are what need the network.
	pub(super) fn bench_create_pair(
		&self,
		local_root: &str,
		remote_root: Uuid,
		mode: SyncMode,
	) -> Result<PairId, Error> {
		let (pair, _) = locked(&self.control)
			.create_pair(local_root, remote_root, mode)
			.map_err(|e| db_error(e, "registering the benchmark pair"))?;
		Ok(pair)
	}

	/// Seed a converged baseline in ONE transaction, so a million rows is one commit rather than a
	/// million.
	pub(super) async fn bench_seed_rows(
		&self,
		pair: PairId,
		rows: &[BaselineEntry],
	) -> Result<(), Error> {
		let store = self.pair_store(pair).await?;
		let changes: Vec<super::baseline::BaselineChange<'_>> = rows
			.iter()
			.map(super::baseline::BaselineChange::Upsert)
			.collect();
		locked(&store)
			.apply_changes(pair, &changes)
			.map_err(|e| db_error(e, "seeding the benchmark baseline"))
	}

	/// Put the pair in the state one whole pass leaves it in, which is the only state a
	/// change-scoped pass runs from at all: without it every pass returns
	/// [`FullPassReason::FirstPass`].
	pub(super) async fn bench_seed_carry(&self, pair: PairId) {
		self.carried.lock().await.insert(
			pair,
			PairCarry {
				facts: PairFacts::default(),
				remote_converged: true,
				scan_complete: true,
			},
		);
	}

	/// Run a change-scoped pass over `scope`.
	pub(super) async fn bench_prepare(
		&self,
		pair: PairId,
		scope: &mut PassScope,
	) -> Result<BenchPass, Error> {
		self.bench_pass(pair, Some(scope)).await
	}

	/// Run a WHOLE pass — the yardstick every benchmark run records beside its own figure.
	pub(super) async fn bench_prepare_whole(&self, pair: PairId) -> Result<BenchPass, Error> {
		self.bench_pass(pair, None).await
	}

	/// # What this does NOT run
	///
	/// The pass BODY is the engine's own [`Self::prepare`], so nothing there can drift. The tail is
	/// hand-written, and it is shorter than [`Self::sync_once`]'s: this omits `refusal`,
	/// `Prepared::unsyncable`, `Prepared::ignored` and `Prepared::planned` over the held list. On
	/// every scenario in the matrix those are O(scan errors) plus a bool — the held list is empty
	/// except in the mass-delete row, and the safe list never goes through `planned` in a real pass
	/// either — but a scenario that grew a large held list would under-report, so the omission is
	/// stated here rather than left to be discovered.
	async fn bench_pass(
		&self,
		pair: PairId,
		scope: Option<&mut PassScope>,
	) -> Result<BenchPass, Error> {
		let prep = self.prepare(pair, scope).await?;
		super::step("prepare_tail");
		let screened = reconcile_and_screen(&prep, screen_state(&prep));
		super::step("reconcile_and_screen");
		let pass = BenchPass {
			scoped: prep.read.is_scoped(),
			full_reason: prep.read.full_pass_reason(),
			actions: screened.decision.safe.len(),
			rows: prep.baseline.len(),
		};
		// Dropped inside a NAMED step rather than at the end of the function. Freeing what a pass
		// built was once the second-largest term of a change-scoped pass at a million rows, and it
		// sat inside the phase total and inside no step — which is exactly where it hid.
		drop(screened);
		drop(prep);
		super::step("drop_pass");
		Ok(pass)
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
#[non_exhaustive]
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

/// What a pass read, and why.
///
/// A pass either reads both trees WHOLE — the only read that can find a change nothing announced —
/// or narrows itself to the paths its changelists named and derives the rest from the baseline
/// rows (the optimization plan's section 3.4). Which it was is reported on
/// [`SyncReport::full_pass`], so a caller can see why a pass cost what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PassRead {
	/// Both sides whole, for this reason. `None` is a dry run, which is always whole and reports
	/// nothing (see [`SyncEngine::plan_pair`]).
	Whole(Option<FullPassReason>),
	/// Only what changed since the last pass — carrying the paths this pass DECIDES: every path
	/// whose two nodes are not the ones its baseline row carries (see [`plan::PassPaths`]).
	///
	/// One value for both because they are one question. A pass that read everything knows
	/// everything and decides everything; a pass that read what changed can only decide what it
	/// read, and the set is the evidence it has.
	Scoped(BTreeSet<String>),
}

impl PassRead {
	/// Why this pass read everything, for its report: `None` when it narrowed itself down.
	fn full_pass_reason(&self) -> Option<FullPassReason> {
		match self {
			Self::Whole(reason) => *reason,
			Self::Scoped(_) => None,
		}
	}

	fn is_scoped(&self) -> bool {
		matches!(self, Self::Scoped(_))
	}

	/// Which paths this pass's reconcile decides (see [`plan::PassPaths`]).
	fn paths(&self) -> plan::PassPaths<'_> {
		match self {
			Self::Whole(_) => plan::PassPaths::Whole,
			Self::Scoped(decided) => plan::PassPaths::Changed(decided),
		}
	}
}

/// Whether a pass with nothing announced on either side is worth running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WhenIdle {
	/// Run it anyway: an explicit [`sync_once`](SyncEngine::sync_once) asked for a pass, and its
	/// caller wants the report — including one that had a watcher running and beat its event.
	Run,
	/// Skip it entirely. A watch wake with nothing announced has nothing to do, and reading two
	/// trees to discover that is the whole cost change-scoping exists to remove.
	Skip,
}

/// The per-pair reads every pass makes before it looks at either tree.
struct PassInputs {
	record: PairRecord,
	user_ignore: String,
	store: SharedStore,
	baseline: Arc<Baseline>,
	/// The failure streaks that BLOCK planning this pass (see [`streak_blocks`]).
	failures: HashMap<String, PathFailure>,
	last_ignored: BTreeSet<String>,
}

/// What [`SyncEngine::prepare_scoped`] came back with.
enum Scoped {
	/// The pass narrowed its read and has everything it needs to plan.
	Prepared(Box<Prepared>),
	/// The derivation could not answer for something it was handed, so the inputs come back for a
	/// whole read. Never a partial map: a map missing a path reads downstream as a deletion, which
	/// is the one thing invariant I1 forbids.
	Whole(Box<PassInputs>, FullPassReason),
}

/// What a pass leaves the next one, so a change-scoped pass need not derive it again.
///
/// Written only by [`run_pass`](SyncEngine::run_pass), and only by a pass that got as far as a
/// plan: a dry run reads both sides and records nothing, so it can neither refresh this nor
/// corrupt what the next pass believes.
#[derive(Debug, Default, Clone)]
struct PairCarry {
	/// Everything that pass blocked, reported or withheld, keyed by path.
	facts: PairFacts,
	/// Whether the remote view had converged when it was last READ whole. A change-scoped pass
	/// reads no snapshot and so has no watermark of its own; this is the judgement the guard is
	/// still working from (plan 3.4).
	remote_converged: bool,
	/// Whether that read's local walk was complete — the other half of the same rule.
	scan_complete: bool,
}

/// Whether the local map a change-scoped pass assembled accounts for the rows and observations it
/// was built from.
///
/// The map is the baseline's rows, minus the rows each observation replaces, plus the nodes that
/// observation found. Both ends are computable from the pieces themselves:
///
/// - a `Dir` walk that came back COMPLETE replaces the rows under its key, except those under a
///   path it pruned and never looked at ([`LocalObservation::uncovered_roots`]); an incomplete one
///   replaces nothing,
/// - an `Absent` replaces every row at or under the path whose `stat` said so, and so does a
///   `Hidden`: the rules hide that subtree, so `merge_local` takes those rows out of the map too
///   — held rather than deleted, which is a fact about the plan and not about this count,
/// - a `File` replaces nothing — it lands on a row that is still carried,
///
/// so the assembled size has to land between "every replaceable row went" and "every one of them
/// was replaced by an observed node". Outside those bounds the assembly dropped paths that no
/// observation asked it to drop — risk #1 of the plan, and the shape that fabricates an absence —
/// so the pass reads both sides instead of planning from it.
pub(super) fn assembly_accounted(
	baseline: &Baseline,
	derived: &Derived,
	observed: &LocalObservations,
	held: &BTreeSet<String>,
) -> bool {
	// Rows the observations replace, and the nodes they put back. A COMPLETE walk's contribution is
	// exact — every row under its key goes, every node it found arrives, and the two sets cannot
	// overlap, since a node it found is not a row it pruned. The slack is the observations that may
	// land ON a row that is still carried: a file (whose row is usually still there) and an
	// incomplete walk (which replaced nothing).
	let mut replaced = 0usize;
	let mut certain = 0usize;
	let mut slack = 0usize;
	for (at, observation) in &observed.observed {
		let pruned: BTreeSet<String> = observation.uncovered_roots().cloned().collect();
		// Rows the derivation CARRIED, and no others: a held row was never in the map to begin
		// with, so counting it as one an observation replaced subtracts it a second time — `carried`
		// below has already left it out — and puts the bound below the map the pass legitimately
		// assembled.
		let rows_at = |at: &str| {
			let mut rows = usize::from(
				baseline.contains_key(at) && !pruned.contains(at) && !held.contains(at),
			);
			baseline.visit_subtree_paths(at, |path| {
				rows += usize::from(!plan::at_or_under_root(&pruned, path) && !held.contains(path));
			});
			rows
		};
		match observation {
			LocalObservation::Dir(scan) if scan.complete => {
				replaced += rows_at(at);
				certain += scan.nodes.of(baseline).len();
			}
			LocalObservation::Dir(scan) => slack += scan.nodes.of(baseline).len(),
			LocalObservation::Absent(absence) => replaced += rows_at(absence.path()),
			LocalObservation::Hidden(_) => replaced += rows_at(at),
			LocalObservation::File { .. } => slack += 1,
		}
	}
	// A row that records one side only is in neither map and is held instead (`derive::carried`),
	// which is the one other way a row legitimately misses the map. Taken by the caller, which holds
	// the set before the pass's view takes it — and subtracted HERE only, never again inside
	// `rows_at`.
	let carried = baseline.len().saturating_sub(held.len());
	let lowest = carried.saturating_sub(replaced).saturating_add(certain);
	(lowest..=lowest.saturating_add(slack)).contains(&derived.local.of(baseline).len())
}

/// The first key in the two maps a narrowed reconcile would never visit, if there is one — the
/// decided set's half of plan 3.6's self-check.
///
/// [`derive::from_baseline`] only ever inserts at a baseline row's path, so a key in either map
/// that is NOT a row path was put there by a producer — the local observation, the remote delta,
/// the fold of this engine's own unacknowledged writes — and every one of them is supposed to
/// record what it moved in [`Derived::decided`]. A key that got past all three is a key
/// [`PassPaths::Changed`](plan::PassPaths::Changed) never visits: nothing is planned at it,
/// whatever the three inputs say there.
///
/// One pass over the two maps, and the answer is the path itself so the assertion can name it.
///
/// Two mistakes are OUT of its reach, and the claim is only ever about keys no row names. A key a
/// producer REMOVED without recording does not need to be seen: a path in neither map is decided by
/// nobody, which delays an action and invents none (invariant I1's safe direction). A producer that
/// REPLACED the node at a row's own path without recording it — a remote overwrite at a tracked
/// path, the pending-write fold landing on a row — is invisible, because that key IS a row path and
/// this cannot tell it from the node the row carried. Nothing here bounds that class; the producers'
/// own funnels are the only record of it.
///
/// One `Baseline` path resolve per key a PRODUCER placed, which is what makes this affordable in
/// every build: the two sides hold what this pass read, not the tree ([`Side::own_keys`]).
pub(super) fn unaccounted_key(
	baseline: &Baseline,
	local: &Side<scan::LocalNode>,
	remote: &Side<RemoteNode>,
	decided: &BTreeSet<String>,
) -> Option<String> {
	local
		.own_keys()
		.chain(remote.own_keys())
		.find(|path| !decided.contains(*path) && !baseline.contains_key(path))
		.map(str::to_owned)
}

/// The read-only inputs to a pass, shared by planning and applying.
struct Prepared {
	record: PairRecord,
	baseline: Arc<Baseline>,
	/// What this pass read, and why (see [`PassRead`]).
	read: PassRead,
	/// The announced remote changes this pass CONSUMED — kept so a pass cut short can hand them
	/// back to the next one (see [`next_pass_scope`] and `changes`'s module docs).
	///
	/// Empty on a whole read: its remote evidence was the snapshot, which is in no changelist to
	/// begin with and cannot be handed anywhere.
	remote_delta: Vec<RemoteDeltaEntry>,
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
	/// Everything this pass blocks, reports or withholds, keyed by path: what a whole read found,
	/// or — on a change-scoped pass — what the last read found, pruned and re-answered wherever
	/// this one looked (see [`PairFacts`]).
	facts: PairFacts,
	/// Directories whose REMOTE rule file could not be used THIS pass.
	///
	/// Kept out of [`PairFacts::ignore_blocked`], which holds only what a local walk found: the
	/// remote half is recomputed every pass from the rule files the baseline names, so carrying it
	/// would leave a stale block with nothing left to prune it.
	remote_blocked: BTreeSet<String>,
	/// A report line per remote `.filenignore` that could not be used, or per bad line in one.
	remote_rule_errors: Vec<String>,
	/// The ignored roots the pair's previous pass recorded (see
	/// [`ignored_roots_to_record`](Self::ignored_roots_to_record)).
	last_ignored: BTreeSet<String>,
	/// Baseline rows whose agreed-content marker this pass's raw snapshot advanced (see
	/// [`plan::confirm_agreed_content`]). Already applied to `baseline`, so planning reads them
	/// either way; a real pass persists them, a dry run writes nothing.
	confirmed: Vec<BaselineEntry>,
}

impl Prepared {
	/// Carry each directory move across as one move — a case-only rename included — and read the
	/// rest of the pass where those subtrees end up (see [`plan::fold_dir_moves`]).
	fn fold_dir_moves(&mut self) {
		self.dir_moves = plan::fold_dir_moves(
			self.record.mode,
			&mut self.baseline,
			&mut self.local_scan.nodes,
			&mut self.remote_view.nodes,
			&self.holds.held_remote,
			self.read.paths(),
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
			// Keyed like the maps and the rows the fold just moved — so it moves with them, or the
			// next pass decides a path nothing is keyed by any more.
			if let PassRead::Scoped(decided) = &mut self.read {
				*decided = mem::take(decided)
					.into_iter()
					.map(|path| plan::moved_path(&path, from, to).unwrap_or(path))
					.collect();
			}
			rekey_paths(&mut self.facts.unknown_remote, from, to);
			self.failures.remove(from);
			rekey_paths(&mut self.failures, from, to);
			for blocked in [&mut self.facts.ignore_blocked, &mut self.remote_blocked] {
				*blocked = mem::take(blocked)
					.into_iter()
					.map(|path| plan::moved_path(&path, from, to).unwrap_or(path))
					.collect();
			}
			if matches!(action, SyncAction::MoveRemote { .. }) {
				// Keyed like the remote view, which only a remote move re-keys.
				for report in &mut self.facts.never_synced_remote {
					if let Some(path) = plan::moved_path(&report.rel_path, from, to) {
						report.rel_path = path;
					}
				}
				rekey_ignored(&mut self.facts.ignored_remote, from, to);
			} else {
				// Keyed like the local scan, which only a local move re-keys.
				rekey_ignored(&mut self.facts.ignored_local, from, to);
				rekey_paths(&mut self.facts.invalid_names, from, to);
				rekey_paths(&mut self.facts.aliased_dirs, from, to);
				for target in self.facts.aliased_dirs.values_mut() {
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
			.map(|action| {
				planned_action(
					action,
					&self.local_scan.nodes.of(&self.baseline),
					&self.remote_view.nodes.of(&self.baseline),
				)
			})
			.collect()
	}

	/// Every path this pass will not act on, and why — reported identically by the dry run and by
	/// the pass itself, so a caller sees the same list either way.
	fn unsyncable(&self) -> Vec<UnsyncablePath> {
		let names = self.facts.invalid_names.iter().map(|(rel_path, detail)| {
			UnsyncablePath::new(
				rel_path.clone(),
				UnsyncableReason::InvalidName {
					detail: detail.clone(),
				},
			)
		});
		let mut all: Vec<UnsyncablePath> =
			names
				.chain(self.failures.iter().map(|(rel_path, failure)| {
					UnsyncablePath::new(
						rel_path.clone(),
						UnsyncableReason::RepeatedFailure {
							attempts: failure.attempts,
							last_error: failure.last_error.clone(),
						},
					)
				}))
				.chain(self.facts.unknown_remote.iter().map(|(rel_path, reason)| {
					UnsyncablePath::new(rel_path.clone(), reason.clone())
				}))
				.chain(self.facts.never_synced_remote.iter().cloned())
				.chain(self.facts.aliased_dirs.iter().map(|(rel_path, target)| {
					UnsyncablePath::new(
						rel_path.clone(),
						UnsyncableReason::LocalAlias {
							target: target.clone(),
						},
					)
				}))
				.collect();
		// One stable order, so a caller diffing consecutive reports sees only real changes.
		all.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
		all
	}

	/// Every path this pass must not plan an action for: a name the remote would reject, a local
	/// symlink to a directory inside the root, a path whose failure streak ran out, and a synced path
	/// whose remote item is out of the view, all reported by [`unsyncable`](Self::unsyncable); and an
	/// ignored path, or one whose ignore rules could not be read.
	fn blocked_paths(&self) -> BTreeSet<String> {
		self.failures
			.keys()
			.chain(self.facts.invalid_names.keys())
			.chain(self.facts.aliased_dirs.keys())
			.chain(self.facts.unknown_remote.keys())
			.chain(self.facts.ignored_local.keys())
			.chain(self.facts.ignored_remote.keys())
			.chain(&self.facts.ignore_blocked)
			.chain(&self.remote_blocked)
			.cloned()
			.collect()
	}

	/// Every directory whose ignore rules this pass could not use, from either side (see
	/// [`remote_blocked`](Self::remote_blocked)).
	fn blocked_rules(&self) -> BTreeSet<String> {
		self.facts
			.ignore_blocked
			.iter()
			.chain(&self.remote_blocked)
			.cloned()
			.collect()
	}

	/// The top-most ignored paths on either side: blocked, and untracked once the pass is done.
	fn ignored_roots(&self) -> BTreeSet<String> {
		self.facts
			.ignored_local
			.keys()
			.chain(self.facts.ignored_remote.keys())
			.cloned()
			.collect()
	}

	/// Whether `path` is at or under a root this pass ignores, or under rules it could not read.
	fn hides(&self, path: &str) -> bool {
		self.facts.ignore_blocked.contains("")
			|| self.remote_blocked.contains("")
			|| path
				.match_indices('/')
				.map(|(i, _)| &path[..i])
				.chain([path])
				.any(|at| {
					self.facts.ignored_local.contains_key(at)
						|| self.facts.ignored_remote.contains_key(at)
						|| self.facts.ignore_blocked.contains(at)
						|| self.remote_blocked.contains(at)
				})
	}

	/// The roots the pair's previous pass ignored that nothing hides any more, neither a rule nor
	/// rules this pass could not read. What sits there has no row (the pass that ignored it dropped
	/// them), so it syncs like a first sync.
	fn unignored(&self) -> BTreeSet<&str> {
		self.last_ignored
			.iter()
			.map(String::as_str)
			.filter(|root| !self.hides(root))
			.collect()
	}

	/// The ignored roots to record for the pair's next pass: every root this pass ignores; a recorded
	/// root this pass still hides, which the next pass has to see un-ignored when that ends; and an
	/// un-ignored root while a deletion at or under it is `held`, so the next pass holds it again.
	fn ignored_roots_to_record<'a>(
		&self,
		held: impl IntoIterator<Item = &'a str>,
	) -> BTreeSet<String> {
		let unignored = self.unignored();
		let held: BTreeSet<String> = held.into_iter().map(str::to_owned).collect();
		let mut roots = self.ignored_roots();
		roots.extend(
			self.last_ignored
				.iter()
				.filter(|root| {
					!unignored.contains(root.as_str())
						|| held.contains(root.as_str())
						|| held
							.range(plan::subtree_bounds(root.as_str()))
							.next()
							.is_some()
				})
				.cloned(),
		);
		roots
	}

	/// The ignored paths to report, shared by the dry run and the pass: the top of each ignored
	/// subtree on either side, once, in path order. A path both sides hide carries the local scan's
	/// rule, level and deciding line together. One only the defaults hide is noise (a `.DS_Store` in
	/// every folder) and is left out, unless a baseline row sits at or under it, which this pass
	/// drops.
	fn ignored(&self) -> Vec<IgnoredPath> {
		fn parents(path: &str) -> impl Iterator<Item = &str> {
			path.match_indices('/').map(|(i, _)| &path[..i])
		}
		let mut roots: BTreeMap<&str, (&IgnoreDecision, bool)> = BTreeMap::new();
		for (path, decision) in self
			.facts
			.ignored_local
			.iter()
			.chain(&self.facts.ignored_remote)
		{
			roots.entry(path).or_insert((decision, false));
		}
		if roots.is_empty() {
			return Vec::new();
		}
		// Each side records only its own top-most paths; one side's can still sit under the other's.
		let nested: Vec<&str> = roots
			.keys()
			.filter(|path| parents(path).any(|parent| roots.contains_key(parent)))
			.copied()
			.collect();
		for path in nested {
			roots.remove(path);
		}
		// One subtree probe per ignored root — the roots are a handful and the rows are the whole
		// tree, so asking the baseline about each root beats walking every row and its ancestors.
		for (path, (_, tracked)) in &mut roots {
			*tracked = self.baseline.tracked(path, true);
		}
		roots
			.into_iter()
			.filter(|(_, (decision, tracked))| *tracked || decision.level != IgnoreLevel::Default)
			.map(|(path, (decision, tracked))| {
				IgnoredPath::new(
					path,
					decision.level.clone(),
					decision.pattern.clone(),
					tracked,
				)
			})
			.collect()
	}
}

/// [`rekey_paths`] for ignored roots, carrying the directory of a `.filenignore` that decided one
/// along with it.
fn rekey_ignored(map: &mut BTreeMap<String, IgnoreDecision>, from: &str, to: &str) {
	rekey_paths(map, from, to);
	for decision in map.values_mut() {
		if let IgnoreLevel::File { dir } = &mut decision.level
			&& let Some(moved) = plan::moved_path(dir, from, to)
		{
			*dir = moved;
		}
	}
}

/// Whether the remote view is wholly empty while the baseline still tracks remote items: what a
/// transient backend or cache fault looks like. Read from the view BEFORE ignored items are filtered
/// out of it, or a root `.filenignore` of `*` would read as a vanished remote on every pass.
fn remote_emptied(nodes: &impl Nodes<Node = RemoteNode>, baseline: &Baseline) -> bool {
	nodes.is_empty() && baseline.has_remote_rows()
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
		let store = {
			let db_path = db_path.clone();
			tokio::task::spawn_blocking(move || BaselineStore::open(&db_path))
				.await
				.map_err(|e| {
					Error::custom(ErrorKind::Internal, format!("baseline open panicked: {e}"))
				})??
		};
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
			db_path,
			control: Arc::new(std::sync::Mutex::new(store)),
			stores: Mutex::new(HashMap::new()),
			registry: Mutex::new(()),
			pending: PendingWrites::default(),
			observed: Arc::new(Observations::default()),
			roots: Mutex::new(HashMap::new()),
			approvals: Mutex::new(HashMap::new()),
			registrations: Mutex::new(()),
			paused: Mutex::new(paused),
			removals: Mutex::new(HashMap::new()),
			lock_budget: LockBudget::default(),
			reading: Mutex::new(HashMap::new()),
			remote_rule_bodies: Mutex::new(HashMap::new()),
			changes: Mutex::new(HashMap::new()),
			carried: Mutex::new(HashMap::new()),
			user_ignore_changed: tokio::sync::watch::channel(0).0,
		};
		// Pairs registered by an earlier session are live again from here on, so they need their
		// cache subscription back too.
		for record in engine.list_pairs().await? {
			// Nothing was announced to THIS process yet, so its first pass has no changelist to
			// narrow itself with.
			engine
				.pair_changes(record.id)
				.await
				.force(FullPassReason::FirstPass);
			engine.observe_pair(record.id, record.remote_root).await;
		}
		// An engine that went down inside the cache-lag window left the writes it had made in the
		// journal; folding them is what stops this one from making them a second time.
		let now = Utc::now().timestamp_millis();
		let rows = off_store(&engine.control, move |store| {
			store.load_pending(now, grace_millis())
		})
		.await?
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
		let changes = self.pair_changes(pair).await;
		let callback = observation_callback(Arc::clone(&self.observed), Arc::clone(&changes));
		match Arc::clone(&self.client)
			.add_sync_root(remote_root, callback)
			.await
		{
			Ok(handle) => {
				self.roots.lock().await.insert(pair, handle);
			}
			Err(error) => {
				// Without the subscription no remote change is announced at all, so no pass can
				// narrow its remote half down: every one reads the whole subtree, as they all do
				// today.
				changes.degrade(FullPassReason::RemoteEventsDegraded);
				tracing::warn!(
					"sync pair {pair}: cache notifications are unavailable ({error}); its pending writes will retire on the grace window alone"
				);
			}
		}
	}

	/// `pair`'s changelists, created on first use — shared with the pair's cache callback and, while
	/// it is watched, with its filesystem watcher's handler.
	pub(super) async fn pair_changes(&self, pair: PairId) -> Arc<PairChanges> {
		Arc::clone(
			self.changes
				.lock()
				.await
				.entry(pair)
				.or_insert_with(|| Arc::new(PairChanges::new())),
		)
	}

	/// `pair`'s changelists if it still HAS any — never creating one, unlike
	/// [`pair_changes`](Self::pair_changes). For recording something onto a changelist: a pair that
	/// has none is one `remove_pair` has already dropped (or one that was never registered), and
	/// creating an entry under that id leaks it — sqlite reuses pair ids, so the next pair to take
	/// this one would inherit it.
	pub(super) async fn existing_pair_changes(&self, pair: PairId) -> Option<Arc<PairChanges>> {
		self.changes.lock().await.get(&pair).map(Arc::clone)
	}

	/// Record that `pair`'s next pass must read both sides whole, whatever its changelists hold.
	/// A pair with no changelist has nothing to protect: its next pass reads everything anyway.
	pub(super) async fn force_full_pass(&self, pair: PairId, reason: FullPassReason) {
		if let Some(changes) = self.existing_pair_changes(pair).await {
			changes.force(reason);
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
		let (pair, stored_mode) = {
			// The registry lock for the INSERT alone, never for the overlap check above: that check
			// asks the server for two ancestries, and a pass taking its gate must not wait for the
			// network. Add-versus-add is what `registrations` above covers.
			let _registry = self.registry.lock().await;
			// Off the runtime thread for the reason the paused flag is (see `set_control`).
			off_store(&self.control, move |store| {
				store.create_pair(&local, remote_root, mode)
			})
			.await?
			.map_err(|e| db_error(e, "registering a sync pair"))?
		};
		if stored_mode != mode {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!(
					"sync pair {pair} is already registered for these roots in {stored_mode:?} \
					 mode; call reconfigure_pair to change it to {mode:?}"
				),
			));
		}
		// A pair nothing has passed over has no changelist to narrow its first pass with.
		self.pair_changes(pair)
			.await
			.force(FullPassReason::FirstPass);
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
				let prep = self.prepare(pair, None).await?;
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
					&prep.local_scan.nodes.of(&prep.baseline),
					&prep.remote_view.nodes.of(&prep.baseline),
				)
			}
		};
		let adopted_rows = adopted.len();
		let store = self.pair_store(pair).await?;
		// On the PAIR's connection, because the mode and the rows it adopts commit in one
		// transaction and those rows are the pair's own; under the registry lock, because the mode
		// lives in the registry row a removal deletes. Off the runtime thread: an adoption re-seeds
		// one row per path the source no longer has, which at a backup-to-two-way switch is the
		// whole tree.
		let changed = {
			let _registry = self.registry.lock().await;
			off_store(&store, move |store| store.set_mode(pair, mode, &adopted)).await?
		}
		.map_err(|e| db_error(e, "reconfiguring a sync pair"))?;
		if changed == 0 {
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		tracing::debug!(
			"sync pair {pair}: mode changed to {mode:?} from the next pass ({backlog:?}, {adopted_rows} destination item(s) adopted)"
		);
		// A mode change changes what an action MEANS at every path, so the next pass reconciles
		// both sides whole rather than a dirty set under new rules.
		self.pair_changes(pair)
			.await
			.force(FullPassReason::RulesChanged);
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
	/// Returns where in the bin that copy went, so a caller can offer those bytes back without
	/// walking `.filen-sync-trash` and guessing among ` (N)` siblings — the same thing
	/// [`SyncEvent::Quarantined`](super::SyncEvent::Quarantined) does for a copy a PASS moved
	/// aside. `None` when nothing was moved: the two sides held the same content, or the
	/// resolution kept the local copy. [`KeepBoth`](ConflictResolution::KeepBoth) is `None` too —
	/// the copy it renames aside stays in the tree, where the caller can already see it.
	///
	/// A resolution never interleaves with a pass: a pass still reading the pair, or recording the
	/// conflicts it read, is waited for, since the rows it writes would overwrite this one. A
	/// [`paused`](Self::pause_pair) pair stays resolvable — a pause parks a pass between actions,
	/// never inside that read, so the wait is at most the read in flight. That read includes the
	/// local scan, so on a large tree the wait can run to seconds. A pair whose
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
	) -> Result<Option<PathBuf>, Error> {
		// The gate (refusing an unknown pair) keeps a removal waiting for this call; the lock keeps
		// it out of a pass's read. The retirement is read AFTER the lock, since a removal may start
		// while this waits for it.
		let gate = self.pass_gate(pair).await?;
		let _reading = self.reading_lock(pair).await.lock_owned().await;
		if gate.retired() {
			return Err(being_removed(pair));
		}
		let store = self.pair_store(pair).await?;
		let record = locked(&self.control)
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?;
		// One row by primary key — but the lock it needs is the one a directory move's subtree
		// re-key holds on a blocking thread for as long as that subtree takes, and the reading
		// lock is released before a pass applies, so the two do meet. Off the runtime thread, so
		// resolving a conflict while the same pair applies a large move waits without stalling a
		// runtime worker.
		let wanted = rel_path.to_string();
		let mut held = off_store(&store, move |store| store.entry(pair, &wanted))
			.await?
			.map_err(|e| db_error(e, "loading the held conflict"))?
			.filter(|entry| entry.state.is_conflict())
			.ok_or_else(|| {
				Error::custom(
					ErrorKind::InvalidState,
					format!("no conflict is being held at {rel_path:?} for this pair"),
				)
			})?;

		// A divergence this engine's own push created is resolved against the SERVER's version
		// history rather than against what the remote holds now — our upload is what it holds.
		if held.state == BaselineState::Overwritten {
			let quarantined = self
				.resolve_overwritten(pair, &record, rel_path, &held, resolution)
				.await?;
			self.note_resolution(pair).await;
			return Ok(quarantined);
		}

		// Where a local copy this resolution moved aside ended up, for the caller to offer back.
		let mut quarantined = None;
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
			quarantined = apply::quarantine_local(Path::new(&record.local_root), rel_path)?;
			tracing::debug!(
				"resolve_conflict[pair {pair}]: quarantined the local copy of {rel_path:?} as {quarantined:?}"
			);
			held.local_kind = None;
		}

		let resolved = resolution_entry(rel_path, &held, winner);
		let resolving = rel_path.to_string();
		off_store(&store, move |store| match resolved {
			Some(entry) => store.upsert_entry(pair, &entry),
			// The winner's side had nothing at this path: drop the row entirely, so the other
			// side reads as a fresh create (or as already-gone) rather than as a second conflict.
			None => store.delete_entry(pair, &resolving),
		})
		.await?
		.map_err(|e| db_error(e, "resolving a conflict"))?;
		self.note_resolution(pair).await;
		Ok(quarantined)
	}

	/// Record that a resolution is waiting to be applied at a path, so the next pass reads both
	/// sides whole.
	///
	/// The resolution re-anchors the baseline row — the winning side has to read as the CHANGED
	/// one for the next pass to propagate it — and that is a change no filesystem event and no
	/// cache announcement carries: neither side of the pair moved. A pass narrowed to its
	/// changelists would not look at the path at all.
	async fn note_resolution(&self, pair: PairId) {
		self.force_full_pass(pair, FullPassReason::ConflictResolved)
			.await;
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
	///
	/// Returns where our own copy went in the bin — the `KeepRemote` branch, the only one that
	/// moves one — for [`resolve_conflict`](Self::resolve_conflict) to hand on.
	async fn resolve_overwritten(
		&self,
		pair: PairId,
		record: &PairRecord,
		rel_path: &str,
		held: &BaselineEntry,
		resolution: ConflictResolution,
	) -> Result<Option<PathBuf>, Error> {
		let buried_uuid = held.remote_uuid.ok_or_else(|| {
			Error::custom(
				ErrorKind::InvalidState,
				format!("the conflict held at {rel_path:?} names no buried version"),
			)
		})?;
		let local_root = Path::new(&record.local_root);
		let mut quarantined = None;
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
				quarantined = apply::quarantine_local(local_root, rel_path)?;
				tracing::debug!(
					"resolve_conflict[pair {pair}]: restored the version buried at {rel_path:?} and quarantined the local copy as {quarantined:?}"
				);
			}
		}
		// Either way the row has said all it has to say: dropping it lets the next pass reconcile
		// the path from what the two sides now actually hold.
		let store = self.pair_store(pair).await?;
		let resolving = rel_path.to_string();
		off_store(&store, move |store| store.delete_entry(pair, &resolving))
			.await?
			.map_err(|e| db_error(e, "resolving a conflict"))?;
		Ok(quarantined)
	}

	/// The remote file objects for `uuids`, read whole out of the cache (see
	/// [`Client::hydrate_cached_items`](crate::auth::Client)). A uuid the cache does not hold — or
	/// a read that fails — is simply absent: the row that named it stays unconfirmed and its
	/// pending record stands, which is the safe direction for both callers below.
	async fn hydrate_files(&self, uuids: Vec<Uuid>) -> HashMap<Uuid, crate::io::RemoteFile> {
		match self.client.hydrate_cached_items(uuids).await {
			Ok(items) => items
				.into_iter()
				.filter_map(|item| match item {
					SearchResult::File(file) => {
						Some((file.uuid, crate::io::RemoteFile::from(file)))
					}
					SearchResult::Dir(_) => None,
				})
				.collect(),
			Err(error) => {
				tracing::debug!(
					"sync_once: could not read the cached file payloads this pass asks about — {error}"
				);
				HashMap::new()
			}
		}
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
		baseline: &mut Baseline,
		raw_remote: &impl NodesAt<Node = RemoteNode>,
	) -> Vec<BaselineEntry> {
		let (mut confirmed, ask_server) =
			observed_confirmations(&self.observed, baseline, raw_remote, Instant::now());

		// The foreign versions this dates our pushes against, read whole in one go.
		let foreign = self
			.hydrate_files(ask_server.iter().map(|(_, _, uuid)| *uuid).collect())
			.await;
		for (rel_path, ours, stranger) in ask_server {
			let Some(file) = foreign.get(&stranger) else {
				continue;
			};
			let versions = match self.client.list_file_versions(file).await {
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
			match plan::version_chain_verdict(&chain, ours, stranger, CONFIRM_TENURE) {
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
		baseline: &Baseline,
		raw_remote: &impl NodesAt<Node = RemoteNode>,
	) -> Result<(), Error> {
		let candidates = self.pending.strangers(pair, baseline, raw_remote);
		// The standing strangers, read whole in one go: their whole-life id is what the lookup
		// below asks the server by.
		let strangers = self
			.hydrate_files(candidates.iter().map(|(_, _, _, uuid)| *uuid).collect())
			.await;
		let mut retired = Vec::new();
		for (record, path, ours, stranger) in candidates {
			let Some(file) = strangers.get(&stranger) else {
				continue;
			};
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
			let pair_store = self.pair_store(pair).await?;
			// The in-memory retirement and the DELETE that records it under ONE hold of the pair's
			// store, with no await between them: a cancel dropping this read in between — the next
			// candidate's lookup is a whole network call wide — would leave the DB holding a record
			// memory has already retired, and the next open would fold this engine's write back
			// over the version the server just said replaced it.
			let store = locked(&pair_store);
			for record in &retired {
				self.pending.retire(*record);
			}
			store
				.delete_pending(pair, &retired)
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
		let store = self.pair_store(pair).await?;
		let mut baseline = off_store(&store, move |store| store.baseline(pair))
			.await?
			.map_err(|e| db_error(e, "loading the baseline"))?;
		// A pair with nothing awaiting confirmation is the steady state, and the sweep runs in the
		// stretches where no pass does: it must not copy the resident map to find that out. The
		// gate `prepare` uses, and the filter all three confirmation steps already apply.
		if !baseline.any_unconfirmed() {
			return Ok(());
		}
		let advanced = self
			.confirm_pushes(Arc::make_mut(&mut baseline), &HashMap::new())
			.await;
		if advanced.is_empty() {
			return Ok(());
		}
		// The rows come back out of the closure: the push records they retire are named by them.
		let advanced = off_store(&store, move |store| {
			for entry in &advanced {
				store
					.upsert_entry(pair, entry)
					.map_err(|e| db_error(e, "recording the confirmed agreed content"))?;
			}
			Ok::<_, Error>(advanced)
		})
		.await??;
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

	/// Read the remote `.filenignore` files `record`'s mode takes rules from (see
	/// [`load_remote_rules`]), reusing the bodies the pair's last pass read wherever the uuid is the
	/// same.
	async fn remote_rules(
		&self,
		record: &PairRecord,
		baseline: &Baseline,
		view: &RemoteView,
		rule_files: Vec<String>,
		user: Option<IgnoreSource>,
	) -> RemoteRules {
		// Taken rather than copied: a pass on the same pair running alongside only downloads again.
		let cached = self
			.remote_rule_bodies
			.lock()
			.await
			.remove(&record.id)
			.unwrap_or_default();
		let local_root = Path::new(&record.local_root);
		let client = &self.client;
		let mut rules = load_remote_rules(
			record.mode,
			RuleCandidates {
				view,
				baseline,
				paths: rule_files,
			},
			user,
			// Asked only where a two-way pair, or a pushing pair's synced rule file, could read the
			// remote copy: a stat, and a listing where one is found. A file on disk the scan cannot read
			// is still the scan's to block, so the remote copy is not read over it.
			|dir| {
				scan::rule_file_metadata(&local_root.join(dir))
					.map_or(true, |found| found.is_some())
			},
			|dir| baseline.contains_key(&Origin::File { dir }.to_string()),
			&cached,
			|uuid| async move {
				let file = client.get_file(uuid).await?;
				client.download_file(&file).await
			},
		)
		.await;
		self.remote_rule_bodies
			.lock()
			.await
			.insert(record.id, mem::take(&mut rules.bodies));
		rules
	}

	/// The per-pair reads every pass makes before it looks at either tree, whichever way it then
	/// reads them.
	async fn pass_inputs(&self, pair: PairId) -> Result<PassInputs, Error> {
		let (record, user_ignore) = {
			// The registry row and the device-wide patterns: two point reads on the control
			// connection, which no pass ever holds for longer than one statement.
			let control = locked(&self.control);
			let record = control
				.pair(pair)
				.map_err(|e| db_error(e, "loading the sync pair"))?
				.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?;
			let user_ignore = control
				.user_ignore()
				.map_err(|e| db_error(e, "loading the user ignore patterns"))?;
			(record, user_ignore)
		};
		let store = self.pair_store(pair).await?;
		let now = Utc::now().timestamp_millis();
		// The pair's three reads in ONE hop off the runtime thread. The baseline comes from the
		// store's resident copy, which costs a `SELECT` over the whole pair the FIRST time anything
		// asks for it and an `Arc` clone every time after (see [`BaselineStore::baseline`]); the
		// other two ride along rather than paying for a hop each.
		let (baseline, failures, last_ignored) = off_store(&store, move |store| {
			let baseline = store
				.baseline(pair)
				.map_err(|e| db_error(e, "loading the baseline"))?;
			let mut failures = store
				.failures(pair)
				.map_err(|e| db_error(e, "loading the per-path failure counts"))?;
			failures.retain(|_, failure| streak_blocks(failure, now));
			let last_ignored = store
				.ignored_roots(pair)
				.map_err(|e| db_error(e, "loading the recorded ignored paths"))?;
			Ok::<_, Error>((baseline, failures, last_ignored))
		})
		.await??;
		Ok(PassInputs {
			record,
			user_ignore,
			store,
			baseline,
			failures,
			last_ignored,
		})
	}

	/// The read-only half of a pass: either both sides WHOLE — scan, enumerate the remote, build
	/// the view — or only what changed since the last pass, with everything else derived from the
	/// baseline rows (see [`PassRead`], and the optimization plan's section 3.4).
	///
	/// `scope` is the pass's changelists, taken before this runs. `None` is a dry run, which always
	/// reads whole: it is rare, and its report has to show the whole picture (plan 3.5).
	async fn prepare(
		&self,
		pair: PairId,
		scope: Option<&mut PassScope>,
	) -> Result<Prepared, Error> {
		let inputs = self.pass_inputs(pair).await?;
		// The per-pass DB prologue, bounded on its own: five statements and a `spawn_blocking` hop,
		// which read as part of the first phase inside `prepare_scoped` until this mark existed.
		super::step("pass_inputs");
		// The changelists' own reasons, plus the one row of the trigger table that needs the
		// baseline: an empty one is a first sync, and the whole tree is the evidence for it.
		let forced = match &scope {
			Some(scope) => scope.full_pass_reason(inputs.baseline.len()),
			None => None,
		};
		if forced.is_none()
			&& let Some(scope) = scope
		{
			match self.prepare_scoped(inputs, scope).await? {
				Scoped::Prepared(prepared) => return Ok(*prepared),
				// The derivation could not answer for something it was handed.
				Scoped::Whole(inputs, reason) => {
					return self
						.prepare_whole(*inputs, PassRead::Whole(Some(reason)))
						.await;
				}
			}
		}
		self.prepare_whole(inputs, PassRead::Whole(forced)).await
	}

	/// The change-scoped half of [`prepare`](Self::prepare): re-observe the paths the changelists
	/// name, derive everything else from the rows the last pass wrote.
	///
	/// Every question it cannot answer is [`Scoped::Whole`] rather than a partial map. A path
	/// missing from a derived map reads downstream as a deletion — invariant I1's failure mode —
	/// so "derive what I can and leave the rest out" is not one of the options here.
	async fn prepare_scoped(
		&self,
		mut inputs: PassInputs,
		scope: &mut PassScope,
	) -> Result<Scoped, Error> {
		let pair = inputs.record.id;
		// What the last pass left. Without it there is nothing to carry the blocked paths or the
		// guard's two whole-tree judgements from — the state a pair is in until one pass has read
		// it whole in this process.
		let Some(carried) = self.carried.lock().await.get(&pair).cloned() else {
			return Ok(Scoped::Whole(Box::new(inputs), FullPassReason::FirstPass));
		};
		// Copied before the derivation reads anything, for the reason the whole pass copies it
		// before its snapshot: an announcement committed afterwards describes a state this pass
		// did not read, so it must not retire a pending write here.
		let announced = self.observed.snapshot();
		let mut derived = derive::from_baseline(&inputs.baseline, scope.take_local());
		super::step("from_baseline");

		// The remote half FIRST: every path its delta touched is a path the local half has to
		// re-observe too, since a path dirty on either side is re-observed on both.
		let Some(cache_db) = self.client.cache_slot.lock().await.db_path() else {
			return Ok(Scoped::Whole(
				Box::new(inputs),
				FullPassReason::RemoteEventsDegraded,
			));
		};
		let delta = scope.take_remote();
		let root = inputs.record.remote_root;
		let for_remote = Arc::clone(&inputs.baseline);
		let nodes = mem::take(&mut derived.remote);
		// The delta comes back out with the observation: `observe_remote` only borrows it, and a
		// pass cut short hands it to the next one (see `Prepared::remote_delta`).
		let (observed, delta) = tokio::task::spawn_blocking(move || {
			let mut ancestry = |uuid| cache_ancestry(&cache_db, uuid);
			let observed = observe_remote(root, &for_remote, nodes, &delta, &mut ancestry);
			(observed, delta)
		})
		.await
		.map_err(|e| {
			Error::custom(
				ErrorKind::Internal,
				format!("deriving the remote view panicked: {e}"),
			)
		})?;
		let mut observation = match observed {
			RemoteObserved::Applied(observation) => *observation,
			RemoteObserved::Full(reason) => return Ok(Scoped::Whole(Box::new(inputs), reason)),
		};
		super::step("observe_remote");
		// A remote rule file changed: what it hides below itself has no baseline row to derive
		// from, so that subtree cannot be carried. The producer collapses such an event to a whole
		// pass already, which makes this the belt to that braces.
		if !observation.rule_dirs.is_empty() {
			return Ok(Scoped::Whole(
				Box::new(inputs),
				FullPassReason::RulesChanged,
			));
		}
		// The exact keys the delta moved off their rows. `touched` below answers a different
		// question — where to LOOK, a subtree at a time — and this one says what to DECIDE.
		derived.decided.append(&mut observation.changed);
		// TAKEN before the view's held set is built out of it, which is what leaves `derived.held`
		// empty for `merge_local` to put the hidden paths into. The assembly check needs the ROWS this
		// pass holds — which rows, not how many, since it has to leave them out of both ends of its
		// count — and reading the set afterwards would find the hidden paths instead and fall back to
		// a whole read at every held row nothing re-observed.
		let held_rows = mem::take(&mut derived.held);
		let mut view = RemoteView {
			nodes: observation.nodes,
			// A derived view places what the rows and the delta describe and nothing else: an item
			// it cannot place asks for a whole read rather than being skipped, and two items at one
			// path cannot arise from rows that are keyed by path.
			has_collisions: false,
			skipped: Vec::new(),
			ignored: BTreeMap::new(),
			ignored_default_untracked: 0,
			// LOAD-BEARING: the rows that record one side only, and the paths the cache is showing
			// mid-transition. Without them here — and so in `holds.held_remote` below — the missing
			// side of such a row reads as a deletion in `reconcile` (see `derive::Derived::held`).
			held_paths: held_rows
				.iter()
				.cloned()
				.chain(observation.held_paths)
				.collect(),
		};
		derived.dirty.extend(observation.touched.iter().cloned());
		super::step("view_assembly");

		// The rules, from the same sources and in the same order a whole pass reads them.
		let (user, user_error) = match parse_user_ignore(&inputs.user_ignore) {
			Ok(source) => (Some(source), None),
			Err(error) => (None, Some(error)),
		};
		// Which paths could hold a rule file: the resident index, plus any path this pass moved a
		// node onto. The index answers for every row, and a scan of the view would answer for the
		// same rows plus one shape it cannot — a directory move carrying a `.filenignore` to a path
		// no row sits on yet, which `decided` names (`RemoteObservation::changed` was drained into
		// it above). Naming a path twice costs nothing: the loader takes them as a set.
		let rule_files: Vec<String> = inputs
			.baseline
			.rule_file_rows()
			.chain(
				derived
					.decided
					.iter()
					.filter(|path| rule_file_dir(path).is_some())
					.cloned(),
			)
			.collect();
		let mut remote_rules = self
			.remote_rules(&inputs.record, &inputs.baseline, &view, rule_files, user)
			.await;
		super::step("remote_rules");
		if let Some(error) = user_error {
			remote_rules.blocked.insert(String::new());
			remote_rules.errors.push(error.to_string());
		}
		let rule_files = if inputs.record.mode.pushes() {
			RuleFiles::Read
		} else {
			RuleFiles::Only(
				inputs
					.baseline
					.rule_file_rows()
					.filter(|rel_path| !view.nodes.of(&inputs.baseline).holds(rel_path))
					.filter_map(|rel_path| rule_file_dir(&rel_path).map(str::to_owned))
					.collect(),
			)
		};

		super::step("rule_files_only");

		// The local half: one stat per dirty path and its ancestors, one subtree walk per dirty
		// directory. The rules come back carrying only the `.filenignore` files this observation
		// read — the branches it visited — which is why the carried `ignored_remote` facts still
		// stand for everything it did not.
		let local_root = PathBuf::from(&inputs.record.local_root);
		let for_local = Arc::clone(&inputs.baseline);
		let dirty = mem::take(&mut derived.dirty);
		let pass_rules = remote_rules.rules;
		let (observations, rules) = tokio::task::spawn_blocking(move || {
			observe::observe_local(&local_root, &for_local, pass_rules, &rule_files, &dirty)
		})
		.await
		.map_err(|e| {
			Error::custom(
				ErrorKind::Internal,
				format!("re-observing the local paths panicked: {e}"),
			)
		})?;
		super::step("observe_local");
		derive::merge_local(&mut derived, &inputs.baseline, &observations);
		super::step("merge_local");

		// Plan 3.6's self-check, before anything plans against these maps.
		if !assembly_accounted(&inputs.baseline, &derived, &observations, &held_rows) {
			tracing::warn!(
				"sync_once[pair {pair}]: the derived local map ({} node(s)) does not account for \
				 the {} baseline row(s) and {} observation(s) it was built from; reading both \
				 sides instead",
				derived.local.of(&inputs.baseline).len(),
				inputs.baseline.len(),
				observations.observed.len(),
			);
			return Ok(Scoped::Whole(
				Box::new(inputs),
				FullPassReason::AssemblyMismatch,
			));
		}
		super::step("assembly_check");
		// A path this pass set out to look at and could not read leaves ITS absences trustworthy —
		// nothing is derived from a read that did not happen — and the NEXT pass blind, because the
		// list that named the path is drained. So read everything then, rather than wait out the
		// safety net.
		if !observations.complete {
			self.force_full_pass(pair, FullPassReason::IncompleteObservation)
				.await;
		}

		// The rules can take a node OUT of the view here, and what they remove is not recorded in
		// the decided set. It does not need to be, because the other half of a hidden path is gone
		// too: `merge_local` drops the carried local node of anything it observed as hidden, with
		// these same rules, so the two halves AGREE — neither side describes the path, and a path
		// the reconcile finds on neither side plans nothing.
		//
		// Which is what this pass rests on now, rather than on a screen. Every level of the rules
		// forces a whole read when it CHANGES (`FullPassReason::RulesChanged`, from
		// `set_user_ignore` and from a rule file on either side), so with the rules standing still
		// what they hide normally has no row to be carried from — the pass that first hid it
		// untracked those rows. Where it does — an `untrack_ignored` whose `delete_subtrees`
		// failed leaves rows at hidden paths and forces nothing — the two absences would read as a
		// convergent delete and retire a live row, so `merge_local` holds those paths as well and
		// the reconcile decides nothing at them. `drop_blocked` still drops anything planned at or
		// under an ignored root (they are all in `blocked_paths()`, from
		// `facts.ignored_local`/`ignored_remote`, which this pass fills from this very filter and
		// from `LocalObservation::Hidden`) — it is the belt to this, not the thing standing
		// between the user and a local delete.
		view.filter_changed(
			plan::ViewFilter {
				rules: &rules,
				baseline: &inputs.baseline,
			},
			&derived.decided,
		);
		super::step("view_filter");
		// The paths the observation found hidden with a row still behind them, withheld for the
		// same reason the half-written rows above are: no derived map describes them. Added AFTER
		// the filter, so the collision check inside it does not claim a hidden path's folded name
		// and refuse the pass over a name nothing is syncing.
		view.held_paths.append(&mut derived.held);
		// The facts: the remote half pruned at the paths the delta touched, the local half at the
		// paths this pass observed, each replaced by what that evidence says now.
		let mut facts = carried.facts;
		facts.merge_remote_view(&observation.touched, &view, &inputs.baseline);
		facts.observe_local(&observations);
		super::step("facts_merge");

		// Tenure and the server's version chain ONLY, never `confirm_agreed_content`: that would
		// confirm rows against a view derived from those very rows (I6). What this still costs is a
		// confirmation delayed to the next whole pass, never one granted early — the verdicts come
		// from announcements, and the view is only read to spot a FOREIGN version at a row's path.
		let mut confirmed = Vec::new();
		if inputs.baseline.any_unconfirmed() {
			// Read BEFORE the rows are borrowed mutably: a change-scoped view derives its nodes
			// from those very rows, so it cannot be read across the `make_mut`. Only the
			// unconfirmed rows' own paths are looked up — `observed_confirmations` asks about no
			// other — so this is the per-push work the confirmation already was, never the tree.
			// Cloning the `Arc` instead would deep-copy the whole tree on every pass after a push.
			let at_unconfirmed: HashMap<String, RemoteNode> = {
				let nodes = view.nodes.of(&inputs.baseline);
				inputs
					.baseline
					.unconfirmed()
					.filter_map(|row| {
						nodes
							.at(&row.rel_path)
							.map(|node| (row.rel_path.clone(), node.into_owned()))
					})
					.collect()
			};
			let rows = Arc::make_mut(&mut inputs.baseline);
			confirmed = self.confirm_pushes(rows, &at_unconfirmed).await;
		}
		super::step("confirm_pushes");

		let mut holds = {
			let journal = locked(&inputs.store);
			let before = self.pending.uuids();
			// An EMPTY map, deliberately: `settle`'s third rule retires a write whose item the
			// SNAPSHOT shows at its path, and this pass has no snapshot. The derived view shows our
			// own write because the baseline row records it, so reading it there would retire the
			// record against itself (I5). The announcement rule and the grace ceiling still apply.
			let holds =
				self.pending
					.settle(pair, &announced, &HashMap::<String, RemoteNode>::new());
			let retired: Vec<Uuid> = before.difference(&self.pending.uuids()).copied().collect();
			if !retired.is_empty() {
				journal
					.delete_pending(pair, &retired)
					.map_err(|e| db_error(e, "retiring pending writes"))?;
			}
			holds
		};
		self.retire_superseded_creates(pair, &inputs.baseline, &view.nodes.of(&inputs.baseline))
			.await?;
		let folded = self.pending.fold_into(
			pair,
			&inputs.baseline,
			&mut view.nodes,
			&mut derived.decided,
		);
		if folded > 0 {
			tracing::debug!(
				"sync_once[pair {pair}]: folding {folded} unacknowledged write(s) into the derived view"
			);
		}
		super::step("pending_settle_and_fold");
		// The other half of plan 3.6's self-check, here because this is where the last producer has
		// run: `assembly_accounted` above bounds what the maps HOLD, and this asks whether the pass
		// would actually decide it.
		//
		// A DEBUG assert, which is the one thing here that was decided by measurement rather than
		// by argument. The failure mode is silent and that argues for a loud release check — a pass
		// that skips a key plans nothing at it and reports nothing about it, which reads exactly
		// like a path where the three inputs agree. But the check is one `Baseline` path resolve
		// per map key, and while the maps are still the whole tree that is not a rounding error: on
		// the probe's 20,3,73 tree it took the one-file-changed pass from 34.2 ms to 162.3 ms at
		// 100k and from 395.2 ms to 3191.7 ms at 1M, with `pass_pure` steady at 1232.9/1238.8 ms
		// and 29.88/29.83 s across the two runs. Those figures were taken on the PRE-CORRECTION
		// phase, which timed `from_baseline -> observe_local -> merge_local -> fold_dir_moves ->
		// reconcile` and stopped. They stand as a RATIO — both halves measured the same phase —
		// and must not be set beside the 50 ms target, which the docs read off the wider
		// `scoped_twoway_*` rows: 395.2 is not comparable with what those record.
		// Paying 7x the phase to re-derive what the producers already recorded is the wrong trade
		// for a shipped pass.
		//
		// So it runs in every test binary this crate has — the unit tests, `sync_suite`, the
		// blackbox and stress binaries, all debug builds exercising real passes — and in no
		// release one. What changes that is plan 6.4: once the maps hold what the pass READ instead
		// of the whole baseline, this is O(changed) and belongs in the release path.
		// In EVERY build now, where this was a `debug_assert` while the two sides were still the
		// whole tree. It costs one baseline resolve per key a producer placed — the change, not
		// the tree — so the failure it catches is worth a whole read rather than a silent skip: a
		// key the reconcile never visits is one nothing is planned or reported at.
		//
		// What the fallback re-does: `pending.settle` and `retire_superseded_creates` have already
		// run, and the `confirmed` rows this pass advanced are dropped. A whole pass redoes all
		// three against a real snapshot, which is the stricter evidence anyway — the cost is one
		// wasted settle/retire round trip and a confirmation deferred by a pass.
		if let Some(key) = unaccounted_key(
			&inputs.baseline,
			&derived.local,
			&view.nodes,
			&derived.decided,
		) {
			tracing::warn!(
				"sync_once[pair {pair}]: a derived side holds {key:?}, which is no baseline row and \
				 which no producer recorded, so the narrowed reconcile would never decide it; \
				 reading both sides instead"
			);
			return Ok(Scoped::Whole(
				Box::new(inputs),
				FullPassReason::AssemblyMismatch,
			));
		}
		super::step("decided_check");
		holds.held_remote = view.held_paths.clone();
		self.observed.prune_before(self.pending.oldest_stamp());

		let LocalObservations {
			siblings,
			mut errors,
			complete,
			..
		} = observations;
		// The per-directory collision check a whole walk makes with its `claimed` set: two names in
		// one directory that fold together have no 1:1 local mapping, and the pair is refused until
		// the user resolves it. Narrower than the walk's on purpose — only names this pass holds a
		// node for count — so a collision between two paths it never looked at cannot refuse a pass
		// that is not touching them.
		for (dir, folded) in &siblings {
			for names in folded.values().filter(|names| names.len() > 1) {
				let present: Vec<String> = names
					.iter()
					.map(|name| plan::join_path(dir, name))
					.filter(|path| derived.local.of(&inputs.baseline).holds(path))
					.collect();
				if let Some(rel_path) = present.get(1) {
					errors.push(ScanError::DuplicateName {
						rel_path: rel_path.clone(),
					});
				}
			}
		}

		// Plan 3.4's composition: the last whole read's walk was complete, every dirty stat and
		// walk of this one succeeded, and nothing has since made the next pass a whole-tree one —
		// a reason recorded while this pass was reading says the list it narrowed itself with may
		// be missing exactly what it is about to act on.
		let narrowed_read_is_whole = match self.existing_pair_changes(pair).await {
			Some(changes) => !changes.full_pending(),
			None => false,
		};
		let local_scan = LocalScan {
			nodes: mem::take(&mut derived.local),
			complete: carried.scan_complete && complete && narrowed_read_is_whole,
			errors,
			// A change-scoped pass reads these off `facts`, which is where the carried ones live.
			// The scan struct is only the shape the rest of the pass expects its nodes in.
			invalid_names: BTreeMap::new(),
			aliased_dirs: BTreeMap::new(),
			ignored: BTreeMap::new(),
			ignored_default_untracked: 0,
			ignore_blocked: BTreeSet::new(),
		};
		let remote_emptied = remote_emptied(&view.nodes.of(&inputs.baseline), &inputs.baseline);
		let mut prepared = Prepared {
			record: inputs.record,
			baseline: inputs.baseline,
			read: PassRead::Scoped(mem::take(&mut derived.decided)),
			remote_delta: delta,
			dir_moves: Vec::new(),
			local_scan,
			remote_view: view,
			// Neither judgement can be made from a derived view: both are what the last WHOLE read
			// found, and every hold they cause re-forces a whole read anyway (plan 3.5).
			remote_converged: carried.remote_converged,
			remote_emptied,
			holds,
			failures: inputs.failures,
			facts,
			remote_blocked: remote_rules.blocked,
			remote_rule_errors: remote_rules.errors,
			last_ignored: inputs.last_ignored,
			confirmed,
		};
		super::step("local_scan_assembly");
		prepared.fold_dir_moves();
		super::step("fold_dir_moves");
		Ok(Scoped::Prepared(Box::new(prepared)))
	}

	/// The whole-tree half of [`prepare`](Self::prepare): read both sides entire, which is the only
	/// read that can find a change nothing announced.
	async fn prepare_whole(&self, inputs: PassInputs, read: PassRead) -> Result<Prepared, Error> {
		let PassInputs {
			record,
			user_ignore,
			store,
			mut baseline,
			failures,
			last_ignored,
		} = inputs;
		let pair = record.id;

		// Copied BEFORE the snapshot is read: an event the cache commits afterwards describes a
		// state this snapshot predates, so it must not retire a pending write this pass.
		let observed = self.observed.snapshot();
		// One view, built once and unfiltered at first: what the snapshot confirms, which remote
		// rule files there are, which of this engine's own writes the cache has caught up to, and
		// whether the remote reads as emptied are facts about the remote as it is, whatever the
		// rules hide — and the rules cannot be known until the scan below has read the
		// `.filenignore` files on disk. Those reads come first; then the very rules the scan
		// matched with hide what they hide, in place (see `RemoteView::filter`).
		//
		// Built from the cache's rows AS THEY ARRIVE. A materialized read hands back two `Vec`s
		// of the whole subtree, and at a million items that is a second copy of the tree, alive
		// beside the view built out of it at the widest point the pass ever reaches. The
		// baseline's row count sizes the map, so a converged pair's view never grows one.
		//
		// A read that fails partway takes the builder with it (see `stream_sync_root_snapshot`):
		// this pass gets an error, never a view with holes in it.
		let (built, watermark) = self
			.client
			.stream_sync_root_snapshot(
				record.remote_root,
				plan::ViewBuilder::with_capacity(record.remote_root, baseline.len()),
			)
			.await?;
		// Every row the pass still needs is in the view now; the objects it ACTS on are read back
		// whole by uuid, for the handful a plan names.
		let mut view = built.finish();
		let remote_converged = watermark.is_some();

		// Read the snapshot BEFORE the local scan, so the confirmation below runs against the RAW
		// view — before this engine's own writes are folded into it, and before the baseline is
		// shared (immutably) with the scan. A row the snapshot confirms is one both sides
		// demonstrably hold, which is what a later foreign edit is measured against.
		// Asked before the map is touched: the confirmation advances the rows an unconfirmed push
		// left behind, and a pair with none — the steady state — must not copy the whole resident
		// baseline to find that out.
		let mut confirmed = Vec::new();
		if baseline.any_unconfirmed() {
			// A WHOLE view, whose backing derives nothing — so reading it against an empty tree is
			// exact, and leaves the rows free to be borrowed mutably beside it.
			let no_rows = Baseline::default();
			let raw = view.nodes.of(&no_rows);
			let rows = Arc::make_mut(&mut baseline);
			confirmed = plan::confirm_agreed_content(rows, &raw);
			confirmed.extend(self.confirm_pushes(rows, &raw).await);
		}
		let baseline = baseline;

		// `set_user_ignore` refuses a text that does not compile, so a stored one fails only if this
		// build reads patterns differently from the one that stored it. Guessing what it hides could
		// sync what the user meant to hide: block the whole pair and say why.
		let (user, user_error) = match parse_user_ignore(&user_ignore) {
			Ok(source) => (Some(source), None),
			Err(error) => (None, Some(error)),
		};
		let mut remote_rules = self
			.remote_rules(
				&record,
				&baseline,
				&view,
				rule_file_paths(&view, &baseline),
				user,
			)
			.await;
		if let Some(error) = user_error {
			remote_rules.blocked.insert(String::new());
			remote_rules.errors.push(error.to_string());
		}
		let local_root = PathBuf::from(&record.local_root);
		let scan_baseline = Arc::clone(&baseline);
		// Rules come from the side that is the source of truth: a mode that pushes reads the
		// `.filenignore` files on disk, over any remote copy read for the same directory. A mode that
		// pulls alone reads on disk only the synced ones the remote has lost: they govern until that
		// loss propagates, so a directory trashed on the remote keeps what they hide here.
		let rule_files = if record.mode.pushes() {
			RuleFiles::Read
		} else {
			RuleFiles::Only(
				baseline
					.rule_file_rows()
					.filter(|rel_path| !view.nodes.of(&baseline).holds(rel_path))
					.filter_map(|rel_path| rule_file_dir(&rel_path).map(str::to_owned))
					.collect(),
			)
		};
		let (mut local_scan, rules) = tokio::task::spawn_blocking(move || {
			scan::scan_local(&local_root, &scan_baseline, remote_rules.rules, rule_files)
		})
		.await
		.map_err(|e| Error::custom(ErrorKind::Internal, format!("local scan panicked: {e}")))?;
		// This pass's facts, computed whole: `""` answers for every path, so the merge replaces
		// whatever the previous pass carried (see `PairFacts::merge_local_scan`). Kept on the pass,
		// where directory moves re-key them along with everything else it blocks.
		let mut facts = PairFacts::default();
		facts.merge_local_scan("", &local_scan);
		// One copy only, on `facts`: a directory move re-keys that one (`Prepared::fold_dir_moves`)
		// and nothing re-keys a second, so a reader of the scan's copy would get paths the pass has
		// already moved on from. The change-scoped path builds its `LocalScan` with these empty for
		// the same reason.
		local_scan.invalid_names = BTreeMap::new();
		local_scan.aliased_dirs = BTreeMap::new();
		local_scan.ignored = BTreeMap::new();
		local_scan.ignore_blocked = BTreeSet::new();
		let remote_blocked = remote_rules.blocked;

		// The reads that are about the remote AS IT IS come before anything is hidden: what the
		// cache shows is evidence whatever the rules hide.
		let remote_emptied = remote_emptied(&view.nodes.of(&baseline), &baseline);
		// The rows `settle` retires have to leave the DB too, or a restart would fold writes the
		// cache has demonstrably caught up to. Diffed around the call so `settle` itself stays a
		// pure in-memory operation, and the two halves — the in-memory retirement and the DELETE
		// that records it — run under ONE hold of the pair's store with no await between them. This
		// read runs under the pass's cancel, and a cancel dropped between the halves would leave
		// the retirement standing in memory and not in the DB.
		let mut holds = {
			let journal = locked(&store);
			let before = self.pending.uuids();
			let holds = self
				.pending
				.settle(pair, &observed, &view.nodes.of(&baseline));
			let retired: Vec<Uuid> = before.difference(&self.pending.uuids()).copied().collect();
			if !retired.is_empty() {
				journal
					.delete_pending(pair, &retired)
					.map_err(|e| db_error(e, "retiring pending writes"))?;
			}
			holds
		};
		// A create whose path shows another version of the same file is the one thing the fold
		// cannot settle on its own; ask the server before it paints over a stranger.
		self.retire_superseded_creates(pair, &baseline, &view.nodes.of(&baseline))
			.await?;

		// Hidden with exactly the rules the scan matched with, so both sides hide the same paths.
		// From here on the view is the filtered set — what reconcile, the folds and apply act on.
		view.filter(Some(plan::ViewFilter {
			rules: &rules,
			baseline: &baseline,
		}));
		let mut remote_view = view;
		// Hidden on both sides, but carried as roots on neither: the `.DS_Store` in every folder
		// that no row was ever written for. The report leaves them out and untracking them deletes
		// nothing, so they only cost the pass the filters they scale.
		let hidden_by_defaults =
			local_scan.ignored_default_untracked + remote_view.ignored_default_untracked;
		if hidden_by_defaults > 0 {
			tracing::debug!(
				"sync_once[pair {pair}]: {hidden_by_defaults} item(s) hidden by the built-in \
				 defaults with nothing synced at or under them, so not tracked as ignored roots"
			);
		}
		// The remote half, also whole: `[""]` clears every carried remote fact, and the merge runs
		// the `unknown_remote_paths` read itself over what this view skipped.
		facts.merge_remote_view(&BTreeSet::from([String::new()]), &remote_view, &baseline);

		// Correct the view with what this engine wrote and the cache has not shown yet, BEFORE
		// anything reconciles or detects moves against it.
		let folded = self
			.pending
			// A whole read decides every path, so which ones the fold moved is a question nothing
			// asks here.
			.fold_into(
				pair,
				&baseline,
				&mut remote_view.nodes,
				&mut BTreeSet::new(),
			);
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
			read,
			// A whole read derives nothing from the announced changes, so it has none to hand on.
			remote_delta: Vec::new(),
			dir_moves: Vec::new(),
			local_scan,
			remote_view,
			remote_converged,
			remote_emptied,
			holds,
			failures,
			facts,
			remote_blocked,
			remote_rule_errors: remote_rules.errors,
			last_ignored,
			confirmed,
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
		let prep = self.prepare(pair, None).await?;
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
			ignored: prep.ignored(),
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
		// The threshold lives in the registry row, so it takes the registry lock for the same
		// reason the paused flag does: a removal must not land between the write and the answer.
		let _registry = self.registry.lock().await;
		// Off the runtime thread for the reason the paused flag is (see `set_control`): a write
		// waits out another connection's transaction inside SQLite, not on a lock here.
		let changed = off_store(&self.control, move |store| {
			store.set_delete_guard(pair, guard)
		})
		.await?
		.map_err(|e| db_error(e, "setting the delete guard"))?;
		if changed == 0 {
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		Ok(())
	}

	/// Replace the device-wide user ignore patterns: gitignore syntax, read by every pair below its
	/// `.filenignore` files and above [`DEFAULT_IGNORE_PATTERNS`](super::DEFAULT_IGNORE_PATTERNS),
	/// from each pair's next pass. Persisted in the baseline DB; `""` clears them.
	///
	/// A text with a line that does not parse is refused whole, naming the first bad line, and the
	/// stored patterns stay as they were.
	///
	/// Every running watch is woken once the new patterns are stored, so the pass that applies them
	/// runs at once rather than at that loop's [`safety_net`](super::WatchConfig::safety_net) tick.
	/// A pass already running is left alone — it planned against the patterns it read, and the pass
	/// behind it re-reads them. A [`paused`](Self::pause_pair) pair's loop runs nothing until it is
	/// resumed, and then picks this up with everything else it missed. Nor is a loop waiting out the
	/// backoff after a failed pass woken: that delay is its retry timer, it is syncing nothing while
	/// it waits, and the retry it already owes re-reads the patterns.
	pub async fn set_user_ignore(&self, patterns: &str) -> Result<(), Error> {
		parse_user_ignore(patterns).map_err(|e| {
			Error::custom(
				ErrorKind::InvalidState,
				format!("refusing the user ignore patterns: {e}"),
			)
		})?;
		// Off the runtime thread for the reason the paused flag is (see `set_control`).
		let stored = patterns.to_owned();
		off_store(&self.control, move |store| store.set_user_ignore(&stored))
			.await?
			.map_err(|e| db_error(e, "storing the user ignore patterns"))?;
		// Only once the write has committed: a loop this wakes reads the patterns back out of the
		// DB, so a wake-up sent any earlier could run a pass on the old ones. `send_modify` rather
		// than `send`, which fails when the last watch has gone — nobody to wake is not an error.
		self.user_ignore_changed
			.send_modify(|version| *version = version.wrapping_add(1));
		// The patterns are device-wide: what is hidden may have changed at any path of any pair, so
		// every pair's next pass reads both sides whole.
		for changes in self.changes.lock().await.values() {
			changes.force(FullPassReason::RulesChanged);
		}
		Ok(())
	}

	/// A signal that fires whenever [`set_user_ignore`](Self::set_user_ignore) commits new
	/// patterns, for a watch loop to wait on (see [`watch_with`](Self::watch_with)).
	pub(super) fn user_ignore_changes(&self) -> tokio::sync::watch::Receiver<u64> {
		self.user_ignore_changed.subscribe()
	}

	/// The stored user ignore patterns, `""` when none were set.
	pub async fn user_ignore(&self) -> Result<String, Error> {
		locked(&self.control)
			.user_ignore()
			.map_err(|e| db_error(e, "loading the user ignore patterns"))
	}

	/// Every sync pair this engine has registered, in registration order.
	pub async fn list_pairs(&self) -> Result<Vec<PairRecord>, Error> {
		locked(&self.control)
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
		let pair_store = self.pair_store(pair).await?;
		let _registry = self.registry.lock().await;
		// Under the registry lock, which is also the lock a watch reads the pair and subscribes
		// under (see `watchable_pair`): a watch setting up right now either registered before this
		// and gets tripped here, or reads the deleted row afterwards and is refused. Before the
		// delete, so a loop sitting between two passes learns about the removal at once rather than
		// starting one more against the rows this is about to take away.
		if let Some(signal) = self.removals.lock().await.remove(&pair) {
			let _ = signal.send(true);
		}
		// On the pair's OWN connection, and off the runtime thread: the registry row is one row,
		// but the cascade under it takes every baseline, journal and failure row the pair ever had.
		// On the control connection that cascade would be the one long hold on the lock every other
		// pair's control verbs go through.
		off_store(&pair_store, move |store| store.delete_pair(pair))
			.await?
			.map_err(|e| db_error(e, "removing a sync pair"))?;
		// Under the same lock as the delete, and as `set_control`'s own write: a pause that landed
		// in between would leave the map holding an id whose row is gone. The channel itself is
		// already `Retired` — that is what kept a pause or resume made while this WAITED for the
		// pass from taking the cancel back — so there is nothing left to say on it.
		self.paused.lock().await.remove(&pair);
		self.reading.lock().await.remove(&pair);
		self.remote_rule_bodies.lock().await.remove(&pair);
		// Nothing of the removed pair is left to narrow a pass with (a pair re-registered under the
		// same id — sqlite reuses one — starts from an empty changelist and a forced first pass).
		self.changes.lock().await.remove(&pair);
		self.carried.lock().await.remove(&pair);
		// The pair's connection goes with the pair. Last, so nothing above can reopen it, and the
		// file handle closes as soon as the cascade above lets this function's own handle go.
		self.stores.lock().await.remove(&pair);
		Ok(())
	}

	/// Stop the pass in flight on `pair` and wait (up to [`REMOVE_CANCEL_GRACE`]) for it to end: a
	/// suspension becomes a cancel, the transfer running is dropped, and every action behind either
	/// is skipped, so the pass returns an [`interrupted`](SyncReport::interrupted) report. Waits on
	/// the pass's own gate going away, which is what tells the two apart — a pass that has ended
	/// from one that has merely been told to.
	///
	/// Holds NO other lock meanwhile: the pass it waits for takes its pair's store lock on its way
	/// out, to record what it did and what failed.
	async fn cancel_pass_in_flight(&self, pair: PairId) {
		// Only a pass that has ASKED for its gate is reachable — and waited for — here, which a pass
		// does before it reads either side, so scanning and planning are covered as well as
		// applying. A pass that asks for its gate AFTER this is stopped by `pass_gate` refusing it,
		// under the same registry lock this removal deletes the pair row under.
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
		// A paused pair ran no pass to take its changelists, so a local set that overflowed while
		// it waited is all the first pass afterwards would have: read both sides whole instead.
		self.pair_changes(pair)
			.await
			.force(FullPassReason::FirstPass);
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
	/// Taken under the registry lock, and REFUSED for a pair the registry no longer knows: a pass
	/// that asks for its gate after [`remove_pair`](Self::remove_pair) has finished learns here that
	/// its pair is gone — before it reads or applies anything. Under the same lock as the delete, so
	/// the two cannot interleave: either the gate is in the map before the removal reads it, and the
	/// removal cancels it and waits for it, or the row is already gone and there is no pass to gate.
	async fn pass_gate(&self, pair: PairId) -> Result<PassGate, Error> {
		let _registry = self.registry.lock().await;
		if locked(&self.control)
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.is_none()
		{
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		Ok(PassGate::new(self.control_channel(pair).await))
	}

	/// `pair`'s own baseline connection, opened if this is the first time anyone asked.
	///
	/// Every connection is to the same file, so which one a statement runs on decides only who
	/// waits for it, never what it can see. A pair's whole-tree work therefore holds a lock nothing
	/// outside that pair takes.
	/// A pair the registry no longer knows gets NO connection. [`remove_pair`](Self::remove_pair)
	/// drops the pair's entry from the map last of all, so a verb that read the registry before the
	/// removal and asked for the store after it would otherwise open a fresh connection under a
	/// dead id — one nothing ever removes, because the pair whose removal would have is already
	/// gone. Checked under the registry lock, which is the lock the removal deletes the row under,
	/// so the two cannot interleave; every caller is therefore covered rather than each verb having
	/// to remember to check first.
	async fn pair_store(&self, pair: PairId) -> Result<SharedStore, Error> {
		{
			let _registry = self.registry.lock().await;
			if locked(&self.control)
				.pair(pair)
				.map_err(|e| db_error(e, "loading the sync pair"))?
				.is_none()
			{
				return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
			}
		}
		let mut stores = self.stores.lock().await;
		if let Some(store) = stores.get(&pair) {
			return Ok(Arc::clone(store));
		}
		// Opened under the map's lock, so two first uses of one pair cannot end up with a
		// connection each — which would read and write the same rows correctly, but would put the
		// pair's own statements behind SQLite's write lock instead of behind its mutex, where the
		// waiting is visible and bounded.
		let path = self.db_path.clone();
		let store = tokio::task::spawn_blocking(move || BaselineStore::open(&path))
			.await
			.map_err(|e| {
				Error::custom(ErrorKind::Internal, format!("baseline open panicked: {e}"))
			})??;
		let store = Arc::new(std::sync::Mutex::new(store));
		stores.insert(pair, Arc::clone(&store));
		Ok(store)
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
		let _registry = self.registry.lock().await;
		if locked(&self.control)
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.is_none()
		{
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		Ok(self.control_channel(pair).await)
	}

	/// The pair a watch is about to start on, together with the signal that ends its loop.
	///
	/// Both come out of ONE hold of the registry lock, and [`remove_pair`](Self::remove_pair) trips
	/// the signal under that same lock, so a removal racing a watch's setup has two outcomes only:
	/// either it deleted the row first and this reports an unknown pair, or it finds the
	/// subscription already registered and trips it. Subscribing afterwards — the setup does real
	/// work, a cache registration and a filesystem watcher, in between — would let a removal fall
	/// into the gap and go unheard, leaving the loop retrying a pair that is gone.
	pub(super) async fn watchable_pair(
		&self,
		pair: PairId,
	) -> Result<(PairRecord, tokio::sync::watch::Receiver<bool>), Error> {
		let _registry = self.registry.lock().await;
		let record = locked(&self.control)
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
		// Both writes happen under the registry lock, so a concurrent `remove_pair` cannot land
		// between them and leave the map holding a pair it has already deleted.
		let _registry = self.registry.lock().await;
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
		//
		// Off the runtime thread, because this is a WRITE: WAL lets a reader through during one,
		// but a second WRITER waits for the first to commit, and it waits inside `sqlite3_step`'s
		// busy handler rather than on any lock this code holds. So a pause made while another pair
		// commits its first sync sleeps out that transaction — half a second at 100k rows, twelve
		// at a million — and would do it on whichever runtime thread called in. The registry lock
		// is held across the hop, which is what still keeps a removal from landing between this
		// write and the answer below.
		let paused = control.is_paused();
		let known = off_store(&self.control, move |store| store.set_paused(pair, paused))
			.await?
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
		// A pass cut short by a cancel cannot tell which of its actions ran, and an action that
		// never ran proves nothing about the path: clearing its streak would hand a permanently
		// broken path a fresh set of retries every time someone pauses. Only the failures count. A
		// pass that found a side full held back transfers the same way.
		let cleared: Option<Vec<String>> = (report.interrupted == 0 && report.halted.is_none())
			.then(|| {
				attempted
					.iter()
					.filter(|path| !failed.contains_key(path.as_str()))
					.cloned()
					.collect()
			});
		let recorded: Vec<(String, String)> = failed
			.iter()
			.map(|(path, error)| ((*path).to_string(), (*error).to_string()))
			.collect();
		let store = match self.pair_store(pair).await {
			Ok(store) => store,
			Err(error) => {
				report
					.errors
					.push(format!("recording this pass's path outcomes: {error}"));
				return;
			}
		};
		let now = Utc::now().timestamp_millis();
		// One statement per attempted path, so this scales with the plan rather than with the
		// tree — but a first sync's plan IS the tree. Off the runtime thread, with the paths owned
		// by the closure that writes them.
		let outcome = off_store(&store, move |store| {
			let mut problems = Vec::new();
			for (path, error) in &recorded {
				if let Err(e) = store.record_failure(pair, path, error, now) {
					problems.push(format!("{path}: recording the failure count failed: {e}"));
				}
			}
			if let Some(cleared) = cleared {
				let cleared: Vec<&str> = cleared.iter().map(String::as_str).collect();
				if let Err(e) = store.clear_failures(pair, &cleared) {
					problems.push(format!(
						"clearing the failure count of {} applied path(s) failed: {e}",
						cleared.len()
					));
				}
			}
			problems
		})
		.await;
		match outcome {
			Ok(problems) => report.errors.extend(problems),
			Err(error) => report
				.errors
				.push(format!("recording this pass's path outcomes: {error}")),
		}
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
		if locked(&self.control)
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.is_none()
		{
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		let store = self.pair_store(pair).await?;
		let rel_path = rel_path.to_string();
		// One row, so the statement itself is microseconds — but the pair it names may be in the
		// middle of a pass, whose baseline read holds this lock for as long as the tree takes. Off
		// the runtime thread, so a caller retrying a path cannot block one.
		off_store(&store, move |store| store.clear_failure(pair, &rel_path))
			.await?
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
		if locked(&self.control)
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.is_none()
		{
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		let store = self.pair_store(pair).await?;
		// A seek of the `(pair_id, state)` index rather than a read of the pair's rows, and it
		// deliberately does not wait for the pass in flight. Still off the runtime thread: the
		// statement is cheap now, but it queues behind whatever the pass in flight is holding
		// the pair's store mutex for.
		Ok(off_store(&store, move |store| store.conflicts(pair))
			.await?
			.map_err(|e| db_error(e, "loading the held conflicts"))?
			.into_iter()
			.map(|entry| {
				PlannedConflict::new(
					entry.rel_path,
					entry.local_kind.map(PlannedNodeKind::from),
					entry.remote_kind.map(PlannedNodeKind::from),
				)
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
		self.sync_pass(pair, observer, WhenIdle::Run).await
	}

	/// [`sync_once_observed`](Self::sync_once_observed) plus what a finished pass owes the next one:
	/// its outcome decides whether that pass may narrow itself to its changelists (see
	/// [`next_pass_scope`]).
	pub(super) async fn sync_pass(
		&self,
		pair: PairId,
		observer: &mut (dyn FnMut(SyncEvent) + Send),
		when_idle: WhenIdle,
	) -> Result<SyncReport, Error> {
		let mut contained = super::events::contain_panics(observer);
		let observer: &mut (dyn FnMut(SyncEvent) + Send) = &mut contained;
		let result = self.run_pass(pair, observer, when_idle).await;
		// What this pass's outcome means for the NEXT one's scope, wherever it ended. A pass that
		// failed outright took both changelists and applied nothing, so what it took is reflected
		// nowhere and the next pass cannot rely on them.
		let next_scope = match &result {
			Ok(report) => next_pass_scope(report),
			Err(_) => Some(FullPassReason::InterruptedPass),
		};
		if let Some(reason) = next_scope {
			self.force_full_pass(pair, reason).await;
		}
		if let Err(error) = &result {
			observer(SyncEvent::PassFailed {
				error: error.to_string(),
			});
		}
		result
	}

	/// Drop the baseline rows at or under every path this pass found ignored, touching neither tree:
	/// the path stops syncing, and once no rule hides it any more it is read like a first sync. Then
	/// record the ignored roots the next pass needs to tell when that happens (see
	/// [`Prepared::ignored_roots_to_record`]). A failure is reported; rows left behind are blocked and
	/// dropped again by the next pass.
	async fn untrack_ignored(&self, pair: PairId, prep: &Prepared, report: &mut SyncReport) {
		let roots = prep.ignored_roots();
		let record =
			prep.ignored_roots_to_record(report.held.iter().map(|action| action.rel_path.as_str()));
		if roots.is_empty() && record == prep.last_ignored {
			return;
		}
		let store = match self.pair_store(pair).await {
			Ok(store) => store,
			Err(error) => {
				report
					.errors
					.push(format!("untracking ignored paths: {error}"));
				return;
			}
		};
		let record = (record != prep.last_ignored).then_some(record);
		// Two seeking statements per ignored root, and the roots are whatever the rules matched:
		// off the runtime thread, with both sets owned by the closure that writes them.
		let outcome = off_store(&store, move |store| {
			let mut problems = Vec::new();
			if !roots.is_empty()
				&& let Err(error) = store.delete_subtrees(pair, &roots)
			{
				problems.push(format!("untracking ignored paths: {error}"));
			}
			if let Some(record) = record
				&& let Err(error) = store.set_ignored_roots(pair, &record)
			{
				problems.push(format!("recording the ignored paths: {error}"));
			}
			problems
		})
		.await;
		match outcome {
			Ok(problems) => report.errors.extend(problems),
			Err(error) => report
				.errors
				.push(format!("untracking ignored paths: {error}")),
		}
	}

	/// What a pass that made a plan leaves the next one: the facts it blocked with, the two
	/// whole-tree judgements only a whole read makes, and the paths its plan owes a second look at
	/// (the optimization plan's section 3.4 step 7, and its carry-over set in 3.6).
	///
	/// Not called by a pass that REFUSED: a refusal forces the next pass whole, which rebuilds all
	/// of this from a read of both sides.
	async fn carry_forward(
		&self,
		pair: PairId,
		prep: &Prepared,
		report: &SyncReport,
		decision: &guard::GuardDecision,
	) {
		// The UNFILTERED streaks, not `Prepared::failures`: a path whose attempts are below the
		// blocking threshold is filtered out of the pass, and filtering it out here too would mean
		// no later pass ever looks at it again and its retry never comes.
		let failures = match self.pair_store(pair).await {
			Ok(store) => off_store(&store, move |store| store.failures(pair))
				.await
				.ok()
				.and_then(Result::ok)
				.unwrap_or_default(),
			Err(_) => HashMap::new(),
		};
		let owed = facts::carry_over(
			decision.safe.iter().chain(&decision.held),
			&report.conflicts,
			&prep.holds.held_remote,
			&failures,
		);
		if let Some(changes) = self.existing_pair_changes(pair).await {
			changes.note_owed(owed);
		}
		self.carried.lock().await.insert(
			pair,
			PairCarry {
				facts: prep.facts.clone(),
				// A change-scoped pass makes neither judgement for itself; what it carries on is
				// what the last whole read found, which is what it planned with.
				remote_converged: prep.remote_converged,
				scan_complete: prep.local_scan.complete,
			},
		);
	}

	/// The body of [`sync_pass`](Self::sync_pass), reporting to an observer whose panics are
	/// already contained.
	async fn run_pass(
		&self,
		pair: PairId,
		observer: &mut (dyn FnMut(SyncEvent) + Send),
		when_idle: WhenIdle,
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
		// Both changelists, taken BEFORE either side is read: everything in them is either already
		// in the snapshot this pass is about to read (a harmless duplicate) or not yet, and this
		// pass has it. What arrives from here on belongs to the next pass (see `changes`).
		let changes = self.pair_changes(pair).await;
		let mut scope = changes.take();
		let (dirty_local, dirty_remote) = scope.sizes();
		// Nothing announced on either side, nothing owed by the last plan, and nothing forcing a
		// whole read: there is no pass to run. A watch wake that lands here does nothing at all —
		// no `PassStarted`, and the safety net goes on measuring from the last whole read — because
		// reading two trees to discover there was nothing to do is the cost this scoping exists to
		// remove. An explicit `sync_once` still runs: its caller asked for a pass and wants the
		// report, and may well have beaten its own watcher's event.
		if when_idle == WhenIdle::Skip && scope.is_idle() {
			tracing::debug!(
				"sync_once[pair {pair}]: nothing announced since the last pass — no pass run"
			);
			return Ok(SyncReport::default());
		}
		// What reading both sides costs this pair — measured over the read and nothing else, since
		// the watch's safety net scales its interval by it (see `SyncReport::read_cost`).
		let read_started = Instant::now();
		let Some((recording, prepared)) = gate
			.guard(async move {
				let recording = reading.lock_owned().await;
				let prepared = self.prepare(pair, Some(&mut scope)).await;
				(recording, prepared)
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
		// The read ends here: everything above it is the baseline, the scan, the snapshot and the
		// view; everything below writes something.
		let read_cost = read_started.elapsed();
		let mut prep = prepared?;
		let store = self.pair_store(pair).await?;
		// Persist what this pass confirmed. Only a real pass writes it: `plan_pair` stays a pure
		// read, so a dry run inside the confirmation window just leaves it for the next pass — and
		// leaves the evidence with it, which is why the records are retired HERE and not in the
		// reading step.
		let confirmed = mem::take(&mut prep.confirmed);
		let confirmed = if confirmed.is_empty() {
			confirmed
		} else {
			// One transaction for the lot: pass two of a first sync confirms every row it pushed,
			// and a transaction per row is a transaction per item of the tree — which is also why
			// it does not run on a runtime thread. The rows come back out of the closure, since
			// the push records they retire are named by them.
			let (written, confirmed) = off_store(&store, move |store| {
				let changes: Vec<BaselineChange<'_>> =
					confirmed.iter().map(BaselineChange::Upsert).collect();
				(store.apply_changes(pair, &changes), confirmed)
			})
			.await?;
			written.map_err(|e| db_error(e, "recording the confirmed agreed content"))?;
			confirmed
		};
		self.forget_settled_pushes(&confirmed);
		// One decision in one place: the changelists' own reasons plus the two facts only the pass
		// knows. `prepare` above has already acted on it — a pass with no reason read only what its
		// changelists named — and the report carries it on, for the safety net and for the caller.
		let mut report = SyncReport {
			full_pass: prep.read.full_pass_reason(),
			read_cost,
			..SyncReport::default()
		};
		tracing::debug!(
			"sync_once[pair {pair}]: {dirty_local} local path(s) and {dirty_remote} remote change(s) announced since the last pass; {}",
			match &prep.read {
				PassRead::Scoped(decided) => format!(
					"this pass read only what changed, and decided {} path(s)",
					decided.len()
				),
				PassRead::Whole(reason) => format!(
					"this pass read everything ({})",
					reason.map_or_else(|| "a dry run".to_string(), |reason| reason.to_string())
				),
			},
		);
		// Only a whole read measures the tree, so only a whole read re-scales the changelist caps
		// to it; a change-scoped pass never saw all of it.
		if !prep.read.is_scoped() {
			changes.note_tree_size(prep.baseline.len());
		}

		tracing::debug!(
			"sync_once[pair {pair}]: mode {:?} — local scan {} node(s) (complete={}), remote view {} node(s) (converged={})",
			prep.record.mode,
			prep.local_scan.nodes.of(&prep.baseline).len(),
			prep.local_scan.complete,
			prep.remote_view.nodes.of(&prep.baseline).len(),
			prep.remote_converged,
		);
		observer(SyncEvent::PassStarted {
			mode: prep.record.mode,
		});

		report.unsyncable = prep.unsyncable();
		report.ignored = prep.ignored();
		// Before the refusal check, so a refused pass still says what the scan could not read.
		report.errors.extend(prep.local_scan.reported_errors());
		report.errors.append(&mut prep.remote_rule_errors);

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
		report.dropped_actions = screened.dropped;

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
				&store,
				pair,
				rel_path,
				prep.local_scan
					.nodes
					.of(&prep.baseline)
					.at(rel_path)
					.as_deref(),
				prep.remote_view
					.nodes
					.of(&prep.baseline)
					.at(rel_path)
					.as_deref(),
			)
			.await
			{
				// The conflict is not held, so the next pass surfaces it again — but a pass whose
				// record did not land is a failed pass, not a healthy one that mentions a problem.
				report.errors.push(format!("{rel_path}: {error}"));
				report.store_failed |= apply::record_not_written(&error);
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
		// Before the early return below: a pass with nothing to apply still made a plan, and still
		// owes the next one everything that plan named.
		self.carry_forward(pair, &prep, &report, &decision).await;

		// The return below hands back the LOCAL half alone: `note_owed_remote` sits after the
		// apply, since `report.interrupted` is what gates it and nothing has applied anything yet.
		// That is the shape this stage's first attempt was reverted for, and it is safe here only
		// because a pass with an empty `safe` list either planned nothing at all or had every
		// action held or dropped — a hold sets `decision.reason`, which `next_pass_scope` reads as
		// `DeletionHold`, and a drop counts in `dropped_actions`, which it reads as
		// `UnappliedWork`. Both send the next pass to a whole read, which needs no hand-back. A
		// change that let an action reach `held` without a reason would break that silently.
		if decision.safe.is_empty() {
			tracing::debug!(
				"sync_once[pair {pair}]: nothing to apply ({} deletion(s) held, {} conflict(s), {} path(s) deferred)",
				report.held_deletions(),
				report.conflicts.len(),
				report.deferred_paths,
			);
			self.untrack_ignored(pair, &prep, &mut report).await;
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
			store: &store,
			local: &prep.local_scan.nodes.of(&prep.baseline),
			baseline: &prep.baseline,
			remote: &prep.remote_view.nodes.of(&prep.baseline),
			root_remote,
			absence_trusted: state.absence_trusted(),
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
		// Cut short mid-apply: hand the next pass BOTH halves of what this one took. `carry_forward`
		// above recorded the paths the plan named; this puts the announced changes back with them,
		// and without them the next pass would derive those paths' remote side from baseline rows
		// that still say the item is there — losing every action owed to a remote absence (see
		// `next_pass_scope` and `changes`'s module docs). A whole read hands back an empty delta,
		// and forces a whole read instead.
		if report.interrupted > 0
			&& let Some(changes) = self.existing_pair_changes(pair).await
		{
			changes.note_owed_remote(mem::take(&mut prep.remote_delta));
		}
		// After the apply, which carries rows along with the directory moves the roots were re-keyed by.
		self.untrack_ignored(pair, &prep, &mut report).await;
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

/// What a finished pass's outcome means for the NEXT pass's scope: the rows of the full-pass
/// trigger table that are facts about the pass before it (see
/// [`FullPassReason`](super::FullPassReason)). `None` leaves the next pass to its changelists.
///
/// Decided purely so the policy is unit-testable. The rule behind the rows: a pass consumed both
/// changelists to make its plan, so anything it planned and did NOT carry out is in no list any
/// more and only a whole-tree read finds it again — unless the pass handed both lists back, which
/// is the one row that reads `full_pass` rather than the outcome alone.
fn next_pass_scope(report: &SyncReport) -> Option<FullPassReason> {
	if report.refused.is_some() {
		return Some(FullPassReason::PreviousRefusal);
	}
	// Either shape of a cut-short pass, and they part company here.
	//
	// A cancel that landed before there was a plan (`paused`) took both changelists and has
	// nothing to show for them: no plan named the paths, and the lists went with the pass.
	//
	// A plan abandoned part-way (`interrupted`) does: `carry_forward` recorded the paths it named
	// and the apply handed its announced changes back (`note_owed_remote`), so the next pass
	// re-plans what this one did not reach from the same evidence. That holds only for a pass that
	// PLANNED from its changelists — one that read both sides whole planned from a snapshot and a
	// walk, which no list can hand back, so its remainder still wants a whole read. `full_pass` is
	// exactly that question: `None` is a change-scoped pass (a dry run reads it too, and never
	// reaches this).
	if report.paused || (report.interrupted > 0 && report.full_pass.is_some()) {
		return Some(FullPassReason::InterruptedPass);
	}
	// A pass that planned work and did not apply it: the drive-write lock it could not take (its
	// whole plan), a side that filled up (the transfers held behind it), a record of work already
	// done that did not land (nothing more is admitted after it), a single action that failed, or an
	// action screened out before it ran (`drop_blocked`, `withhold_deletions_over_unreachable`).
	// Each leaves paths owed with nothing left holding them: a dropped action's evidence was an
	// observation or an announcement this pass consumed, and a derived map rebuilds neither — the
	// baseline row it would derive from is the one that says the change never happened.
	if report.lock_failed
		|| report.store_failed
		|| report.halted.is_some()
		|| !report.failed_paths.is_empty()
		|| report.dropped_actions > 0
	{
		return Some(FullPassReason::UnappliedWork);
	}
	// EVERY deletion hold, including the volume threshold and the first-sync hold: an approval
	// names one exact batch, and the pass that offers it again has to reproduce it from the same
	// absences — which were observations the holding pass consumed (see
	// [`FullPassReason::DeletionHold`]).
	if report.guard.is_some() {
		return Some(FullPassReason::DeletionHold);
	}
	None
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
	/// Actions this pass planned and then dropped (see [`SyncReport::dropped_actions`]).
	dropped: usize,
}

/// Reconcile the prepared inputs and screen deletions through the guard, splitting out conflicts.
fn reconcile_and_screen(prep: &Prepared, state: guard::ScreenState) -> Screened {
	let plan = plan::reconcile(
		prep.record.mode,
		&prep.baseline,
		&prep.local_scan.nodes.of(&prep.baseline),
		&prep.remote_view.nodes.of(&prep.baseline),
		&prep.holds,
		prep.read.paths(),
	);
	// The directory moves run before everything else in the plan, which already names their
	// subtrees by the paths they move to. Copied: the dry run plans from the same borrowed `Prepared`.
	let mut actions = prep.dir_moves.clone();
	actions.extend(plan.actions);
	// Both filters only ever REMOVE actions, so the difference is what this pass planned and will
	// not carry out. The next pass has to read everything to find those paths again: the evidence
	// that planned them — a local observation, an announced remote change — was consumed here.
	let planned = actions.len();
	let actions = drop_blocked(actions, &prep.blocked_paths(), &prep.ignored_roots());
	let (actions, over_ignored) = withhold_deletions_over_unreachable(
		actions,
		&prep.facts.unknown_remote,
		&prep.holds.held_remote,
		[&prep.facts.ignored_local, &prep.facts.ignored_remote],
		&prep.blocked_rules(),
	);
	let dropped = planned - actions.len();
	let deferred_paths = plan.deferred_paths + over_ignored;
	let actions = creates_before_dir_moves(actions);
	let (conflict_actions, executable): (Vec<_>, Vec<_>) = actions
		.into_iter()
		.partition(|a| matches!(a, SyncAction::Conflict { .. }));
	let conflicts = conflict_actions
		.iter()
		.map(|a| {
			planned_conflict(
				a.rel_path(),
				&prep.local_scan.nodes.of(&prep.baseline),
				&prep.remote_view.nodes.of(&prep.baseline),
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
	// A path that stops being ignored has no rows and syncs like a first sync, whose deletions are held
	// for approval: the rows the pair tracks elsewhere say nothing about what sits there.
	let unignored = prep.unignored();
	let first_sync = state.first_sync
		|| (!unignored.is_empty()
			&& executable.iter().any(|action| {
				let path = action.rel_path();
				action.is_delete()
					&& path
						.match_indices('/')
						.map(|(i, _)| &path[..i])
						.chain([path])
						.any(|at| unignored.contains(at))
			}));
	let decision = guard::screen(
		executable,
		guard::ScreenState {
			first_sync,
			..state
		},
		prep.record.delete_guard,
	);
	let pass_token = (!decision.held.is_empty()).then(|| deletion_batch_token(&decision.held));
	Screened {
		conflicts,
		all,
		decision,
		pass_token,
		deferred_paths,
		dropped,
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
///
/// A move whose SOURCE is at or under an `ignored` root (a subset of `blocked`) goes too: the tracked
/// copy there is no longer synced, so it must not be moved away. Nothing is stranded by that, since
/// the deletions at the source are blocked already, and whether a directory above it may still be
/// deleted is for [`withhold_deletions_over_unreachable`] to decide.
fn drop_blocked(
	actions: Vec<SyncAction>,
	blocked: &BTreeSet<String>,
	ignored: &BTreeSet<String>,
) -> Vec<SyncAction> {
	if blocked.is_empty() && ignored.is_empty() {
		return actions;
	}
	// `""` is the pair root, whose rules can be unreadable too: everything is under it. Every other
	// root is found by walking the path's own ancestors, rather than reading every root per action.
	let at_or_under = |roots: &BTreeSet<String>, path: &str| {
		roots.contains("") || plan::at_or_under_root(roots, path)
	};
	// The sources of the moves this dropped: their content is staying exactly where it is.
	let mut stranded: Vec<String> = Vec::new();
	let kept: Vec<SyncAction> = actions
		.into_iter()
		.filter(|action| {
			let path = action.rel_path();
			if let SyncAction::MoveRemote { from_path, .. }
			| SyncAction::MoveLocal { from_path, .. } = action
				&& at_or_under(ignored, from_path)
			{
				tracing::debug!(
					"reconcile: skipping {} — it moves an ignored item",
					action.describe()
				);
				return false;
			}
			if !at_or_under(blocked, path) {
				return true;
			}
			tracing::debug!(
				"reconcile: skipping {} — its path is blocked (a name the remote rejects, \
				 {MAX_PATH_FAILURES} consecutive failures, or an ignore rule)",
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

/// Drop a deletion of a directory above a path whose item still exists where this pass cannot act on
/// it, and the create a type flip pairs with it at that path (whose own stash or overwrite would take
/// the directory just the same). Deletions are recursive; this is the per-path plan of a directory
/// move the fold could not make, or of a directory deleted around content nothing may delete.
///
/// - Under a path the cache is holding mid-transition (`held`), both sides wait: the reconcile
///   deferred that path, and the next pass reads it settled.
/// - Under a synced path whose remote item is out of the view (`unknown`), only a LOCAL deletion
///   waits, for as long as the item stays out of reach: the local copy is the only readable one. A
///   remote trash there still runs — the local side already let go of its copy, and a trash is
///   recoverable.
/// - Over an item a user-level or `.filenignore` rule hides on the side that would delete
///   (`[ignored_local, ignored_remote]`), the deletion waits for as long as the rule stands: ignored
///   content is never deleted, so the directory stays, holding only it. Content only the built-in
///   defaults hide does not keep a directory, or every folder deleted elsewhere would survive on
///   each Mac for its `.DS_Store`. Returned as the second value, counted as deferred.
/// - Over a subtree whose ignore rules could not be read (`ignore_blocked`), on either side, the
///   same: what those rules hide cannot be told apart from what they do not.
///
/// A deletion around a path blocked for any other reason (a rejected name, an alias, a park) runs as
/// it did, into the recoverable quarantine or trash.
fn withhold_deletions_over_unreachable(
	actions: Vec<SyncAction>,
	unknown: &BTreeMap<String, UnsyncableReason>,
	held: &BTreeSet<String>,
	[ignored_local, ignored_remote]: [&BTreeMap<String, IgnoreDecision>; 2],
	ignore_blocked: &BTreeSet<String>,
) -> (Vec<SyncAction>, usize) {
	// By level alone: which line matched says nothing about whether the content must be kept. Each
	// question is "is anything under this directory", which a sorted map answers by seeking to it.
	let keeps_ignored = |ignored: &BTreeMap<String, IgnoreDecision>, dir: &str| {
		ignore_blocked
			.range(plan::subtree_bounds(dir))
			.next()
			.is_some()
			|| plan::under_dir(ignored, dir)
				.any(|(_, decision)| decision.level != IgnoreLevel::Default)
	};
	let mut over_ignored = 0;
	// The `.filenignore` files whose rules keep a withheld directory. Deleted, they would leave the
	// next pass no rule there, and the ignored content would go down with the directory after all.
	let mut kept_rule_files = Vec::new();
	let mut withheld: BTreeSet<String> = actions
		.iter()
		.filter(|action| {
			let dir = action.rel_path();
			let above_held = held.range(plan::subtree_bounds(dir)).next().is_some();
			let (unreachable, ignored, side) = match action {
				SyncAction::DeleteLocal { .. } => (
					above_held || plan::under_dir(unknown, dir).next().is_some(),
					keeps_ignored(ignored_local, dir),
					ignored_local,
				),
				SyncAction::TrashRemote { .. } => (
					above_held,
					keeps_ignored(ignored_remote, dir),
					ignored_remote,
				),
				_ => return false,
			};
			if ignored {
				kept_rule_files.extend(side.values().filter_map(
					|decision| match &decision.level {
						IgnoreLevel::File { dir: rules }
							if rules == dir || plan::is_under(rules, dir) =>
						{
							Some(Origin::File { dir: rules }.to_string())
						}
						_ => None,
					},
				));
				if !unreachable {
					over_ignored += 1;
				}
			}
			unreachable || ignored
		})
		.map(|action| action.rel_path().to_string())
		.collect();
	withheld.extend(kept_rule_files);
	if withheld.is_empty() {
		return (actions, over_ignored);
	}
	let kept = actions
		.into_iter()
		.filter(|action| {
			let waits =
				(action.is_delete() || action.is_create()) && withheld.contains(action.rel_path());
			if waits {
				tracing::debug!(
					"reconcile: skipping {} — the directory holds an item this pass cannot act on",
					action.describe()
				);
			}
			!waits
		})
		.collect();
	(kept, over_ignored)
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
	use std::{
		collections::HashSet,
		sync::atomic::{AtomicBool, AtomicUsize, Ordering},
	};

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
			ignore::IgnoreRules,
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

	/// The rows of the full-pass trigger table that are facts about the PREVIOUS pass: a refusal is
	/// whole-tree state, a cut-short pass that read both sides whole planned from evidence no
	/// changelist holds — a change-scoped one hands its two lists back instead — so did a pass
	/// that planned work it could not carry out, and so did every deletion hold.
	#[test]
	fn a_passs_outcome_decides_whether_the_next_one_reads_everything() {
		assert_eq!(
			next_pass_scope(&SyncReport::default()),
			None,
			"a pass that ran to the end leaves the next one to its changelists"
		);
		assert_eq!(
			next_pass_scope(&SyncReport {
				dropped_actions: 1,
				..SyncReport::default()
			}),
			Some(FullPassReason::UnappliedWork),
			"an action the pass planned and dropped is named in no changelist any more"
		);
		assert_eq!(
			next_pass_scope(&SyncReport {
				refused: Some(RefuseReason::LocalCollision),
				..SyncReport::default()
			}),
			Some(FullPassReason::PreviousRefusal)
		);
		assert_eq!(
			next_pass_scope(&SyncReport {
				paused: true,
				..SyncReport::default()
			}),
			Some(FullPassReason::InterruptedPass),
			"a cancel that beat the plan still consumed the changelists"
		);
		assert_eq!(
			next_pass_scope(&SyncReport {
				interrupted: 3,
				full_pass: Some(FullPassReason::SafetyNet),
				..SyncReport::default()
			}),
			Some(FullPassReason::InterruptedPass),
			"a whole read's evidence is in no changelist, so its remainder needs another one"
		);
		assert_eq!(
			next_pass_scope(&SyncReport {
				interrupted: 3,
				..SyncReport::default()
			}),
			None,
			"a change-scoped pass hands both its lists back, so the next one re-plans from them"
		);
		assert_eq!(
			next_pass_scope(&SyncReport {
				interrupted: 3,
				failed_paths: vec![("a.txt".to_string(), "boom".to_string())],
				..SyncReport::default()
			}),
			Some(FullPassReason::UnappliedWork),
			"an action that FAILED is a debt of its own, and still wants a whole read"
		);

		// EVERY deletion hold: three want evidence only a whole-tree read supplies, and the other
		// two have to be able to offer the caller the very same batch again under the same token,
		// which means reproducing absences the holding pass consumed.
		for reason in [
			GuardReason::ScanIncomplete,
			GuardReason::RemoteUnconverged { deletions: 2 },
			GuardReason::RemoteEmptied { deletions: 2 },
			GuardReason::ExceededThreshold {
				deletions: 40,
				limit: 10,
			},
			GuardReason::FirstSyncWithDeletions { deletions: 4 },
		] {
			assert_eq!(
				next_pass_scope(&SyncReport {
					guard: Some(reason.clone()),
					..SyncReport::default()
				}),
				Some(FullPassReason::DeletionHold),
				"{reason:?}"
			);
		}

		// A pass that made a plan and did not apply it, in each of the four shapes. Every one of
		// them drained the changelists first, so the paths are owed and nothing else holds them.
		for (label, report) in [
			(
				"a drive lock it could not take",
				SyncReport {
					lock_failed: true,
					..SyncReport::default()
				},
			),
			(
				"a side that filled up",
				SyncReport {
					halted: Some(crate::sync_engine::HaltReason::LocalStorageFull),
					..SyncReport::default()
				},
			),
			(
				"a record that did not land",
				SyncReport {
					store_failed: true,
					..SyncReport::default()
				},
			),
			(
				"an action that failed",
				SyncReport {
					failed_paths: vec![("a.txt".to_string(), "boom".to_string())],
					..SyncReport::default()
				},
			),
		] {
			assert_eq!(
				next_pass_scope(&report),
				Some(FullPassReason::UnappliedWork),
				"{label}"
			);
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
		let kept: Vec<String> = drop_blocked(actions.clone(), &blocked, &BTreeSet::new())
			.iter()
			.map(|a| a.rel_path().to_string())
			.collect();
		assert_eq!(kept, vec!["ok.txt", "badly.txt", "badge/y.txt"]);

		// Nothing blocked -> the plan is untouched.
		assert_eq!(
			drop_blocked(actions.clone(), &BTreeSet::new(), &BTreeSet::new()),
			actions
		);
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
			None,
		);
		let reconciled = plan::reconcile(
			SyncMode::TwoWay,
			&tree(&baseline),
			&local,
			&view.nodes.whole(),
			&plan::PassHolds::default(),
			plan::PassPaths::Whole,
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

		let (unknown, never_synced) = plan::unknown_remote_paths(&tree(&baseline), &view.skipped);
		assert_eq!(
			unknown,
			BTreeMap::from([("doc.txt".to_string(), UnsyncableReason::RemoteUndecodable)])
		);
		assert!(never_synced.is_empty(), "{never_synced:?}");
		assert!(
			drop_blocked(
				reconciled.actions,
				&unknown.keys().cloned().collect(),
				&BTreeSet::new()
			)
			.is_empty(),
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
		let baseline = Baseline::from_rows(baseline.into_values());
		let (unknown_remote, never_synced_remote) =
			plan::unknown_remote_paths(&baseline, &remote_view.skipped);
		Prepared {
			read: PassRead::Whole(Some(FullPassReason::SafetyNet)),
			remote_delta: Vec::new(),
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
				ignored: BTreeMap::new(),
				ignored_default_untracked: 0,
				ignore_blocked: BTreeSet::new(),
			},
			holds: plan::PassHolds {
				trashed: HashSet::new(),
				held_remote: remote_view.held_paths.clone(),
			},
			remote_view,
			remote_converged: true,
			remote_emptied: false,
			failures,
			facts: PairFacts {
				unknown_remote,
				never_synced_remote,
				..PairFacts::default()
			},
			remote_blocked: BTreeSet::new(),
			remote_rule_errors: Vec::new(),
			last_ignored: BTreeSet::new(),
			confirmed: Vec::new(),
		}
	}

	/// The assembly self-check of plan 3.6: a map that accounts for the rows it carried and the
	/// nodes its observations found is accepted, and one that lost a path NO observation covered —
	/// the bookkeeping error that reads downstream as a deletion — is refused, which sends the
	/// pass to a whole read instead.
	#[test]
	fn the_assembly_check_catches_a_path_no_observation_dropped() {
		let hash = Blake3Hash::from([7u8; 32]);
		let baseline = Baseline::from_rows([
			dir_row("docs", Uuid::new_v4()),
			file_row("docs/a.txt", Uuid::new_v4(), hash),
			file_row("keep.txt", Uuid::new_v4(), hash),
		]);
		let file_node = |rel: &str| LocalNode {
			rel_path: rel.to_string(),
			kind: NodeKind::File,
			size: 4,
			mtime_millis: 1,
			content_hash: Some(hash),
		};
		// One observation: a COMPLETE walk of `docs`, which found both of its rows again.
		let scan = LocalScan {
			nodes: [local_dir("docs"), file_node("docs/a.txt")]
				.into_iter()
				.map(|node| (node.rel_path.clone(), node))
				.collect(),
			complete: true,
			errors: Vec::new(),
			invalid_names: BTreeMap::new(),
			aliased_dirs: BTreeMap::new(),
			ignored: BTreeMap::new(),
			ignored_default_untracked: 0,
			ignore_blocked: BTreeSet::new(),
		};
		let observed = LocalObservations {
			observed: BTreeMap::from([("docs".to_string(), LocalObservation::Dir(Box::new(scan)))]),
			siblings: BTreeMap::new(),
			ignore_blocked: BTreeSet::new(),
			complete: true,
			errors: Vec::new(),
		};
		let none = BTreeSet::new();
		let held = BTreeSet::from(["keep.txt".to_string()]);
		let assembled = |paths: &[&str]| Derived {
			local: paths
				.iter()
				.map(|rel| ((*rel).to_string(), file_node(rel)))
				.collect(),
			remote: Side::default(),
			dirty: BTreeSet::new(),
			decided: BTreeSet::new(),
			held: BTreeSet::new(),
		};

		assert!(
			assembly_accounted(
				&baseline,
				&assembled(&["docs", "docs/a.txt", "keep.txt"]),
				&observed,
				&none
			),
			"the walk replaced its own two rows and the third was carried: that is the whole tree"
		);
		assert!(
			!assembly_accounted(
				&baseline,
				&assembled(&["docs", "docs/a.txt"]),
				&observed,
				&none
			),
			"a row outside every observation went missing and the check passed it — that is the \
			 shape that reads as a deletion"
		);
		// The same map, with `keep.txt` HELD instead: a held row is in no map on purpose, and the
		// count of them is what tells the two apart. `keep.txt` gets no observation at all — the
		// shape a hidden path or a path with no reading takes — so nothing else accounts for it.
		assert!(
			assembly_accounted(
				&baseline,
				&assembled(&["docs", "docs/a.txt"]),
				&observed,
				&held
			),
			"a row this pass holds is in neither map by design"
		);
		assert!(
			!assembly_accounted(
				&baseline,
				&assembled(&["docs", "docs/a.txt", "keep.txt"]),
				&observed,
				&held
			),
			"a held row that is in the map anyway means the two do not describe the same tree"
		);
		assert!(
			!assembly_accounted(
				&baseline,
				&assembled(&["docs", "docs/a.txt", "keep.txt", "invented.txt"]),
				&observed,
				&none
			),
			"a path no row and no observation named appeared in the map"
		);
	}

	/// A hidden observation takes its rows out of the local map (`derive::merge_local`), so the
	/// check has to expect them gone. Counting a `Hidden` as "replaces nothing" would send every
	/// pass that re-observes a hidden path with rows still behind it to a whole read — and would
	/// pass a map that kept them, which is the pair of maps that plans a delete there.
	#[test]
	fn the_assembly_check_expects_a_hidden_path_to_take_its_rows() {
		let hash = Blake3Hash::from([7u8; 32]);
		let baseline = Baseline::from_rows([
			dir_row("logs", Uuid::new_v4()),
			file_row("logs/a.txt", Uuid::new_v4(), hash),
			file_row("keep.txt", Uuid::new_v4(), hash),
		]);
		let observed = LocalObservations {
			observed: BTreeMap::from([(
				"logs".to_string(),
				LocalObservation::Hidden(IgnoreDecision {
					level: IgnoreLevel::User,
					pattern: "logs/".to_string(),
				}),
			)]),
			siblings: BTreeMap::new(),
			ignore_blocked: BTreeSet::new(),
			complete: true,
			errors: Vec::new(),
		};
		let none = BTreeSet::new();
		let assembled = |paths: &[&str]| Derived {
			local: paths
				.iter()
				.map(|rel| ((*rel).to_string(), local_file(rel, hash)))
				.collect(),
			remote: Side::default(),
			dirty: BTreeSet::new(),
			decided: BTreeSet::new(),
			held: BTreeSet::new(),
		};

		assert!(
			assembly_accounted(&baseline, &assembled(&["keep.txt"]), &observed, &none),
			"the hidden root took its own row and the one under it"
		);
		assert!(
			!assembly_accounted(
				&baseline,
				&assembled(&["keep.txt", "logs/a.txt"]),
				&observed,
				&none
			),
			"a row under a hidden root that stayed in the map is a map the view no longer agrees \
			 with"
		);
	}

	/// A row this pass HOLDS under an observed path is not a row that observation replaced: it was
	/// never in the map to be taken out of it. Counting it as both — held, and replaced — puts the
	/// bound below the map the derivation legitimately assembled, and the pass falls back to a whole
	/// read at every hidden root, dirty directory or absence with a half-written row under it.
	#[test]
	fn the_assembly_check_does_not_subtract_a_held_row_twice() {
		let hash = Blake3Hash::from([7u8; 32]);
		let baseline = Baseline::from_rows([
			dir_row("logs", Uuid::new_v4()),
			file_row("logs/a.txt", Uuid::new_v4(), hash),
			file_row("keep.txt", Uuid::new_v4(), hash),
		]);
		// `logs/a.txt` records one side only, so `derive::carried` refused it: it is in neither map,
		// it is held — and it sits under the root the observation below covers.
		let held = BTreeSet::from(["logs/a.txt".to_string()]);
		let observed = LocalObservations {
			observed: BTreeMap::from([(
				"logs".to_string(),
				LocalObservation::Hidden(IgnoreDecision {
					level: IgnoreLevel::User,
					pattern: "logs/".to_string(),
				}),
			)]),
			siblings: BTreeMap::new(),
			ignore_blocked: BTreeSet::new(),
			complete: true,
			errors: Vec::new(),
		};
		let assembled = Derived {
			local: [("keep.txt".to_string(), local_file("keep.txt", hash))]
				.into_iter()
				.collect(),
			remote: Side::default(),
			dirty: BTreeSet::new(),
			decided: BTreeSet::new(),
			held: BTreeSet::new(),
		};

		assert!(
			assembly_accounted(&baseline, &assembled, &observed, &held),
			"the hidden root took the one row it had in the map, and the held row was never in one"
		);
	}

	/// The decided set's own check. A key in either map that no baseline row put there came from a
	/// producer, and a producer that did not record it leaves a key the narrowed reconcile never
	/// visits — no action at it, and nothing downstream that can tell that from agreement.
	#[test]
	fn the_decided_check_catches_a_key_no_producer_recorded() {
		let hash = Blake3Hash::from([9u8; 32]);
		let baseline = Baseline::from_rows([file_row("docs/a.txt", Uuid::new_v4(), hash)]);
		// A local producer's key: the node the observation found, off the path its row records.
		let moved: Side<LocalNode> = Side::from(HashMap::from([(
			"docs/moved.txt".to_string(),
			local_file("docs/moved.txt", hash),
		)]));
		// And a remote one: a path the delta placed that no row names.
		let placed: Side<RemoteNode> = Side::from(HashMap::from([(
			"docs/new".to_string(),
			remote_dir("docs/new", Uuid::new_v4()),
		)]));
		let empty_local: Side<LocalNode> = Side::default();
		let empty_remote: Side<RemoteNode> = Side::default();

		assert_eq!(
			unaccounted_key(&baseline, &moved, &empty_remote, &BTreeSet::new()).as_deref(),
			Some("docs/moved.txt"),
			"a local key nothing recorded"
		);
		assert_eq!(
			unaccounted_key(&baseline, &empty_local, &placed, &BTreeSet::new()).as_deref(),
			Some("docs/new"),
			"and the same on the remote side"
		);
		assert_eq!(
			unaccounted_key(
				&baseline,
				&moved,
				&placed,
				&BTreeSet::from(["docs/moved.txt".to_string(), "docs/new".to_string()])
			),
			None,
			"a producer that records what it moved is what the check is written for"
		);
		assert_eq!(
			unaccounted_key(
				&baseline,
				&Side::from(HashMap::from([(
					"docs/a.txt".to_string(),
					local_file("docs/a.txt", hash)
				)])),
				&empty_remote,
				&BTreeSet::new()
			),
			None,
			"a carried row needs no record: `from_baseline` only ever inserts at a row's path"
		);

		// And the same asked of a CARRIED side, which is where this check now runs. Only the
		// overlay is walked: the rows such a side derives answer for themselves and are no
		// producer's doing, which is what makes the check cost the change and not the tree.
		let mut produced: Side<LocalNode> = Side::carried();
		produced.insert(
			"docs/moved.txt".to_string(),
			local_file("docs/moved.txt", hash),
		);
		assert_eq!(
			unaccounted_key(&baseline, &produced, &empty_remote, &BTreeSet::new()).as_deref(),
			Some("docs/moved.txt"),
			"the overlay is where every producer writes, so that is where an unrecorded key is"
		);
		let mut at_a_row: Side<LocalNode> = Side::carried();
		at_a_row.insert("docs/a.txt".to_string(), local_file("docs/a.txt", hash));
		assert_eq!(
			unaccounted_key(&baseline, &at_a_row, &empty_remote, &BTreeSet::new()),
			None,
			"a key at a row's own path is no invention, whichever side holds it"
		);
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
			ignored: BTreeMap::new(),
			ignored_default_untracked: 0,
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
					path_is_dir: true,
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
				path_is_dir: false,
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
		prep.facts
			.invalid_names
			.insert("docs/CON".to_string(), "reserved".to_string());
		prep.facts
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
				path_is_dir: true,
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

	const MODES: [SyncMode; 5] = [
		SyncMode::TwoWay,
		SyncMode::LocalToRemote,
		SyncMode::RemoteToLocal,
		SyncMode::LocalBackup,
		SyncMode::RemoteBackup,
	];

	/// An ignore decision at `level` whose deciding line is `pattern`.
	fn hidden_by(level: IgnoreLevel, pattern: &str) -> IgnoreDecision {
		IgnoreDecision {
			level,
			pattern: pattern.to_string(),
		}
	}

	/// A decision by the `pattern` line of the root `.filenignore`.
	fn by_root_file(pattern: &str) -> IgnoreDecision {
		hidden_by(IgnoreLevel::File { dir: String::new() }, pattern)
	}

	/// `docs/` synced and now ignored by the root `.filenignore`, on both sides.
	fn ignoring_docs(prep: &mut Prepared) {
		let decision = by_root_file("docs/");
		prep.facts
			.ignored_local
			.insert("docs".to_string(), decision.clone());
		prep.facts
			.ignored_remote
			.insert("docs".to_string(), decision);
	}

	/// The baseline rows a pass that found `roots` ignored leaves behind.
	fn untracked(
		mut baseline: HashMap<String, BaselineEntry>,
		roots: &BTreeSet<String>,
	) -> HashMap<String, BaselineEntry> {
		baseline.retain(|path, _| {
			!roots
				.iter()
				.any(|root| path == root || plan::is_under(path, root))
		});
		baseline
	}

	/// A synced directory that becomes ignored plans nothing in any mode, whether the two sides hide
	/// it or still show a change under it: a local deletion and a remote edit are both left alone,
	/// and the pass untracks exactly that directory.
	#[test]
	fn a_synced_directory_that_becomes_ignored_plans_nothing_in_any_mode() {
		let docs = Docs::new();
		let edited_x = remote_file("docs/x.bin", Uuid::new_v4(), Blake3Hash::from([9; 32]), 4);
		for mode in MODES {
			let hidden = prepared(
				mode,
				docs.baseline(),
				Vec::new(),
				view(Vec::new(), Vec::new(), &[]),
				HashMap::new(),
			);
			let changed = prepared(
				mode,
				docs.baseline(),
				vec![local_dir("docs"), local_file("docs/x.bin", docs.x_hash)],
				view(
					vec![
						remote_dir("docs", docs.dir),
						remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
						edited_x.clone(),
					],
					Vec::new(),
					&[],
				),
				HashMap::new(),
			);
			for (what, mut prep) in [("hidden", hidden), ("changed", changed)] {
				ignoring_docs(&mut prep);
				assert_eq!(
					prep.ignored_roots(),
					BTreeSet::from(["docs".to_string()]),
					"{mode:?} {what}"
				);
				let (actions, unsyncable) = run_prepared(prep);
				assert!(actions.is_empty(), "{mode:?} {what}: {actions:?}");
				assert!(unsyncable.is_empty(), "{mode:?} {what}: {unsyncable:?}");
			}
			// Without the rules the same hidden tree reads as deleted from both sides.
			let (actions, _) = run_prepared(prepared(
				mode,
				docs.baseline(),
				Vec::new(),
				view(Vec::new(), Vec::new(), &[]),
				HashMap::new(),
			));
			assert!(!actions.is_empty(), "{mode:?}");
		}
	}

	/// Once a pass has untracked an ignored directory, removing the rule reads its copies like a first
	/// sync: identical ones are adopted with no transfer in every mode, and differing ones are a
	/// conflict in a two-way pair.
	#[test]
	fn an_unignored_directory_reads_like_a_first_sync() {
		let docs = Docs::new();
		let mut ignored = prepared(
			SyncMode::TwoWay,
			docs.baseline(),
			Vec::new(),
			view(Vec::new(), Vec::new(), &[]),
			HashMap::new(),
		);
		ignoring_docs(&mut ignored);
		let baseline = untracked(docs.baseline(), &ignored.ignored_roots());
		assert!(baseline.is_empty(), "{baseline:?}");

		let remote = |x: RemoteNode| {
			view(
				vec![
					remote_dir("docs", docs.dir),
					remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
					x,
				],
				Vec::new(),
				&[],
			)
		};
		for mode in MODES {
			let (actions, _) = folded_pass(
				mode,
				baseline.clone(),
				docs.local("docs"),
				remote(remote_file("docs/x.bin", docs.x, docs.x_hash, 4)),
				HashMap::new(),
			);
			assert!(
				!actions.is_empty()
					&& actions
						.iter()
						.all(|action| matches!(action, SyncAction::AdoptBaseline { .. })),
				"{mode:?}: {actions:?}"
			);
		}

		let prep = prepared(
			SyncMode::TwoWay,
			baseline,
			docs.local("docs"),
			remote(remote_file(
				"docs/x.bin",
				Uuid::new_v4(),
				Blake3Hash::from([9; 32]),
				4,
			)),
			HashMap::new(),
		);
		let screened = reconcile_and_screen(&prep, screen_state(&prep));
		assert_eq!(
			screened
				.conflicts
				.iter()
				.map(|conflict| conflict.rel_path.as_str())
				.collect::<Vec<_>>(),
			vec!["docs/x.bin"]
		);
	}

	/// The report names the top of each ignored subtree once across both sides, with the line that
	/// hid it, says whether a baseline row sat at or under it, and leaves out what only the defaults
	/// hide unless it was tracked. A sibling that sorts between a directory and its children is still
	/// reported. Where the two sides hide one path with different lines, the local one is reported.
	#[test]
	fn the_report_lists_each_ignored_top_once_and_skips_untracked_defaults() {
		let docs = Docs::new();
		let mut baseline = docs.baseline();
		baseline.insert(
			"old.swp".to_string(),
			file_row("old.swp", Uuid::new_v4(), Blake3Hash::from([3; 32])),
		);
		let mut prep = prepared(
			SyncMode::TwoWay,
			baseline,
			Vec::new(),
			view(Vec::new(), Vec::new(), &[]),
			HashMap::new(),
		);
		prep.facts.ignored_local.extend([
			("docs".to_string(), by_root_file("docs/")),
			("docs b".to_string(), hidden_by(IgnoreLevel::User, "docs ?")),
			(
				"sub/.DS_Store".to_string(),
				hidden_by(IgnoreLevel::Default, ".DS_Store"),
			),
		]);
		prep.facts.ignored_remote.extend([
			// The same path, hidden there by another line: the local side's is the one reported.
			("docs".to_string(), hidden_by(IgnoreLevel::User, "*ocs")),
			(
				"docs/x.bin".to_string(),
				hidden_by(IgnoreLevel::User, "*.bin"),
			),
			("docs b/c".to_string(), hidden_by(IgnoreLevel::User, "c")),
			(
				"old.swp".to_string(),
				hidden_by(IgnoreLevel::Default, "*.swp"),
			),
		]);
		let ignored = |rel_path: &str, decision: IgnoreDecision, tracked: bool| IgnoredPath {
			rel_path: rel_path.to_string(),
			level: decision.level,
			pattern: decision.pattern,
			tracked,
		};
		assert_eq!(
			prep.ignored(),
			vec![
				ignored("docs", by_root_file("docs/"), true),
				ignored("docs b", hidden_by(IgnoreLevel::User, "docs ?"), false),
				ignored("old.swp", hidden_by(IgnoreLevel::Default, "*.swp"), true),
			]
		);
		assert_eq!(
			ignored("docs", by_root_file("docs/"), true).to_string(),
			r#"ignored "docs" (by .filenignore: docs/), no longer synced"#
		);
	}

	/// A directory deleted on one side keeps its own copy on the other while a user or `.filenignore`
	/// rule hides something in it there, and counts that as deferred; its visible children are still
	/// deleted. Content only the built-in defaults hide goes with the directory.
	#[test]
	fn a_deleted_directory_keeps_ignored_children_unless_only_defaults_hide_them() {
		let docs = Docs::new();
		for (level, kept) in [
			(IgnoreLevel::User, true),
			(
				IgnoreLevel::File {
					dir: "docs".to_string(),
				},
				true,
			),
			(IgnoreLevel::Default, false),
		] {
			let remote_side = view(
				vec![
					remote_dir("docs", docs.dir),
					remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
					remote_file("docs/x.bin", docs.x, docs.x_hash, 4),
				],
				Vec::new(),
				&[],
			);
			// Trashed on the remote, ignored content left locally; then deleted locally, ignored
			// content left on the remote.
			let mut pulled = prepared(
				SyncMode::TwoWay,
				docs.baseline(),
				docs.local("docs"),
				view(Vec::new(), Vec::new(), &[]),
				HashMap::new(),
			);
			pulled.facts.ignored_local.insert(
				"docs/node_modules".to_string(),
				hidden_by(level.clone(), "node_modules/"),
			);
			let mut pushed = prepared(
				SyncMode::TwoWay,
				docs.baseline(),
				Vec::new(),
				remote_side,
				HashMap::new(),
			);
			pushed.facts.ignored_remote.insert(
				"docs/node_modules".to_string(),
				hidden_by(level.clone(), "node_modules/"),
			);

			for (side, prep) in [("pulled", pulled), ("pushed", pushed)] {
				let screened = reconcile_and_screen(&prep, screen_state(&prep));
				let actions: Vec<&SyncAction> = screened
					.decision
					.safe
					.iter()
					.chain(&screened.decision.held)
					.collect();
				let deletes = |path: &str| {
					actions
						.iter()
						.any(|action| action.is_delete() && action.rel_path() == path)
				};
				assert_eq!(!deletes("docs"), kept, "{level:?} {side}: {actions:?}");
				assert!(
					deletes("docs/a.txt") && deletes("docs/x.bin"),
					"{level:?} {side}: {actions:?}"
				);
				assert_eq!(
					screened.deferred_paths,
					usize::from(kept),
					"{level:?} {side}"
				);
			}
		}
	}

	/// A path that stops being ignored syncs like a first sync in the one-way modes too: what only the
	/// destination holds there has no row, and its deletion is held for approval as a first sync's
	/// is, although the pair tracks other rows. A path the previous pass did not ignore gets no such
	/// hold, and one a rule still hides plans no deletion at all.
	#[test]
	fn a_deletion_under_a_path_that_stops_being_ignored_is_held_like_a_first_sync() {
		let docs = Docs::new();
		let psd = Blake3Hash::from([7; 32]);
		let docs_remote = |extra: Option<RemoteNode>| {
			let mut nodes = vec![
				remote_dir("docs", docs.dir),
				remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
				remote_file("docs/x.bin", docs.x, docs.x_hash, 4),
			];
			nodes.extend(extra);
			view(nodes, Vec::new(), &[])
		};
		let pushed = || {
			prepared(
				SyncMode::LocalToRemote,
				docs.baseline(),
				docs.local("docs"),
				docs_remote(Some(remote_file("a.psd", Uuid::new_v4(), psd, 4))),
				HashMap::new(),
			)
		};
		let pulled = || {
			let mut local = docs.local("docs");
			local.push(local_file("a.psd", psd));
			prepared(
				SyncMode::RemoteToLocal,
				docs.baseline(),
				local,
				docs_remote(None),
				HashMap::new(),
			)
		};
		let deletes_psd = |screened: &Screened| {
			screened
				.decision
				.safe
				.iter()
				.chain(&screened.decision.held)
				.any(|action| action.is_delete() && action.rel_path() == "a.psd")
		};
		for (side, build) in [
			("pushed", &pushed as &dyn Fn() -> Prepared),
			("pulled", &pulled),
		] {
			let plain = build();
			let screened = reconcile_and_screen(&plain, screen_state(&plain));
			assert!(
				screened.decision.reason.is_none() && deletes_psd(&screened),
				"{side}: {:?}",
				screened.decision
			);

			let mut unignored = build();
			unignored.last_ignored.insert("a.psd".to_string());
			let screened = reconcile_and_screen(&unignored, screen_state(&unignored));
			assert_eq!(
				screened.decision.reason,
				Some(GuardReason::FirstSyncWithDeletions { deletions: 1 }),
				"{side}: {:?}",
				screened.decision
			);
			assert!(
				screened
					.decision
					.held
					.iter()
					.any(|action| action.rel_path() == "a.psd"),
				"{side}"
			);

			let mut still = build();
			still.last_ignored.insert("a.psd".to_string());
			still
				.facts
				.ignored_local
				.insert("a.psd".to_string(), hidden_by(IgnoreLevel::User, "*.psd"));
			still
				.facts
				.ignored_remote
				.insert("a.psd".to_string(), hidden_by(IgnoreLevel::User, "*.psd"));
			let screened = reconcile_and_screen(&still, screen_state(&still));
			assert!(
				screened.decision.reason.is_none() && !deletes_psd(&screened),
				"{side}: {:?}",
				screened.decision
			);
		}
	}

	/// A pass records for the next one every root it ignores, a recorded root it still hides (under a
	/// hidden root, or under rules it could not read), and an un-ignored root only while a deletion
	/// under it is held.
	#[test]
	fn a_pass_records_the_ignored_roots_the_next_pass_needs() {
		let docs = Docs::new();
		let mut prep = prepared(
			SyncMode::LocalToRemote,
			docs.baseline(),
			Vec::new(),
			view(Vec::new(), Vec::new(), &[]),
			HashMap::new(),
		);
		prep.facts
			.ignored_local
			.insert("build".to_string(), hidden_by(IgnoreLevel::User, "build/"));
		prep.facts.ignore_blocked.insert("locked".to_string());
		prep.last_ignored = ["build/cache", "locked/tmp", "held", "released"]
			.map(String::from)
			.into();
		assert_eq!(prep.unignored(), BTreeSet::from(["held", "released"]));
		assert_eq!(
			prep.ignored_roots_to_record(["held/x.psd"]),
			["build", "build/cache", "locked/tmp", "held"]
				.map(String::from)
				.into()
		);
	}

	/// The `.filenignore` whose rules keep a withheld directory stays with it, or the next pass would
	/// have no rule and take the ignored content down with the directory. A rule file above the
	/// directory is not the directory's to keep.
	#[test]
	fn a_withheld_directory_keeps_the_rule_files_that_keep_it() {
		let file_level = |dir: &str, pattern: &str| {
			hidden_by(
				IgnoreLevel::File {
					dir: dir.to_string(),
				},
				pattern,
			)
		};
		let ignored = BTreeMap::from([
			(
				"proj/node_modules".to_string(),
				file_level("proj", "node_modules/"),
			),
			(
				"proj/sub/cache".to_string(),
				file_level("proj/sub", "cache/"),
			),
			("proj/build".to_string(), file_level("", "proj/build/")),
		]);
		let none = BTreeMap::new();
		let paths = [
			("proj/.filenignore", NodeKind::File),
			("proj/sub/.filenignore", NodeKind::File),
			("proj/a.txt", NodeKind::File),
			("proj/sub", NodeKind::Dir),
			("proj", NodeKind::Dir),
			(".filenignore", NodeKind::File),
		];
		for local in [true, false] {
			let actions = paths
				.iter()
				.map(|&(rel_path, kind)| {
					let rel_path = rel_path.to_string();
					if local {
						SyncAction::DeleteLocal { rel_path, kind }
					} else {
						SyncAction::TrashRemote {
							rel_path,
							kind,
							remote_uuid: Uuid::nil(),
						}
					}
				})
				.collect();
			let sides = if local {
				[&ignored, &none]
			} else {
				[&none, &ignored]
			};
			let (kept, deferred) = withhold_deletions_over_unreachable(
				actions,
				&BTreeMap::new(),
				&BTreeSet::new(),
				sides,
				&BTreeSet::new(),
			);
			let kept: Vec<&str> = kept.iter().map(SyncAction::rel_path).collect();
			assert_eq!(kept, vec!["proj/a.txt", ".filenignore"], "local: {local}");
			assert_eq!(deferred, 2, "local: {local}");
		}
	}

	/// A directory deleted on one side keeps its copy on the other while a subtree in it has rules
	/// this pass could not read: what those rules hide cannot be told apart from what they do not.
	#[test]
	fn a_deleted_directory_keeps_a_subtree_whose_rules_could_not_be_read() {
		let docs = Docs::new();
		let mut pulled = prepared(
			SyncMode::TwoWay,
			docs.baseline(),
			docs.local("docs"),
			view(Vec::new(), Vec::new(), &[]),
			HashMap::new(),
		);
		pulled.facts.ignore_blocked.insert("docs/sub".to_string());
		let mut pushed = prepared(
			SyncMode::TwoWay,
			docs.baseline(),
			Vec::new(),
			view(
				vec![
					remote_dir("docs", docs.dir),
					remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
					remote_file("docs/x.bin", docs.x, docs.x_hash, 4),
				],
				Vec::new(),
				&[],
			),
			HashMap::new(),
		);
		pushed.facts.ignore_blocked.insert("docs/sub".to_string());

		for (side, prep) in [("pulled", pulled), ("pushed", pushed)] {
			let screened = reconcile_and_screen(&prep, screen_state(&prep));
			let actions: Vec<&SyncAction> = screened
				.decision
				.safe
				.iter()
				.chain(&screened.decision.held)
				.collect();
			let deletes = |path: &str| {
				actions
					.iter()
					.any(|action| action.is_delete() && action.rel_path() == path)
			};
			assert!(!deletes("docs"), "{side}: {actions:?}");
			assert!(deletes("docs/a.txt"), "{side}: {actions:?}");
			assert_eq!(screened.deferred_paths, 1, "{side}");
			assert!(
				screened.dropped > 0,
				"{side}: the withheld deletion was planned and dropped, so the next pass has to \
				 read everything to plan it again"
			);
		}
	}

	/// A file moved out of an ignored directory is not paired with its tracked copy there: that copy
	/// is neither moved nor deleted, and once the pass has untracked the directory the file at its new
	/// path is an ordinary new item.
	#[test]
	fn a_move_out_of_an_ignored_directory_leaves_the_ignored_copy_alone() {
		let docs = Docs::new();
		let docs_remote = || {
			view(
				vec![
					remote_dir("docs", docs.dir),
					remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
					remote_file("docs/x.bin", docs.x, docs.x_hash, 4),
				],
				Vec::new(),
				&[],
			)
		};
		for mode in [
			SyncMode::TwoWay,
			SyncMode::LocalToRemote,
			SyncMode::LocalBackup,
		] {
			// Moved locally from docs/a.txt to a.txt; the local scan no longer shows docs/.
			let local = vec![local_file("a.txt", docs.a_hash)];
			let mut prep = prepared(
				mode,
				docs.baseline(),
				local.clone(),
				docs_remote(),
				HashMap::new(),
			);
			prep.facts
				.ignored_local
				.insert("docs".to_string(), hidden_by(IgnoreLevel::User, "docs/"));
			let roots = prep.ignored_roots();
			let (actions, _) = run_prepared(prep);
			assert!(actions.is_empty(), "{mode:?}: {actions:?}");

			let (actions, _) = folded_pass(
				mode,
				untracked(docs.baseline(), &roots),
				local,
				view(Vec::new(), Vec::new(), &[]),
				HashMap::new(),
			);
			assert_eq!(actions, vec![upload("a.txt")], "{mode:?}");
		}
		for mode in [
			SyncMode::TwoWay,
			SyncMode::RemoteToLocal,
			SyncMode::RemoteBackup,
		] {
			// Moved on the remote from docs/a.txt to a.txt; the view no longer shows docs/.
			let mut prep = prepared(
				mode,
				docs.baseline(),
				docs.local("docs"),
				view(
					vec![remote_file("a.txt", docs.a, docs.a_hash, 4)],
					Vec::new(),
					&[],
				),
				HashMap::new(),
			);
			prep.facts
				.ignored_remote
				.insert("docs".to_string(), hidden_by(IgnoreLevel::User, "docs/"));
			let (actions, _) = run_prepared(prep);
			assert!(actions.is_empty(), "{mode:?}: {actions:?}");
		}
	}

	/// A directory move carries the ignored roots under it to the new path, each set keyed like the
	/// side it was read from, and the directory of the `.filenignore` that decided one with it. The
	/// deciding line moves unchanged: it is reported as it matched, so a path-shaped line keeps
	/// naming the pre-move path until the next pass decides again.
	#[test]
	fn a_dir_move_carries_the_ignored_roots_under_it() {
		let docs = Docs::new();
		let remote_at = |at: &str| {
			view(
				vec![
					remote_dir(at, docs.dir),
					remote_file(&format!("{at}/a.txt"), docs.a, docs.a_hash, 4),
					remote_file(&format!("{at}/x.bin"), docs.x, docs.x_hash, 4),
				],
				Vec::new(),
				&[],
			)
		};
		let in_docs = |dir: &str| {
			hidden_by(
				IgnoreLevel::File {
					dir: dir.to_string(),
				},
				"build/",
			)
		};
		let by_user = || hidden_by(IgnoreLevel::User, "cache/");

		// Renamed on the remote: the local scan is re-keyed, the view already reads the new path.
		let mut prep = prepared(
			SyncMode::TwoWay,
			docs.baseline(),
			docs.local("docs"),
			remote_at("documents"),
			HashMap::new(),
		);
		prep.facts
			.ignored_local
			.insert("docs/build".to_string(), in_docs("docs"));
		prep.facts
			.ignored_remote
			.insert("documents/cache".to_string(), by_user());
		prep.facts
			.ignore_blocked
			.insert("docs/unreadable".to_string());
		prep.fold_dir_moves();
		assert_eq!(
			prep.dir_moves,
			vec![SyncAction::MoveLocal {
				from_path: "docs".to_string(),
				to_path: "documents".to_string(),
				kind: NodeKind::Dir,
			}]
		);
		assert_eq!(
			prep.facts.ignored_local,
			BTreeMap::from([("documents/build".to_string(), in_docs("documents"))])
		);
		assert_eq!(
			prep.facts.ignored_remote,
			BTreeMap::from([("documents/cache".to_string(), by_user())])
		);
		assert_eq!(
			prep.facts.ignore_blocked,
			BTreeSet::from(["documents/unreadable".to_string()])
		);

		// Renamed locally: the view is re-keyed, the local scan already reads the new path.
		let mut prep = prepared(
			SyncMode::TwoWay,
			docs.baseline(),
			docs.local("documents"),
			remote_at("docs"),
			HashMap::new(),
		);
		prep.facts
			.ignored_local
			.insert("documents/build".to_string(), by_user());
		prep.facts
			.ignored_remote
			.insert("docs/cache".to_string(), in_docs("docs"));
		prep.facts
			.ignore_blocked
			.insert("docs/unreadable".to_string());
		prep.fold_dir_moves();
		assert!(
			matches!(&prep.dir_moves[..], [SyncAction::MoveRemote { to_path, .. }] if to_path == "documents"),
			"{:?}",
			prep.dir_moves
		);
		assert_eq!(
			prep.facts.ignored_local,
			BTreeMap::from([("documents/build".to_string(), by_user())])
		);
		assert_eq!(
			prep.facts.ignored_remote,
			BTreeMap::from([("documents/cache".to_string(), in_docs("documents"))])
		);
		assert_eq!(
			prep.facts.ignore_blocked,
			BTreeSet::from(["documents/unreadable".to_string()])
		);
	}

	/// A root `.filenignore` of `*` hides every item on both sides. The remote is not read as emptied,
	/// because that is decided on the unfiltered view, and no deletion is planned in any mode.
	#[test]
	fn ignoring_everything_neither_reads_as_an_emptied_remote_nor_deletes() {
		let docs = Docs::new();
		let raw = view(
			vec![
				remote_dir("docs", docs.dir),
				remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
				remote_file("docs/x.bin", docs.x, docs.x_hash, 4),
			],
			Vec::new(),
			&[],
		);
		assert!(!remote_emptied(&raw.nodes.whole(), &tree(&docs.baseline())));
		assert!(
			remote_emptied(&HashMap::new(), &tree(&docs.baseline())),
			"the filtered view alone would read as a vanished remote"
		);
		for mode in MODES {
			let mut prep = prepared(
				mode,
				docs.baseline(),
				Vec::new(),
				view(Vec::new(), Vec::new(), &[]),
				HashMap::new(),
			);
			prep.remote_emptied = remote_emptied(&raw.nodes.whole(), &prep.baseline);
			ignoring_docs(&mut prep);
			assert!(screen_state(&prep).absence_trusted(), "{mode:?}");
			let (actions, _) = run_prepared(prep);
			assert!(actions.is_empty(), "{mode:?}: {actions:?}");
		}
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
		remote: &impl Nodes<Node = RemoteNode>,
	) -> Vec<SyncAction> {
		let (scan, _) = scan::scan_local(
			root,
			&Baseline::default(),
			IgnoreRules::default(),
			RuleFiles::Read,
		);
		let plan = plan::reconcile(
			SyncMode::TwoWay,
			&tree(baseline),
			&scan.nodes.whole(),
			remote,
			&plan::PassHolds::default(),
			plan::PassPaths::Whole,
		);
		let ignored: BTreeSet<String> = scan.ignored.keys().cloned().collect();
		let blocked = scan
			.invalid_names
			.keys()
			.chain(scan.aliased_dirs.keys())
			.chain(&ignored)
			.chain(&scan.ignore_blocked)
			.cloned()
			.collect();
		drop_blocked(plan.actions, &blocked, &ignored)
	}

	/// A root `.filenignore` that cannot be read blocks the whole tree, not only the items under
	/// some directory.
	#[cfg(unix)]
	#[test]
	fn an_unreadable_root_filenignore_blocks_every_action() {
		use std::os::unix::fs::PermissionsExt;

		let root = scan_root("unreadable_rules");
		std::fs::write(root.join("a.txt"), b"a").unwrap();
		std::fs::create_dir(root.join("dir")).unwrap();
		std::fs::write(root.join("dir").join("b.txt"), b"b").unwrap();
		std::fs::write(root.join(".filenignore"), "*.tmp\n").unwrap();
		let readable = planned_over_scan(&root, &HashMap::new(), &HashMap::new());
		assert!(
			!readable.is_empty(),
			"the tree uploads while its rules read"
		);

		let rules = root.join(".filenignore");
		std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o000)).unwrap();
		let unreadable = planned_over_scan(&root, &HashMap::new(), &HashMap::new());
		std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o644)).unwrap();
		assert!(
			unreadable.is_empty(),
			"nothing may upload past rules that could not be read: {unreadable:?}"
		);

		std::fs::remove_dir_all(&root).ok();
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
		let mut remote = Side::default();
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

		let actions = planned_over_scan(&root, &baseline, &remote.whole());
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
		let mut nodes: Side<RemoteNode> = [
			("Docs".to_string(), node("Docs", dir, NodeKind::Dir)),
			(
				"Docs/a.txt".to_string(),
				node("Docs/a.txt", child, NodeKind::File),
			),
		]
		.into_iter()
		.collect();
		let mut path_of = ViewIndex::default();
		// Built BEFORE the fold, as it is for a fold that has already answered one record the two
		// paths it names could not: the assertions below are then about the index being kept in
		// step, which is what they were about when it was built unconditionally.
		assert!(
			path_of
				.at(&nodes, &Baseline::default(), Uuid::new_v4())
				.is_none()
		);
		let mut changed = BTreeSet::new();
		assert!(fold_move(
			&mut nodes,
			&mut path_of,
			&Baseline::default(),
			dir,
			"Docs",
			"docs",
			&mut changed,
		));
		let mut paths: Vec<String> = nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		paths.sort_unstable();
		assert_eq!(paths, ["docs", "docs/a.txt"]);
		assert_eq!(
			nodes.whole().at("docs/a.txt").unwrap().rel_path,
			"docs/a.txt"
		);
		assert_eq!(
			path_of.at(&nodes, &Baseline::default(), child).as_deref(),
			Some("docs/a.txt")
		);
		assert_eq!(
			path_of.at(&nodes, &Baseline::default(), dir).as_deref(),
			Some("docs")
		);
		// And every key it moved, BY NAME: both ends for the directory and both for each child it
		// carried. A key the fold leaves out here is a key a change-scoped reconcile never visits,
		// so the re-key would be invisible to the very pass that made it.
		assert_eq!(
			changed.iter().map(String::as_str).collect::<Vec<_>>(),
			["Docs", "Docs/a.txt", "docs", "docs/a.txt"],
			"the fold must record both ends of every key it re-keyed"
		);
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
				&tree(&baseline),
				&local_map(hash(3)),
				&remote_map(uuid, hash(3)),
				&plan::PassHolds::default(),
				plan::PassPaths::Whole,
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
				&tree(&baseline),
				&local_map(hash(3)),
				&remote_map(uuid, hash(3)),
				&plan::PassHolds::default(),
				plan::PassPaths::Whole,
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
			&tree(&baseline),
			&local_map(hash(3)),
			&remote_map(uuid, hash(9)),
			&plan::PassHolds::default(),
			plan::PassPaths::Whole,
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
			&tree(&baseline),
			&local_map(hash(3)),
			&remote_map(uuid, hash(9)),
			&plan::PassHolds::default(),
			plan::PassPaths::Whole,
		)
		.actions;
		assert_eq!(
			actions.iter().map(describe).collect::<Vec<_>>(),
			vec!["upload file \"a.txt\"".to_string()],
		);
	}
	/// A remote node the snapshot holds at `path`.
	fn node_at(path: &str, uuid: Uuid) -> Side<RemoteNode> {
		Side::from(HashMap::from([(
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
		)]))
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
		let mut remote = Side::default();
		let holds = engine
			.pending
			.settle(pair, &engine.observed.snapshot(), &remote.whole());
		assert_eq!(
			engine
				.pending
				.fold_into(pair, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
			1,
			"the reopened engine folds the write its predecessor made"
		);

		let actions = plan::reconcile(
			SyncMode::LocalToRemote,
			&tree(&baseline),
			&local_map(hash(1)),
			&remote.whole(),
			&holds,
			plan::PassPaths::Whole,
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
		let (pair, _) = locked(&engine.control)
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
		let (pair, _) = locked(&engine.control)
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();

		// The registry lock fixes the interleaving, as in the pause/removal race above: both verbs
		// queue on it while this guard is held, and tokio hands the mutex out in request order, so
		// the watch's setup runs first and the removal second — the order in which the removal has
		// nothing registered to trip yet.
		let guard = engine.registry.lock().await;
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
		let (pair, _) = locked(&engine.control)
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();

		// The registry lock fixes the interleaving: both verbs queue on it while this guard is held,
		// and tokio's mutex hands it out in request order, so the pause runs first and the removal
		// second — the order in which a pause that persisted its flag can land after the row it
		// describes is already gone.
		let guard = engine.registry.lock().await;
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
		// registry lock served first; what must hold either way is that nothing stays paused.
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

	/// One pair's store, held for as long as that pair's whole-tree read holds it. Answers once the
	/// lock is actually in hand, and says so for as long as it keeps it, so a test can prove what
	/// ran DURING the hold rather than timing anything.
	fn store_held_for(
		store: SharedStore,
		how_long: Duration,
	) -> (
		tokio::sync::oneshot::Receiver<()>,
		Arc<AtomicBool>,
		tokio::task::JoinHandle<()>,
	) {
		let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel();
		let holding = Arc::new(AtomicBool::new(true));
		let task = {
			let holding = Arc::clone(&holding);
			tokio::task::spawn_blocking(move || {
				let _guard = locked(&store);
				let _ = acquired_tx.send(());
				std::thread::sleep(how_long);
				holding.store(false, Ordering::SeqCst);
			})
		};
		(acquired_rx, holding, task)
	}

	/// A pass on one pair must not hold up the control verbs of another. The pass's widest step is
	/// its baseline read, which holds that pair's store for as long as the tree takes; before the
	/// split there was one store for the whole engine, so `list_pairs`, `pause_pair` and
	/// `resolve_conflict` on ANY pair queued behind it.
	///
	/// Asserted as "they finished while the hold was still in force" rather than as a deadline: the
	/// property is that they never wait for that lock at all, and that reads the same on a busy
	/// machine as on an idle one.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn a_pairs_store_hold_does_not_delay_another_pairs_control_verbs() {
		let (engine, pair_a, path) = engine_with_pair("store_split_control").await;
		let (pair_b, _) = locked(&engine.control)
			.create_pair("/root-b", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let store_b = engine.pair_store(pair_b).await.unwrap();
		locked(&store_b)
			.upsert_entry(pair_b, &converged_conflict(Uuid::new_v4()))
			.unwrap();

		let store_a = engine.pair_store(pair_a).await.unwrap();
		let (acquired, holding, wedge) = store_held_for(store_a, Duration::from_millis(750));
		acquired.await.unwrap();

		let started = std::time::Instant::now();
		engine.list_pairs().await.unwrap();
		engine.pause_pair(pair_b).await.unwrap();
		assert_eq!(
			engine
				.resolve_conflict(pair_b, "a.txt", ConflictResolution::KeepLocal)
				.await
				.unwrap(),
			None,
			"keeping the local side moves no copy into the bin"
		);
		let elapsed = started.elapsed();
		assert!(
			holding.load(Ordering::SeqCst),
			"pair B's control verbs waited out pair A's store hold: they took {elapsed:?}, by \
			 which time the hold had already been released"
		);

		wedge.await.unwrap();
		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// And when a verb DOES have to wait for its own pair's store, it waits on a blocking thread:
	/// the runtime's worker keeps running other tasks meanwhile. One worker thread here, so a task
	/// that still makes progress proves the wait is not on it — which is what stops a pass's
	/// whole-tree SQLite work from stalling a runtime this engine shares with everything else.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn a_pairs_whole_tree_store_work_waits_off_the_runtime_thread() {
		let (engine, pair, path) = engine_with_pair("store_off_runtime").await;
		let store = engine.pair_store(pair).await.unwrap();
		let (acquired, holding, wedge) = store_held_for(store, Duration::from_millis(300));
		acquired.await.unwrap();

		let ticks = Arc::new(AtomicUsize::new(0));
		let ticking = {
			let ticks = Arc::clone(&ticks);
			tokio::spawn(async move {
				loop {
					tokio::time::sleep(Duration::from_millis(10)).await;
					ticks.fetch_add(1, Ordering::SeqCst);
				}
			})
		};

		// This reads the pair's conflicts, so it wants the very lock the wedge is holding.
		let started = std::time::Instant::now();
		engine.list_conflicts(pair).await.unwrap();
		let waited = started.elapsed();
		ticking.abort();

		assert!(
			!holding.load(Ordering::SeqCst) && waited >= Duration::from_millis(150),
			"the read did not actually contend for the store it shares with the hold: {waited:?}"
		);
		assert!(
			ticks.load(Ordering::SeqCst) >= 5,
			"the runtime's only worker made almost no progress ({} ticks) while a store read \
			 waited: the wait is on the runtime thread, not on a blocking one",
			ticks.load(Ordering::SeqCst)
		);

		wedge.await.unwrap();
		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A fresh engine on a throwaway baseline DB, with one pair registered.
	async fn engine_with_pair(tag: &str) -> (SyncEngine, PairId, PathBuf) {
		let path = std::env::temp_dir().join(format!("filen_sync_{tag}_{}.db", Uuid::new_v4()));
		let engine = SyncEngine::open(offline_client(), path.clone())
			.await
			.unwrap();
		let (pair, _) = locked(&engine.control)
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		(engine, pair, path)
	}

	/// The user ignore patterns round-trip, and a text with a bad line is refused whole, naming that
	/// line and leaving the stored text alone.
	#[tokio::test]
	async fn user_ignore_patterns_round_trip_and_a_bad_text_is_refused_whole() {
		let (engine, _, path) = engine_with_pair("user_ignore").await;
		assert_eq!(engine.user_ignore().await.unwrap(), "");
		engine.set_user_ignore("*.psd\nbuild/").await.unwrap();
		assert_eq!(engine.user_ignore().await.unwrap(), "*.psd\nbuild/");

		let error = engine
			.set_user_ignore("*.tmp\n{a\n*.log")
			.await
			.unwrap_err()
			.to_string();
		assert!(
			error.contains("user ignore patterns:2:"),
			"the refusal must name the bad line: {error}"
		);
		assert_eq!(
			engine.user_ignore().await.unwrap(),
			"*.psd\nbuild/",
			"a refused text must not replace the stored one"
		);

		// The signal every watch loop waits on fires for a text that was STORED, and not for one
		// that was refused — a wake-up for patterns nothing can read is a pass for nothing.
		let rules = engine.user_ignore_changes();
		assert!(!rules.has_changed().unwrap());
		engine.set_user_ignore("*.tmp\n{a").await.unwrap_err();
		assert!(!rules.has_changed().unwrap());
		engine.set_user_ignore("*.tmp").await.unwrap();
		assert!(rules.has_changed().unwrap());

		// And the setter does not depend on anyone listening: the last watch going away must not
		// turn the next call into an error.
		drop(rules);
		engine.set_user_ignore("*.log").await.unwrap();
		assert_eq!(engine.user_ignore().await.unwrap(), "*.log");

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// `list_conflicts` returns exactly the rows held for resolution — both flavours, with what each
	/// side held — and nothing from a synced row; an unknown pair is an error, not an empty list.
	#[tokio::test]
	async fn list_conflicts_reads_the_held_rows_only() {
		let (engine, pair, path) = engine_with_pair("list_conflicts").await;
		assert!(engine.list_conflicts(pair).await.unwrap().is_empty());
		let hash = Blake3Hash::from([7; 32]);
		{
			let pair_store = engine.pair_store(pair).await.unwrap();
			let store = locked(&pair_store);
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
		// its pair's store lock holds it up: that is the window the removal spends waiting for it.
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
			let pair_store = engine.pair_store(pair).await.unwrap();
			let store = locked(&pair_store);
			let now = Utc::now().timestamp_millis();
			store
				.record_failure(pair, "broken.txt", "boom", now)
				.unwrap();
			store
				.record_failure(pair, "broken.txt", "boom", now)
				.unwrap();
		}
		let before = locked(&engine.pair_store(pair).await.unwrap())
			.failures(pair)
			.unwrap();

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
			locked(&engine.pair_store(pair).await.unwrap())
				.failures(pair)
				.unwrap(),
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
			locked(&engine.pair_store(pair).await.unwrap())
				.entries(pair)
				.unwrap()
				.is_empty(),
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
		locked(&engine.pair_store(pair).await.unwrap())
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
			locked(&engine.pair_store(pair).await.unwrap())
				.entry(pair, "a.txt")
				.unwrap()
				.is_some_and(|row| row.state == BaselineState::Conflicted),
			"the resolution wrote its row under the pass"
		);

		// The offline pass fails in its read once it gets the slot, and lets the resolution through.
		drop(slot);
		assert!(pass.await.unwrap().is_err());
		assert_eq!(
			resolving.await.unwrap().unwrap(),
			None,
			"keeping the local side moves no copy into the bin"
		);
		assert_eq!(
			locked(&engine.pair_store(pair).await.unwrap())
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
		locked(&engine.pair_store(pair).await.unwrap())
			.upsert_entry(pair, &converged_conflict(Uuid::new_v4()))
			.unwrap();
		assert_eq!(
			engine
				.resolve_conflict(pair, "a.txt", ConflictResolution::KeepLocal)
				.await
				.expect("a paused pair must stay resolvable"),
			None
		);

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

	/// Keeping the remote side of an ordinary conflict moves the losing local edit to the bin, and
	/// the call NAMES where it put it: that edit was never uploaded, so the download that follows
	/// would otherwise destroy the only copy, and a caller offering it back cannot be left to
	/// guess among the bin's ` (N)` siblings. A local copy that already holds the remote's content
	/// has nothing to lose, stays put, and there is no path to name.
	#[tokio::test]
	async fn keeping_remote_quarantines_a_local_edit_the_remote_never_had() {
		let path =
			std::env::temp_dir().join(format!("filen_sync_keep_remote_{}.db", Uuid::new_v4()));
		let root = std::env::temp_dir().join(format!("filen_sync_keep_remote_{}", Uuid::new_v4()));
		std::fs::create_dir_all(&root).unwrap();
		let engine = SyncEngine::open(offline_client(), path.clone())
			.await
			.unwrap();
		let (pair, _) = locked(&engine.control)
			.create_pair(root.to_str().unwrap(), Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();

		// Diverged: the local copy's content is not the remote head's.
		std::fs::write(root.join("a.txt"), b"LOCAL").unwrap();
		let diverged = BaselineEntry {
			remote_hash: Some(hash(4)),
			..converged_conflict(Uuid::new_v4())
		};
		locked(&engine.pair_store(pair).await.unwrap())
			.upsert_entry(pair, &diverged)
			.unwrap();
		let quarantined = engine
			.resolve_conflict(pair, "a.txt", ConflictResolution::KeepRemote)
			.await
			.unwrap()
			.expect("the resolution has to say where it put the copy it moved aside");
		assert_eq!(
			quarantined,
			root.join(scan::QUARANTINE_DIR).join("a.txt"),
			"the path it named is not where the copy went"
		);
		assert_eq!(
			std::fs::read(&quarantined).unwrap(),
			b"LOCAL",
			"the losing local edit must be recoverable from the bin"
		);
		assert!(!root.join("a.txt").exists());
		assert_eq!(
			locked(&engine.pair_store(pair).await.unwrap())
				.entry(pair, "a.txt")
				.unwrap(),
			None,
			"with the local side moved away, the remote copy must read as a fresh create"
		);

		// Converged: nothing to lose, nothing moved.
		std::fs::write(root.join("b.txt"), b"SAME").unwrap();
		let converged = BaselineEntry {
			rel_path: "b.txt".to_string(),
			..converged_conflict(Uuid::new_v4())
		};
		locked(&engine.pair_store(pair).await.unwrap())
			.upsert_entry(pair, &converged)
			.unwrap();
		assert_eq!(
			engine
				.resolve_conflict(pair, "b.txt", ConflictResolution::KeepRemote)
				.await
				.unwrap(),
			None,
			"nothing was moved aside, so there is no path to name"
		);
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
		let mut remote = Side::default();
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
		pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new());

		let actions = plan::reconcile(
			SyncMode::LocalToRemote,
			&tree(&baseline),
			&local_map(hash(2)),
			&remote.whole(),
			&holds,
			plan::PassPaths::Whole,
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

		let mut remote = Side::default();
		pending.settle(PAIR, &observed, &remote.whole());
		assert_eq!(
			pending.fold_into(
				PAIR,
				&tree(&written_row(uuid, hash(1))),
				&mut remote,
				&mut BTreeSet::new()
			),
			0,
			"an announced uuid retires the write even with nothing left at the path"
		);
		assert!(
			remote.whole().is_empty(),
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

		let mut remote = Side::default();
		pending.settle(PAIR, &observed, &remote.whole());
		assert_eq!(
			pending.fold_into(
				PAIR,
				&tree(&written_row(uuid, hash(1))),
				&mut remote,
				&mut BTreeSet::new()
			),
			1,
			"only what was known before the snapshot may retire a write against it"
		);
		assert_eq!(remote.whole().at("a.txt").unwrap().remote_uuid, uuid);
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
		pending.settle(PAIR, &observations.snapshot(), &remote.whole());
		assert_eq!(
			pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
			1,
			"the snapshot still shows the pre-move path and nothing new has been announced"
		);

		observations.note_uuids([uuid]);
		let mut remote = node_at("a.txt", uuid);
		pending.settle(PAIR, &observations.snapshot(), &remote.whole());
		assert_eq!(
			pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
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
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
		assert_eq!(
			pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
			1
		);

		assert!(
			!remote.whole().holds("a.txt"),
			"the pre-move path is vacated"
		);
		assert_eq!(remote.whole().at("b.txt").unwrap().remote_uuid, uuid);
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
			plan::reconcile(
				SyncMode::TwoWay,
				&tree(&baseline),
				&local,
				&remote.whole(),
				&holds,
				plan::PassPaths::Whole
			)
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
		remote.extend(node_at("b.txt", foreign).into_whole());
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
		assert_eq!(
			pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
			1
		);

		assert!(
			!remote.whole().holds("a.txt"),
			"the pre-move path is vacated even though the destination is somebody else's"
		);
		assert_eq!(
			remote.whole().at("b.txt").unwrap().remote_uuid,
			foreign,
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
		let actions = plan::reconcile(
			SyncMode::LocalToRemote,
			&tree(&baseline),
			&local,
			&remote.whole(),
			&holds,
			plan::PassPaths::Whole,
		)
		.actions;
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
		pending.record(
			&observations,
			PAIR,
			dir,
			PendingKind::Trashed {
				path: "d".to_string(),
			},
		);

		let mut remote = Side::from(HashMap::from([
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
		]));
		// Trashing dropped both baseline rows, and the local side is gone too.
		let (baseline, local) = (HashMap::new(), HashMap::new());
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
		assert_eq!(
			pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
			1
		);
		assert!(
			remote.whole().is_empty(),
			"the trashed directory takes its subtree with it"
		);
		assert!(
			holds.trashed.contains(&dir),
			"the deletion of that uuid is still suppressed, whatever a view shows"
		);
		assert!(
			plan::reconcile(
				SyncMode::LocalToRemote,
				&tree(&baseline),
				&local,
				&remote.whole(),
				&holds,
				plan::PassPaths::Whole,
			)
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
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
		assert_eq!(
			pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
			1
		);
		assert_eq!(
			remote.whole().at("a.txt").unwrap().remote_uuid,
			new,
			"the pre-write occupant is what a lagging cache shows; ours supersedes it"
		);
		assert_eq!(
			remote.whole().len(),
			1,
			"and does not leave the old one behind"
		);

		// A further local edit is then pushed rather than left alone.
		let actions = plan::reconcile(
			SyncMode::LocalToRemote,
			&tree(&baseline),
			&local_map(hash(2)),
			&remote.whole(),
			&holds,
			plan::PassPaths::Whole,
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
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
		assert_eq!(
			pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
			1
		);
		assert_eq!(
			remote.whole().at("a.txt").unwrap().remote_uuid,
			second,
			"the latest write is what the path holds"
		);
		assert!(
			plan::reconcile(
				SyncMode::TwoWay,
				&tree(&baseline),
				&local_map(hash(2)),
				&remote.whole(),
				&holds,
				plan::PassPaths::Whole,
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
		let mut node = remote.whole().at("a.txt").unwrap().into_owned();
		node.stable_uuid = Some(lineage);
		node.content_hash = Some(hash(1));
		remote.insert("a.txt".to_string(), node);

		let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
		assert_eq!(
			pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
			1,
			"the cache has announced nothing of ours, so what it shows is the version we replaced"
		);
		assert_eq!(remote.whole().at("a.txt").unwrap().remote_uuid, ours);
		assert!(
			plan::reconcile(
				SyncMode::TwoWay,
				&tree(&baseline),
				&local_map(hash(2)),
				&remote.whole(),
				&holds,
				plan::PassPaths::Whole,
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
			let mut node = nodes.whole().at("a.txt").unwrap().into_owned();
			node.stable_uuid = Some(lineage);
			nodes.insert("a.txt".to_string(), node);
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
			Side::default(),
		] {
			assert!(
				pending
					.strangers(PAIR, &tree(&baseline), &settled.whole())
					.is_empty(),
				"the record answers for this state on its own"
			);
		}

		assert_eq!(
			pending.strangers(PAIR, &tree(&baseline), &same_lineage(theirs).whole()),
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
		let mut node = remote.whole().at("a.txt").unwrap().into_owned();
		node.stable_uuid = Some(lineage);
		remote.insert("a.txt".to_string(), node);

		pending.retire(ours);
		assert_eq!(
			pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
			0,
			"with the record gone there is nothing left to paint over the snapshot"
		);
		assert_eq!(remote.whole().at("a.txt").unwrap().remote_uuid, theirs);
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
		pending.settle(PAIR, &observations.snapshot(), &remote.whole());
		assert_eq!(
			pending.fold_into(
				PAIR,
				&tree(&written_row(new, hash(1))),
				&mut remote,
				&mut BTreeSet::new()
			),
			0,
			"a uuid that is neither ours nor the one we replaced is a foreign write"
		);
		assert_eq!(
			remote.whole().at("a.txt").unwrap().remote_uuid,
			foreign,
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

		let mut remote = Side::default();
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
		assert_eq!(
			pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
			1
		);
		let actions = plan::reconcile(
			SyncMode::TwoWay,
			&tree(&baseline),
			&local,
			&remote.whole(),
			&holds,
			plan::PassPaths::Whole,
		)
		.actions;
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
		pending.record(
			&observations,
			PAIR,
			trashed,
			PendingKind::Trashed {
				path: "t.txt".to_string(),
			},
		);
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

		let holds = pending.settle(PAIR + 1, &observations.snapshot(), &foreign.whole());
		assert!(
			holds.trashed.is_empty(),
			"a pass holds nothing on behalf of another pair"
		);
		assert_eq!(
			pending.fold_into(
				PAIR + 1,
				&Baseline::default(),
				&mut foreign.clone(),
				&mut BTreeSet::new()
			),
			0,
			"nor does it fold another pair's writes into its own view"
		);

		let mut remote = node_at("gone.txt", trashed);
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
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
			pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
			2,
			"and still has not caught up to either write"
		);
		assert!(!remote.whole().holds("gone.txt"), "the trash is folded out");
		assert_eq!(remote.whole().at("note.txt").unwrap().remote_uuid, created);
	}

	/// A pass indexes the snapshot only for writes of its own, and a pair that journalled none
	/// indexes nothing. What it must NOT skip is the retirement that reads a record by itself: the
	/// cache announcing a uuid retires that record whichever pair's pass happens to see it, or a
	/// write nobody passes over again would be folded into the view for ever.
	#[test]
	fn a_pass_with_no_write_of_its_own_still_retires_an_announced_one() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, created("a.txt", None));
		observations.note_uuids([uuid]);

		let holds = pending.settle(PAIR + 1, &observations.snapshot(), &HashMap::new());

		assert!(
			holds.trashed.is_empty(),
			"a pass holds nothing on behalf of another pair"
		);
		assert!(
			pending.uuids().is_empty(),
			"the announcement retires the record even though the passing pair owns no write"
		);
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

		let read =
			|| observed_confirmations(&observations, &tree(&baseline), &HashMap::new(), ripe).0;
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

	/// The rows a test spells as a path-keyed map, as the pass's resident baseline.
	fn tree(rows: &HashMap<String, BaselineEntry>) -> Baseline {
		Baseline::from_rows(rows.values().cloned())
	}
}
