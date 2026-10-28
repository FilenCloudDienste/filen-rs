//! The engine orchestration: register pairs, plan a pass (read-only), and run one (plan + apply).
//!
//! `prepare` runs the read-only half — load the baseline, scan the local tree (fast-path),
//! enumerate the remote subtree from the cache, build the remote view — shared by `plan_pair` (a
//! dry run) and `sync_once` (plan + guard + apply + baseline advance).

use std::{
	borrow::Cow,
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	hash::Hash,
	mem,
	num::NonZeroUsize,
	ops::RangeInclusive,
	path::{Path, PathBuf},
	sync::{
		Arc, MutexGuard, PoisonError, Weak,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::{Duration, Instant, SystemTime},
};

use chrono::{NaiveDate, Utc};
use filen_macros::js_type;
use filen_types::fs::ParentUuid;
use futures::FutureExt;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
	Backlog, SyncEvent, SyncMode,
	apply::{self, ApplyContext, PlannedEntry, SyncReport},
	baseline::{
		BaselineEntry, BaselineState, BaselineStore, NodeKind, PairId, PairRecord, PathFailure,
		PendingRow, SyncedPaths,
	},
	bin,
	changes::{DIRTY_CAP, FullPassReason, PairChanges, PassScope, RemoteDeltaEntry, past_cap},
	derive,
	facts::{self, PairFacts},
	guard::{self, DeleteGuard, GuardReason},
	ignore::{
		IgnoreDecision, IgnoreLevel, IgnoreRules, IgnoreSource, IgnoredPath, Origin, RemoteRules,
		RuleCandidates, load_remote_rules, parse_user_ignore, rule_file_dir, rule_file_paths,
	},
	legacy::{self, LegacyImport},
	observe::{self, LocalObservation, LocalObservations},
	outcome::{
		PlanOutcome, PlannedAction, PlannedConflict, PlannedNodeKind, RefuseReason, UnsyncablePath,
		UnsyncableReason, planned_action, planned_conflict,
	},
	pause::{PassControl, PassGate, PauseOptions, cancel_suspension},
	plan::{self, RemoteNode, RemoteView, SyncAction},
	remote::{RemoteObserved, cache_ancestry, delta_uuids, observe_remote},
	rows::Baseline,
	scan::{self, LocalScan, PairRoot, RuleFiles, ScanError, ScanRoot},
	seed::{self, SeedCounts, Seeding},
	side::{Nodes, NodesAt, Side, SideRef},
	stage::{self, Confirmed, Rows, Screen, ScreenError, Stage, StagedPlan},
};
use crate::{
	Error, ErrorKind,
	auth::Client,
	cache::{
		CacheEvent, CacheEventType, DirEvent, FileEvent, SearchResult, SyncRootCallback,
		SyncRootHandle,
	},
	fs::{HasParent, HasUUID, categories::DirType},
	io::client_impl::IoSharedClientExt,
};

/// The ceiling on how long a remote write this engine made stays trusted over the cache snapshot.
///
/// A write normally retires long before this: the cache announces the item (see [`Observations`])
/// or the snapshot shows what happened at its path. The window is the last resort for the case
/// where neither ever arrives — past it the snapshot is believed again, so an item genuinely
/// deleted elsewhere is picked up late, never ignored.
const PENDING_CREATE_GRACE: Duration = Duration::from_secs(180);

/// How often the engine's journal — its own recent writes ([`PendingWrites`]) and the cache's
/// announcements ([`Observations`]) — is swept of what has aged past every question still asked
/// of it.
///
/// Nothing else drops those entries until the next pass, so a long pass would otherwise keep one
/// per item it wrote or heard about. A sweep walks the whole journal, which is why it runs at most
/// this often rather than on every write: the journal then holds the grace window's entries plus
/// at most this interval's.
const JOURNAL_SWEEP_EVERY: Duration = Duration::from_secs(45);

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

/// How far apart two failures of one path on a remote outage
/// ([`HaltReason::is_remote_outage`](super::HaltReason::is_remote_outage): the
/// remote out of reach, answering 5xx, 408 or 429, or refusing the session) must be for the second
/// to extend its streak: a third of [`PATH_FAILURE_RETRY_INTERVAL`].
///
/// Such a failure fails the whole pass, and a watch backs off on it, but it says nothing about the
/// path it happened to catch: counted like any other, the transfers an hour-long outage caught
/// would all be blocked for another hour after it. Not counted at all, a path the server answers
/// with a 5xx on every pass, while the rest of the drive works, would fail every pass forever.
/// Spaced out, an outage costs a path at most one failure in each such window, which a success
/// clears, while the path that always fails reaches [`MAX_PATH_FAILURES`] — and is blocked, so its
/// passes stop failing — after two windows.
const TRANSIENT_FAILURE_SPACING: Duration = Duration::from_secs(20 * 60);

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

/// How often a pass waiting for the cache to finish listing its pair's remote folder asks again
/// (see [`SyncEngine::await_remote_view`]). One read of one `cache_meta` row each time.
const CONVERGENCE_POLL: Duration = Duration::from_secs(1);

/// How long one pass waits for that listing before it fails, leaving the next pass to wait again.
/// Long enough for the first listing of a large drive, which takes minutes; a root the cache never
/// converges (one the server deleted, or one it cannot register while offline) fails the pass
/// rather than holding its gate forever.
const CONVERGENCE_WAIT: Duration = Duration::from_secs(15 * 60);

/// How many items of a pass one bulk request carries: the server runs a bulk request in groups of
/// 50 items and takes its per-user lock once per group, so this is one group.
///
/// The server takes about 91-117 ms per directory created and 108-115 ms per item trashed, so the
/// request takes about 6 s. A chunk already sent is not raced against a cancel, so it has to come
/// back within [`REMOVE_CANCEL_GRACE`]. A chunk of creates also looks up the links and shares of the
/// directories it lands in, all at once, before it is sent, and afterwards adds each new directory
/// to the links and shares of its parent, a round trip or so per level of the chunk's tree (a
/// level's directories at once). Under a linked or shared directory, a chunk that nests more than
/// a few dozen levels deep can still outlast the grace.
pub(super) const SYNC_BULK_CHUNK: usize = 50;

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
/// boundary where a release splits nothing (see `stage::Staged::releasable`), and takes it again before
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
/// than take a lock and look a couple of uuids up — and, once per [`JOURNAL_SWEEP_EVERY`], drop
/// what has aged out (see [`ObservationState::sweep`]).
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
	/// When [`ObservationState::sweep`] last ran.
	swept: Option<Instant>,
}

impl Observations {
	/// Record every uuid a committed cache batch touched, and time-stamp what it says about a push
	/// of ours. Runs on the cache worker thread, so it does nothing but take a lock and insert,
	/// plus the periodic [`ObservationState::sweep`].
	fn note(&self, events: &mut dyn Iterator<Item = &CacheEvent<'_>>) {
		let mut state = self.state();
		// Read under the lock, after every counter value handed out before it: an announcement that
		// retires a write is then never older than the write (see `ObservationState::sweep`).
		let now = Instant::now();
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
		self.state()
			.watch_push(uuid, lineage, replaced, Instant::now());
	}

	/// Record a push of ours that the cache announced long enough ago to have stood its
	/// [`CONFIRM_TENURE`] — the state the pass after an upload finds once the cache has caught up,
	/// which is the pass that confirms it.
	#[cfg(feature = "bench-internals")]
	fn bench_stood(&self, uuid: Uuid) {
		let now = Instant::now();
		let announced = now
			.checked_sub(CONFIRM_TENURE * 2)
			.expect("the monotonic clock has run longer than two confirmation windows");
		self.state().insert_push(
			uuid,
			PushTenure {
				lineage: None,
				recorded: announced,
				announced: Some(announced),
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

	/// Drop the push records of `decided`, a page of a pass's persisted confirmations at a time;
	/// [`forget_pushes`](Self::forget_pushes) once they are all dropped hands their memory back.
	fn drop_pushes(&self, decided: impl IntoIterator<Item = Uuid>) {
		let mut state = self.state();
		for uuid in decided {
			state.drop_push(uuid);
		}
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
		// The pass after a first sync retires a record per uploaded file here.
		state.pushes.shrink_to_fit();
		state.pushes_by_lineage.shrink_to_fit();
		retain_shrunk(&mut state.minted, |_, at| {
			now.saturating_duration_since(*at) < MINTED_TTL
		});
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
		retain_shrunk(&mut self.state().seen, |_, (stamp, _)| {
			oldest.is_some_and(|seq| *stamp > seq)
		});
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
		self.sweep(now);
		self.seq += 1;
		let seq = self.seq;
		self.seen.insert(uuid, (seq, now));
	}

	/// Drop the announcements and the minted uuids nothing will ask about again, at most once per
	/// [`JOURNAL_SWEEP_EVERY`]. Only the next pass would prune either otherwise, so a long pass
	/// would keep one of each per item it uploaded or heard about.
	///
	/// An announcement retires only a write it is no older than: [`PendingWrites::record`] reads the
	/// clock before the counter and [`Observations::note`] after it. No write outlives
	/// [`PENDING_CREATE_GRACE`], so neither does any announcement a write could still need. (A write
	/// restored at open is dated back by its age, and an announcement that arrives while the engine
	/// opens can be older than that date by as long as the open took; such a write runs out its
	/// grace that much later than the announcement, so dropping the announcement only lets the
	/// grace retire the write instead.) The other reader, [`Observations::watch_push`], looks only
	/// for an announcement that beat the upload call's own response.
	fn sweep(&mut self, now: Instant) {
		if !sweep_due(&mut self.swept, now) {
			return;
		}
		retain_shrunk(&mut self.seen, |_, (_, at)| {
			now.saturating_duration_since(*at) < PENDING_CREATE_GRACE
		});
		retain_shrunk(&mut self.minted, |_, at| {
			now.saturating_duration_since(*at) < MINTED_TTL
		});
	}

	/// [`Observations::watch_push`], at `now`.
	fn watch_push(
		&mut self,
		uuid: Uuid,
		lineage: Option<filen_types::fs::StableUuid>,
		replaced: Option<Uuid>,
		now: Instant,
	) {
		self.sweep(now);
		if let Some(replaced) = replaced {
			self.drop_push(replaced);
		}
		// The cache can announce our own upload before the call that made it has returned; the
		// announcement map is the only place that moment survives.
		let announced = self.seen.get(&uuid).map(|(_, at)| *at);
		self.minted.insert(uuid, now);
		self.insert_push(
			uuid,
			PushTenure {
				lineage,
				recorded: now,
				announced,
				superseded: None,
			},
		);
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
			DirEvent::New(d) | DirEvent::Restored(d) | DirEvent::Move(d) | DirEvent::Changed(d) => {
				[Some(d.uuid), None]
			}
			DirEvent::Removed(uuid)
			| DirEvent::MetadataChanged { uuid, .. }
			| DirEvent::ColorChanged { uuid, .. } => [Some(*uuid), None],
		},
		CacheEventType::Global(_) | CacheEventType::NoOp => [None, None],
	}
}

/// A pair's cache subscription: the handle that keeps it, and whether the cache still holds it.
pub(super) struct Subscription {
	/// Kept to be dropped: that unsubscribes the pair.
	_handle: SyncRootHandle,
	/// Alive for as long as the cache holds the subscription's callback, which owns the strong
	/// end (see [`observation_callback`]). The cache drops the callback when it ends the
	/// registration on its own — the server deleted the root, which the cache drops from its active
	/// set (`CacheMessage::SyncRootsDeleted`), or the worker shut down (`Client::flush_cache`) —
	/// and the handle is not told: it stays, inert, and nothing it subscribed to is announced or
	/// converged any more.
	registered: Weak<()>,
}

impl Subscription {
	/// Whether the cache still holds this subscription, rather than having ended it under the
	/// handle.
	pub(super) fn live(&self) -> bool {
		self.registered.strong_count() > 0
	}
}

/// The sync-root callback the engine registers per pair: it records the uuids of every committed
/// cache batch — the evidence that retires a pending write — and the batch's changes as the pair's
/// remote changelist. It runs on the cache worker thread, so it never awaits and never touches the
/// database.
///
/// The events are borrowed for the length of the call and cannot be read twice, so they are
/// collected into one `Vec` of references (pointers, not payloads) and handed to both records.
///
/// It owns `registered`, so the cache dropping it is what tells a [`Subscription`] the cache ended
/// it.
fn observation_callback(
	observations: Arc<Observations>,
	changes: Arc<PairChanges>,
	registered: Arc<()>,
) -> SyncRootCallback {
	Box::new(move |events| {
		// Captured to be owned, and dropped with the callback; it carries nothing to read.
		let _registered = &registered;
		let batch: Vec<&CacheEvent<'_>> = events.collect();
		changes.note_remote_batch(&mut batch.iter().copied());
		observations.note(&mut batch.iter().copied());
	})
}

/// Read what the cache's announcements say about every unconfirmed push in `unconfirmed` — rows
/// [`Baseline::unconfirmed`] hands out, a page of them at a time (see [`SyncEngine::confirm`]): the
/// version uuids they confirm, and the rows (`rel_path`, ours, the foreign version `view` holds at
/// the path) they say nothing about that are worth asking the server about. `view` is what the
/// evidence reads ([`Evidence::view`]); `None` has nothing to ask about.
///
/// Purely a READ of the observations. The records it consulted stay put, because the caller may be
/// a dry run: [`SyncEngine::plan_pair`] reaches this through `prepare` and persists nothing, and a
/// record dropped there would leave the pass that follows with no evidence at all that the push was
/// ever confirmed. Only the callers that write the advanced rows retire the records
/// ([`Observations::forget_pushes`]).
fn observed_confirmations<'r>(
	observed: &Observations,
	unconfirmed: impl IntoIterator<Item = &'r BaselineEntry>,
	view: Option<&SideRef<'_, RemoteNode>>,
	now: Instant,
) -> (std::collections::HashSet<Uuid>, Vec<ChainQuestion>) {
	let mut confirmed = std::collections::HashSet::new();
	// rel_path is only for the log; the lookup runs on the foreign version sitting at it.
	let mut ask_server = Vec::new();
	for entry in unconfirmed {
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
				if let Some(node) = view.and_then(|view| view.at_row(entry))
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

/// What a confirmation reads besides the cache's announcements (see [`SyncEngine::confirm`]).
///
/// It holds the pass's remote side for as long as the confirmation reads it, which is on a blocking
/// thread, and [`put_back`](Self::put_back) returns the side to the pass.
pub(super) enum Evidence {
	/// A whole pass's RAW snapshot, read before this engine's own writes are folded into it: it
	/// confirms a row by listing the row's own version at its path ([`plan::snapshot_confirms`]),
	/// and a foreign version of the same file there is what the version chain is asked about.
	Snapshot(Side<RemoteNode>),
	/// A change-scoped pass's view. Never rule (c): that would confirm rows against a view derived
	/// from those very rows (I6). What this still costs is a confirmation delayed to the next whole
	/// pass, never one granted early — the verdicts come from announcements, and the view is only
	/// read to spot a FOREIGN version at a row's path.
	Derived(Side<RemoteNode>),
	/// No view at all, between passes: the announcements alone, so nothing asks the server either.
	Announcements,
}

impl Evidence {
	/// The view to read: a derived side against `rows`, the rows it derives from, and a snapshot
	/// against `no_rows`, an empty tree — it derives nothing, so that is exact.
	fn view<'v>(
		&'v self,
		rows: &'v Baseline,
		no_rows: &'v Baseline,
	) -> Option<SideRef<'v, RemoteNode>> {
		match self {
			Self::Snapshot(side) => Some(side.of(no_rows)),
			Self::Derived(side) => Some(side.of(rows)),
			Self::Announcements => None,
		}
	}

	/// Return the side this evidence read to `nodes`, which lent it.
	pub(super) fn put_back(self, nodes: &mut Side<RemoteNode>) {
		if let Self::Snapshot(side) | Self::Derived(side) = self {
			*nodes = side;
		}
	}
}

/// A row worth asking the server's version chain about (see [`observed_confirmations`]): its path,
/// for the log, our version, and the foreign version at the path.
type ChainQuestion = (String, Uuid, Uuid);

/// A page of unconfirmed rows a confirmation has read (see [`Confirming::page`]).
struct ReadPage {
	/// Each row, with whether the snapshot lists it.
	rows: Vec<(BaselineEntry, bool)>,
	/// The versions the announcements vouched for.
	vouched: HashSet<Uuid>,
}

/// A confirmation of the pushes of ours `baseline` holds unconfirmed (see [`SyncEngine::confirm`]),
/// between the pages it reads: what it has confirmed, and where it has got to. It owns everything it
/// reads, so a page is read and staged on a blocking thread while the server is asked about the
/// page before it on the runtime's.
pub(super) struct Confirming<S> {
	baseline: Baseline,
	evidence: Evidence,
	observed: Arc<Observations>,
	/// Makes the stage the confirmed rows move into past a window of them.
	stage: S,
	confirmed: stage::ConfirmedRows,
	window: usize,
	now: Instant,
	/// The path the next page starts after; `None` once a page came back short, as the last does.
	after: Option<String>,
	/// The page read last, while the server is asked about it.
	asking: Option<ReadPage>,
	/// The versions the server's chain vouched for so far: one per lookup it answered yes to.
	chained: HashSet<Uuid>,
}

impl<S: FnMut() -> Result<Stage, Error>> Confirming<S> {
	pub(super) fn new(
		baseline: Baseline,
		evidence: Evidence,
		observed: Arc<Observations>,
		stage: S,
		window: NonZeroUsize,
	) -> Self {
		Self {
			baseline,
			evidence,
			observed,
			stage,
			confirmed: stage::ConfirmedRows::new(window.get()),
			window: window.get(),
			now: Instant::now(),
			after: Some(String::new()),
			asking: None,
			chained: HashSet::new(),
		}
	}

	/// Stage the page read last, then read the next one and stage what the evidence vouches for in
	/// it. Its rows worth asking the server about come back instead, and the page waits for the
	/// answer, which the caller adds to [`chained`](Self::chained) before the next call stages it.
	/// `None` once every page is read and staged.
	fn page(&mut self) -> Result<Option<Vec<ChainQuestion>>, Error> {
		if let Some(page) = self.asking.take() {
			self.stage_page(page)?;
		}
		let Some(after) = &self.after else {
			return Ok(None);
		};
		let page = self.baseline.unconfirmed_page(after, self.window);
		// Owned: the cursor outlives the page, whose rows are staged.
		self.after = page
			.last()
			.filter(|_| page.len() == self.window)
			.map(|last| last.rel_path.clone());
		if page.is_empty() {
			return Ok(None);
		}
		let no_rows = Baseline::default();
		let view = self.evidence.view(&self.baseline, &no_rows);
		let listed: Vec<bool> = page
			.iter()
			.map(|row| {
				matches!(self.evidence, Evidence::Snapshot(_))
					&& plan::snapshot_confirms(
						view.as_ref().and_then(|view| view.at_row(row)).as_deref(),
						row,
					)
			})
			.collect();
		let rest = page
			.iter()
			.zip(&listed)
			.filter(|(_, listed)| !**listed)
			.map(|(row, _)| row);
		let (vouched, ask_server) =
			observed_confirmations(&self.observed, rest, view.as_ref(), self.now);
		let page = ReadPage {
			rows: page.into_iter().zip(listed).collect(),
			vouched,
		};
		if ask_server.is_empty() {
			self.stage_page(page)?;
		} else {
			self.asking = Some(page);
		}
		Ok(Some(ask_server))
	}

	/// Stage each row of `page` the snapshot lists, or whose version the announcements or the
	/// server's chain vouched for.
	fn stage_page(&mut self, ReadPage { rows, mut vouched }: ReadPage) -> Result<(), Error> {
		vouched.extend(&self.chained);
		for (row, listed) in rows {
			if listed || plan::push_vouched(&vouched, &row) {
				self.confirmed
					.push(plan::confirm_agreed(row), &mut self.stage)?;
			}
		}
		Ok(())
	}

	/// The rows confirmed, once [`page`](Self::page) has staged every page, with the evidence for
	/// the pass to take its side back from. A version the chain vouched for after the page of a row
	/// carrying it was staged confirms that row on a second read of the rows, which runs only when
	/// the server vouched for anything.
	fn finish(mut self) -> Result<(Confirmed, Evidence), Error> {
		if !self.chained.is_empty() {
			let mut after = String::new();
			loop {
				let page = self.baseline.unconfirmed_page(&after, self.window);
				let Some(last) = page.last() else {
					break;
				};
				after.clone_from(&last.rel_path);
				let full = page.len() == self.window;
				for row in page {
					if plan::push_vouched(&self.chained, &row)
						&& !self.confirmed.holds(&row.rel_path)?
					{
						self.confirmed
							.push(plan::confirm_agreed(row), &mut self.stage)?;
					}
				}
				if !full {
					break;
				}
			}
		}
		Ok((self.confirmed.finish()?, self.evidence))
	}
}

/// [`SyncEngine::confirm`] on this thread, with no server to ask: a row worth asking about stays as
/// the announcements left it. What the probe and the tests measure a confirmation by.
#[cfg(any(test, feature = "bench-internals"))]
pub(super) fn confirm_here<S: FnMut() -> Result<Stage, Error>>(
	mut confirming: Confirming<S>,
) -> Result<(Confirmed, Evidence), Error> {
	while confirming.page()?.is_some() {}
	confirming.finish()
}

/// Remote writes this engine made recently. The apply layer records every item it creates or
/// moves; [`settle`](PendingWrites::settle) drops the records the cache has demonstrably caught up
/// to and the ones past [`PENDING_CREATE_GRACE`], and [`expire`](PendingWrites::expire) drops the
/// latter as the writes after them are recorded, so the map holds about one grace window of writes
/// even while a long pass is still making them.
#[derive(Debug, Default)]
pub(super) struct PendingWrites(std::sync::Mutex<Journal>);

#[derive(Debug, Default)]
struct Journal {
	writes: HashMap<Uuid, PendingWrite>,
	/// When [`PendingWrites::expire`] last swept `writes`.
	swept: Option<Instant>,
}

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
		// The clock before the counter: an announcement that retires this write is then never
		// older than it, which is what lets the old announcements go (see `ObservationState::sweep`).
		let at = Instant::now();
		self.insert(pair, uuid, kind, observations.stamp(), at);
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

	/// Drop the records past [`PENDING_CREATE_GRACE`] and return their uuids, for the transaction
	/// that journals the next write to retire from the DB as well
	/// ([`BaselineStore::record_pending`]).
	///
	/// `settle`'s grace rule, applied as the apply journals each write instead of at the next pass:
	/// a pass that writes for hours would otherwise keep every record it made until it ends. The
	/// rule reads a record by itself, so it drops whichever pair's records it finds. A sweep walks
	/// every record, so it runs at most once per [`JOURNAL_SWEEP_EVERY`] and returns nothing in
	/// between.
	pub(super) fn expire(&self) -> Vec<Uuid> {
		self.expire_at(Instant::now())
	}

	fn expire_at(&self, now: Instant) -> Vec<Uuid> {
		let mut journal = self.journal();
		if !sweep_due(&mut journal.swept, now) {
			return Vec::new();
		}
		let mut expired = Vec::new();
		retain_shrunk(&mut journal.writes, |uuid, write| {
			let live = now.duration_since(write.at) < PENDING_CREATE_GRACE;
			if !live {
				expired.push(*uuid);
			}
			live
		});
		expired
	}

	fn insert(&self, pair: PairId, uuid: Uuid, kind: PendingKind, seq: u64, at: Instant) {
		self.journal().writes.insert(
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
		self.journal().writes.keys().copied().collect()
	}

	/// Drop every record for `pair` — the pair is gone, so nothing may fold on its behalf (a
	/// re-registration of the same roots gets the same id back).
	fn forget_pair(&self, pair: PairId) {
		retain_shrunk(&mut self.journal().writes, |_, write| write.pair != pair);
	}

	/// The oldest live write's observation stamp, for pruning [`Observations`].
	fn oldest_stamp(&self) -> Option<u64> {
		self.journal().writes.values().map(|write| write.seq).min()
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
		remote: &SideRef<'_, RemoteNode>,
	) -> plan::PassHolds {
		let now = Instant::now();
		let mut journal = self.journal();
		let map = &mut journal.writes;
		// Only this pair's OWN records consult the snapshot, and walking every remote node for a
		// pass that journalled none is the whole cost of this call on a large tree. The two rules
		// above the pair check read the record by itself, so they still run — from an empty index —
		// and still retire what the cache has demonstrably caught up to, whoever wrote it.
		//
		// Only the nodes a record names are looked up: the index is asked about nothing else, and
		// the pass after a big push would otherwise walk a view of the whole tree to settle a few
		// records. A staged view answers off its uuid index.
		let snapshot_path: HashMap<Uuid, Cow<'_, str>> =
			if map.values().any(|write| write.pair == pair) {
				remote.paths_of(&map.keys().copied().collect())
			} else {
				HashMap::new()
			};
		retain_shrunk(map, |uuid, write| {
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
		self.journal()
			.writes
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
		self.journal().writes.remove(&uuid);
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
	/// be, and [`ViewIndex`] falls back to a walk of the whole view only for the one record shape
	/// that cannot be answered that way.
	pub(super) fn fold_into(
		&self,
		pair: PairId,
		baseline: &Baseline,
		nodes: &mut Side<RemoteNode>,
		changed: &mut BTreeSet<String>,
	) -> usize {
		let journal = self.journal();
		let mut writes: Vec<(&Uuid, &PendingWrite)> = journal
			.writes
			.iter()
			.filter(|(_, write)| write.pair == pair)
			.collect();
		if writes.is_empty() {
			return 0;
		}
		// Oldest first: two writes can name one path (a re-upload on the very next pass), and the
		// later one has to land on top.
		writes.sort_unstable_by_key(|(_, write)| write.at);
		let mut path_of = ViewIndex::new(&journal.writes);
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

	fn journal(&self) -> std::sync::MutexGuard<'_, Journal> {
		// Plain data behind the lock: a panic while holding it cannot leave the map inconsistent.
		self.0
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
	}
}

/// Whether a sweep last run at `swept` is due again at `now` — once per [`JOURNAL_SWEEP_EVERY`] —
/// and if so, date it `now`.
fn sweep_due(swept: &mut Option<Instant>, now: Instant) -> bool {
	if swept.is_some_and(|swept| now.saturating_duration_since(swept) < JOURNAL_SWEEP_EVERY) {
		return false;
	}
	*swept = Some(now);
	true
}

/// [`HashMap::retain`], then hand back the capacity the dropped entries held. A map never shrinks
/// by itself, so a journal map that once held a first sync's worth of records would keep that much
/// allocated for as long as the engine runs, long after every record in it was retired.
fn retain_shrunk<K: Eq + Hash, V>(map: &mut HashMap<K, V>, keep: impl FnMut(&K, &mut V) -> bool) {
	map.retain(keep);
	map.shrink_to_fit();
}

/// [`PENDING_CREATE_GRACE`] as the journal stores it: wall-clock millis.
fn grace_millis() -> i64 {
	i64::try_from(PENDING_CREATE_GRACE.as_millis()).unwrap_or(i64::MAX)
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

/// Where the view holds each uuid the journal names — built only when a record cannot be answered
/// from the paths it names itself.
///
/// Every record says where it expects its item to be: a move its two ends, a trash the path it
/// emptied, a create its own path (which it looks up directly, and never asks this). That is where
/// a cache merely lagging shows the item, so in the ordinary case this stays empty. Indexing the
/// view up front is one `String` clone and one hash per node of the WHOLE tree, and a pass pays the
/// fold every time the previous one pushed anything.
///
/// Only a foreign edit puts the item on a third path. Then the index is built, once, and kept in
/// step by [`place_node`] and by the removals for the rest of the fold — a scan per record would be
/// quadratic on a large tree right after a large pass. It holds only the nodes whose uuid
/// `journal` names, the only uuids the fold looks up, so the index starts at one entry per record
/// rather than one per node of the tree: a staged view seeks its uuid index for each, any other is
/// walked once (see [`SideRef::paths_of`]).
struct ViewIndex<'a> {
	journal: &'a HashMap<Uuid, PendingWrite>,
	index: Option<HashMap<Uuid, String>>,
}

impl<'a> ViewIndex<'a> {
	/// An index, not built yet, for a fold of `journal`'s records.
	fn new(journal: &'a HashMap<Uuid, PendingWrite>) -> Self {
		Self {
			journal,
			index: None,
		}
	}

	/// Where `nodes` holds `uuid`, a uuid `journal` names, building the index if it is not built
	/// yet.
	fn at(&mut self, nodes: &Side<RemoteNode>, baseline: &Baseline, uuid: Uuid) -> Option<String> {
		let journal = self.journal;
		self.index
			.get_or_insert_with(|| {
				nodes
					.of(baseline)
					.paths_of(&journal.keys().copied().collect())
					.into_iter()
					.map(|(uuid, path)| (uuid, path.into_owned()))
					.collect()
			})
			.get(&uuid)
			.cloned()
	}

	fn placed(&mut self, uuid: Uuid, path: &str) {
		if let Some(index) = self.index.as_mut() {
			index.insert(uuid, path.to_owned());
		}
	}

	fn gone(&mut self, uuid: Uuid) {
		if let Some(index) = self.index.as_mut() {
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
	path_of: &mut ViewIndex<'_>,
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
	path_of: &mut ViewIndex<'_>,
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
	path_of: &mut ViewIndex<'_>,
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
	path_of: &mut ViewIndex<'_>,
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
			let Some(new) = plan::moved_path(&old, from, to) else {
				continue;
			};
			let Some(mut child) = nodes.remove(baseline, &old) else {
				continue;
			};
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
	path_of: &mut ViewIndex<'_>,
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

/// Run `work` on a blocking thread, for what a pass does to its own stages and rows whose cost
/// scales with the tree — the reconcile, the screening and the confirmation — as [`off_store`] runs
/// the store's. `work` owns what it reads and hands back whatever the pass still needs. A panic in
/// it is the pass's error, which `what` names.
async fn off_runtime<T>(what: &str, work: impl FnOnce() -> T + Send + 'static) -> Result<T, Error>
where
	T: Send + 'static,
{
	tokio::task::spawn_blocking(work)
		.await
		.map_err(|e| Error::custom(ErrorKind::Internal, format!("{what} panicked: {e}")))
}

/// Remove the download temp files `pair`'s scan found left behind by a killed download (see
/// [`scan::remove_stale_downloads`]). Housekeeping, so nothing here fails the pass: a temp file is
/// never synced, and one left in place is looked at again by the next whole read.
async fn remove_stale_downloads(pair: PairId, stale: Vec<PathBuf>) {
	if stale.is_empty() {
		return;
	}
	let found = stale.len();
	match off_runtime("removing stale download temp files", move || {
		scan::remove_stale_downloads(&stale, SystemTime::now())
	})
	.await
	{
		Ok(removed) => tracing::info!(
			"sync_once[pair {pair}]: removed {removed} of {found} download temp file(s) a killed \
			 download left behind"
		),
		Err(e) => tracing::warn!(
			"sync_once[pair {pair}]: stale download temp files were not removed: {e}"
		),
	}
}

/// Whether `pair` has synced: it has a baseline row. Before its first pass it has none, which is
/// when filen-sync's state may be imported (see [`SyncEngine::import_legacy_state`]) and its trash
/// is moved into the bin (see [`SyncEngine::migrate_legacy_trash`]). A pair whose rows have all
/// gone since (every synced item deleted on both sides) reads as not synced again; for both callers
/// that only means repeating harmless work.
fn has_synced(store: &BaselineStore, pair: PairId) -> Result<bool, Error> {
	Ok(!store
		.baseline(pair)
		.map_err(|e| db_error(e, "reading the sync baseline"))?
		.is_empty())
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
	/// registered; dropping one unsubscribes it.
	roots: Mutex<HashMap<PairId, Subscription>>,
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
	/// The UTC day each pair's local bin was last pruned (see
	/// [`prune_bin_daily`](SyncEngine::prune_bin_daily)), so a pair's passes prune it at most once
	/// a day. In memory only: the first pass after the engine opens always prunes, which costs one
	/// listing of the bin's top level. Bounded by the pair count; an entry goes with its pair.
	bin_pruned: Mutex<HashMap<PairId, NaiveDate>>,
	/// The pairs whose filen-sync trash this engine has finished looking for (see
	/// [`migrate_legacy_trash`](SyncEngine::migrate_legacy_trash)), or found already synced, so the
	/// tree is walked for it at most once per pair and engine: a pair whose first pass is refused
	/// or cancelled again and again does not walk its whole tree once more on each attempt. In
	/// memory only, so an engine opened later looks again while the pair still has no baseline row.
	/// Bounded by the pair count; an entry goes with its pair.
	///
	/// Each with the trash folders the walk found that are still in place (a move that failed),
	/// which every later pass of this engine moves again, without a walk and whether or not the
	/// pair has rows by then, until the move lands or the folder is gone. Empty once nothing is
	/// owed.
	legacy_trash_looked: Mutex<HashMap<PairId, Vec<String>>>,
	/// Bumped once [`set_user_ignore`](SyncEngine::set_user_ignore) has COMMITTED new patterns, so
	/// every running watch loop runs a pass with them instead of waiting out its safety net. A
	/// [`watch`](tokio::sync::watch) channel rather than a [`Notify`](tokio::sync::Notify): every
	/// loop must see the change (a `Notify` wakes one waiter), a burst of setter calls coalesces
	/// into one wake-up, and a version nobody is subscribed to is still a valid send.
	user_ignore_changed: tokio::sync::watch::Sender<u64>,
	/// How many stages this engine has made: the number the next one is named by (see
	/// [`stage`](super::stage)).
	stages: AtomicU64,
	/// The lock that keeps every other engine off the baseline DB while this one lives (see
	/// [`BaselineLock::take`]), or `None` where the platform has no file locks. Last, as fields drop
	/// in order: the engine lets go of its connections to the baseline before it lets go of this.
	_lock: Option<BaselineLock>,
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
	/// What the guard held back. Without it a benchmark cannot tell a pass whose deletions were all
	/// held — the whole point of a mass-delete scenario — from a pass that planned nothing at all,
	/// and both report `actions: 0`.
	pub(super) held: usize,
	/// Paths surfaced as two-way conflicts rather than actions.
	pub(super) conflicts: usize,
	/// Directory moves folded out of the plan, each carrying its whole subtree (see
	/// [`plan::fold_dir_moves`]). A scenario that moves a directory and folds no move measured a
	/// re-upload of the subtree instead, which is a different pass at a very different price.
	pub(super) dir_moves: usize,
	/// Pushes of ours this pass confirmed — the rows whose agreed-content marker it advanced. A
	/// pass after an upload that confirmed none measured a pass that never wrote to its baseline,
	/// which is the one edit such a pass makes to its view of the rows.
	pub(super) confirmed: usize,
	pub(super) rows: usize,
	/// What the pass's own structures computed their size as at its widest point.
	///
	/// `None` in every TIMED pass: summing them visits every node of both sides, and no timed
	/// region may pay that. [`bench::measuring_memory`](super::bench::measuring_memory) — true only
	/// inside a memory child — is what decides, so the one tail below serves both without a second
	/// copy of it existing to drift.
	pub(super) structures: Option<PassStructures>,
}

/// What the structures a pass holds at its widest point compute their own size as. An
/// ATTRIBUTION, not a measurement of the process: see [`probe::pass_structures_bytes`].
#[cfg(feature = "bench-internals")]
#[derive(Debug, Clone, Copy)]
pub(super) struct PassStructures {
	pub(super) baseline_bytes: usize,
	pub(super) view_bytes: usize,
	pub(super) scan_bytes: usize,
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

	/// Open the pair's store and read its baseline the way a pass begins to, and answer how many
	/// rows it holds and what the baseline computes its own size as.
	///
	/// The steady state a memory child reports is an engine holding this pair and NOTHING else, and
	/// running a pass is not a way to reach it: the figure afterwards would be one that had held a
	/// whole pass's structures. This offers no way to assemble a pass — it hands back two numbers —
	/// and what it leaves behind is what an idle engine sits on between passes: the pair's store
	/// and one idle reader connection, with nothing of the rows in memory.
	///
	/// The terms come back BESIDE the total rather than instead of it: the total is what every
	/// published figure has always been, and the split is what says which of nine structures to
	/// go after. A hand estimate of the parts reached only ~175 MiB of the 211.6 measured at a
	/// million rows, so the split was not known.
	pub(super) async fn bench_load_pair(
		&self,
		pair: PairId,
	) -> Result<(usize, usize, super::rows::ResidentTerms), Error> {
		let store = self.pair_store(pair).await?;
		let baseline = locked(&store)
			.baseline(pair)
			.map_err(|e| db_error(e, "loading the benchmark pair's baseline"))?;
		Ok((
			baseline.len(),
			baseline.resident_bytes(),
			baseline.resident_terms(),
		))
	}

	/// Record every push the pair's baseline holds unconfirmed as one the cache announced long
	/// enough ago to have stood its tenure, and answer how many that was.
	///
	/// The in-memory half of "the pass after an upload": the rows an unconfirmed push leaves are in
	/// the DB, and what confirms them in a change-scoped pass is the announcement record, which no
	/// DB carries — so a memory child has to be told again, like [`Self::bench_seed_carry`].
	pub(super) async fn bench_seed_stood_pushes(&self, pair: PairId) -> Result<usize, Error> {
		let store = self.pair_store(pair).await?;
		let baseline = locked(&store)
			.baseline(pair)
			.map_err(|e| db_error(e, "loading the benchmark pair's baseline"))?;
		let mut seeded = 0;
		for uuid in baseline.unconfirmed().filter_map(|row| row.remote_uuid) {
			self.observed.bench_stood(uuid);
			seeded += 1;
		}
		Ok(seeded)
	}

	/// Put a pair's changelist back in the state a freshly registered pair's is in.
	///
	/// A memory child opens an engine on a baseline DB that ALREADY holds the pair, and
	/// [`Self::open`] subscribes every registered pair to the cache's notifications
	/// ([`Self::observe_pair`]). A benchmark fixture's remote root is synthetic, so that
	/// subscription is refused and the pair is marked
	/// [`FullPassReason::RemoteEventsDegraded`] — permanently, by design: a degraded source forces
	/// every later pass to read both sides whole.
	///
	/// The parent process never reaches that state, and not because it has a subscription: its
	/// engine opened on an EMPTY registry and registered the pair afterwards through
	/// [`Self::bench_create_pair`], which subscribes nothing. Neither process ends up with cache
	/// notifications for this fixture, but they do not differ in the flag ALONE: the child's refused
	/// attempt also started a cache worker and a second connection to the fixture's cache DB, and
	/// both are inside the resident set it goes on to report. So this drops the changelist
	/// entry and lets the next [`Self::pair_changes`] build a fresh one, and the child measures the
	/// pass its parent timed instead of a whole read wearing that pass's name.
	///
	/// What holds it honest is that `one_pass` asserts WHICH read ran on every pass, in the child
	/// as in the parent: a scenario whose pass fell back fails rather than reporting a figure.
	pub(super) async fn bench_reset_changes(&self, pair: PairId) {
		self.changes.lock().await.remove(&pair);
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
		let scope = match scope {
			Some(scope) => Some(ScopedRead {
				converged: self
					.await_remote_view(pair, None, std::future::pending())
					.await?,
				scope,
			}),
			None => None,
		};
		// It applies nothing, so it seeds nothing either: the probe does not time the stage.
		let prep = self.prepare(pair, scope, Seeding::Skip).await?;
		super::step("prepare_tail");
		let state = screen_state(&prep);
		let at = self.stage_at(pair);
		let (prep, screened) = off_runtime("screening the pass's plan", move || {
			let screened = reconcile_and_screen(&prep, state, || Stage::create(at));
			(prep, screened)
		})
		.await?;
		let screened = screened?;
		super::step("reconcile_and_screen");
		// Summed HERE, at the pass's widest point: every structure it built is still alive and the
		// drop below is what frees them. Only a memory child asks — the sum is O(nodes), and it is
		// taken AFTER the step mark above so a timed pass could not pay for it even if it did.
		let structures = super::bench::measuring_memory().then(|| {
			let (baseline_bytes, view_bytes, scan_bytes) = super::probe::pass_structures_bytes(
				&prep.baseline,
				&prep.remote_view.nodes,
				&prep.local_scan.nodes,
			);
			PassStructures {
				baseline_bytes,
				view_bytes,
				scan_bytes,
			}
		});
		let pass = BenchPass {
			scoped: prep.read.is_scoped(),
			full_reason: prep.read.full_pass_reason(),
			actions: screened.count(Rows::Safe)?,
			held: screened.count(Rows::Held)?,
			conflicts: screened.conflicts.len(),
			dir_moves: prep.dir_moves.len(),
			confirmed: prep.confirmed.len(),
			rows: prep.baseline.len(),
			structures,
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

/// A pass's changelists, taken while the cache held the pair's remote root converged at
/// generation `converged` (see [`SyncEngine::await_remote_view`]).
struct ScopedRead<'a> {
	scope: &'a mut PassScope,
	converged: u64,
}

/// The per-pair reads every pass makes before it looks at either tree.
struct PassInputs {
	record: PairRecord,
	user_ignore: String,
	store: SharedStore,
	baseline: Baseline,
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

/// The sizes the local map a change-scoped pass assembles may have and still account for the rows
/// and observations it was built from: plan 3.6's self-check, which the pass holds the assembled
/// map's length against.
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
///
/// Computed from the observations alone, and BEFORE `merge_local` runs: the merge moves each walk's
/// nodes out of its scan, so counting them afterwards would count none. Taken first, the bounds
/// also stay what they have to be, a count independent of the code whose output they check.
pub(super) fn assembly_bounds(
	baseline: &Baseline,
	observed: &LocalObservations,
	held: &BTreeSet<String>,
) -> RangeInclusive<usize> {
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
		//
		// COUNTED, not read: the table's count under the path, less what the walk pruned and the
		// held rows outside that. Read, it was every row under the path — after a directory is
		// renamed away, every row it held, which `merge_local` then walks again. A pruned root costs
		// a lookup and a count, and the loop stops once nothing under the path is left to take.
		// Each subtraction takes rows the count holds. Saturating all the same: one the count could
		// not cover errs toward a higher bound and a whole read, never wraps to one that passes any
		// map.
		let rows_at = |at: &str| {
			let own = usize::from(
				baseline.contains_key(at) && !pruned.contains(at) && !held.contains(at),
			);
			// Pruned at `at` or above it: the walk covered nothing under it.
			if !at.is_empty() && plan::at_or_under_root(&pruned, at) {
				return own;
			}
			let mut under = baseline.count_subtree(at);
			for root in strictly_under(&pruned, at) {
				if under == 0 {
					break;
				}
				// Once, at the outermost root: one inside another is in the count of the one it is
				// in.
				if root
					.rsplit_once('/')
					.is_some_and(|(parent, _)| plan::at_or_under_root(&pruned, parent))
				{
					continue;
				}
				under = under.saturating_sub(
					usize::from(baseline.contains_key(root)) + baseline.count_subtree(root),
				);
			}
			// A held row under a pruned root went with that root.
			let held_under = strictly_under(held, at)
				.filter(|path| {
					!plan::at_or_under_root(&pruned, path) && baseline.contains_key(path)
				})
				.count();
			own + under.saturating_sub(held_under)
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
	lowest..=lowest.saturating_add(slack)
}

/// The paths in `set` strictly under `dir`, as [`Baseline::count_subtree`] reads "under": `""`
/// holds every path but itself.
fn strictly_under<'s>(set: &'s BTreeSet<String>, dir: &str) -> impl Iterator<Item = &'s String> {
	if dir.is_empty() {
		set.range::<String, _>(..)
	} else {
		set.range(plan::subtree_bounds(dir))
	}
	.filter(|path| !path.is_empty())
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
		.find(|path| !decided.contains(path.as_ref()) && !baseline.contains_key(path))
		.map(Cow::into_owned)
}

/// The read-only inputs to a pass, shared by planning and applying.
struct Prepared {
	record: PairRecord,
	baseline: Baseline,
	/// How many rows the pair tracked when the pass read its baseline, before the seeding stage
	/// wrote any (see [`seed_unhashed`](Self::seed_unhashed)). The guard's whole-tree judgements are
	/// made from this, so a pass that seeds its first rows is still a first sync and its deletions
	/// are still held.
	tracked_at_start: usize,
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
	/// Baseline rows whose agreed-content marker this pass advanced (see
	/// [`SyncEngine::confirm`]). Already applied to `baseline`, so planning reads them either way;
	/// a real pass persists them, a dry run writes nothing.
	confirmed: Confirmed,
}

impl Prepared {
	/// The pass's seeding stage (see [`seed`]), on a blocking thread, as `seeding` says: the pass
	/// comes back reading the rows it seeded, written into the store or, on a dry run, into its own
	/// view of the rows alone.
	///
	/// Run after everything else the read does and before the directory-move fold, the one step that
	/// layers more onto the rows than the confirmations: a fresh snapshot keeps neither, and this
	/// puts the confirmations back.
	async fn seeded(self, store: &SharedStore, seeding: Seeding) -> Result<Self, Error> {
		let seed: fn(&mut Self, &BaselineStore) -> Result<SeedCounts, Error> = match seeding {
			Seeding::Write => Self::seed_unhashed,
			Seeding::Preview => Self::preview_unhashed,
			Seeding::Skip => return Ok(self),
		};
		let store = Arc::clone(store);
		let (prepared, seeded) = off_runtime(
			"recording the hashless files both sides agree on",
			move || {
				let mut prepared = self;
				let seeded = seed(&mut prepared, &locked(&store));
				(prepared, seeded)
			},
		)
		.await?;
		seeded?;
		Ok(prepared)
	}

	/// Record as synced every rowless path where both sides hold a file the remote has no BLAKE3
	/// for and the evidence says the two agree (see [`seed`]), then read the rows again so the plan
	/// sees them. Returns what it recorded, by evidence.
	///
	/// The rows are read again from the store rather than layered onto this pass's view, and the
	/// confirmations the pass found are put back on them; the guard's first-sync and tracked counts
	/// stay what the pass started from ([`tracked_at_start`](Self::tracked_at_start)).
	fn seed_unhashed(&mut self, store: &BaselineStore) -> Result<SeedCounts, Error> {
		let pair = self.record.id;
		let seeded = self.weigh_unhashed(store)?;
		if seeded.total() == 0 {
			return Ok(seeded);
		}
		tracing::info!(
			"sync_once[pair {pair}]: recorded {} hashless remote file(s) as synced with the local \
			 one: {} the imported filen-sync state lists as synced, {} empty on both sides, {} of \
			 the same size and modification time",
			seeded.total(),
			seeded.imported,
			seeded.empty,
			seeded.size_and_mtime
		);
		// This pass's snapshot goes first, so the fresh one reads on the connection it gives back.
		drop(mem::take(&mut self.baseline));
		let mut fresh = store
			.baseline(pair)
			.map_err(|e| db_error(e, "reading the baseline again after seeding it"))?;
		fresh.confirm(&self.confirmed);
		self.baseline = fresh;
		Ok(seeded)
	}

	/// [`seed_unhashed`](Self::seed_unhashed) for a dry run: weigh the same paths by the same
	/// evidence, and write each row a pass would record into this read's own view of the rows
	/// instead of into `store`, which it only reads. Returns what a pass would record, by evidence.
	fn preview_unhashed(&mut self, store: &BaselineStore) -> Result<SeedCounts, Error> {
		let preview = seed::Preview::new(store);
		let seeded = self.weigh_unhashed(&preview)?;
		if seeded.total() > 0 {
			tracing::debug!(
				"plan_pair[pair {}]: reading {} hashless remote file(s) as synced with the local one, \
				 as a pass would record them",
				self.record.id,
				seeded.total()
			);
			self.baseline.write_seeded(preview.into_rows());
		}
		Ok(seeded)
	}

	/// The seeding stage's weighing, through `store` (see [`seed`]): every candidate this pass
	/// decides that the evidence records as synced is handed to the store's
	/// [`seed_rows`](seed::SeedStore::seed_rows). Returns what was handed over, by evidence.
	///
	/// The candidates are what this pass decides: on a whole read the remote view's hashless files,
	/// read off the stage's index of them; on a change-scoped one the paths its read decided. A path
	/// the pass would not act on — held, blocked, hidden or failing — is left for a pass that does,
	/// and a pass that will be refused weighs nothing at all.
	fn weigh_unhashed(&self, store: &impl seed::SeedStore) -> Result<SeedCounts, Error> {
		if refusal(self).is_some() {
			return Ok(SeedCounts::default());
		}
		let blocked = self.blocked_paths();
		let skip = |path: &str| {
			self.hides(path)
				|| plan::at_or_under_root(&blocked, path)
				|| plan::is_under_held_path(path, &self.holds.held_remote)
		};
		let local = self.local_scan.nodes.of(&self.baseline);
		let remote = self.remote_view.nodes.of(&self.baseline);
		match &self.read {
			PassRead::Whole(_) => seed::seed(
				store,
				self.record.id,
				&self.baseline,
				&local,
				remote.unhashed_files(),
				skip,
			),
			PassRead::Scoped(decided) => seed::seed(
				store,
				self.record.id,
				&self.baseline,
				&local,
				decided.iter().filter_map(|path| {
					remote.at(path).map(|node| (Cow::from(path.as_str()), node))
				}),
				skip,
			),
		}
		.map_err(|e| db_error(e, "recording the hashless files both sides agree on"))
	}

	/// Carry each directory move across as one move — a case-only rename included — and read the
	/// rest of the pass where those subtrees end up (see [`plan::fold_dir_moves`]).
	///
	/// The fold re-keys the paths a scoped pass decides itself, move by move as it folds them; what
	/// is replayed here is everything else this pass keys by path.
	///
	/// `Err` only on a change-scoped pass, for a move over more rows than it narrows itself to (see
	/// [`plan::fold_dir_moves`]): the pass then reads both sides whole instead.
	fn fold_dir_moves(&mut self) -> Result<(), FullPassReason> {
		let decided = match &mut self.read {
			PassRead::Scoped(decided) => Some(decided),
			PassRead::Whole(_) => None,
		};
		self.dir_moves = plan::fold_dir_moves(
			self.record.mode,
			&mut self.baseline,
			&mut self.local_scan.nodes,
			&mut self.remote_view.nodes,
			&self.holds.held_remote,
			decided,
		)?;
		// What this pass blocks and reports is keyed by path too, and has to follow the fold, or a
		// block stays behind at a path nothing is keyed by any more while its item reads as absent at
		// the new one: a remote item the view cannot place would have its local copy quarantined.
		// Replayed in order, each move re-keys exactly what the fold re-keyed for it.
		for action in &self.dir_moves {
			let (from, to) = action.endpoints();
			// Keyed like the baseline rows they were read from. A park on the moved directory itself
			// is cleared by its rename, as any rename clears one; carried along, it would hold back the
			// very move that renames it.
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
		Ok(())
	}

	/// The plan the pass screened (`screen`), each action with what the apply reads for it, staged in
	/// the same stage in the order the apply runs them, with the deletions each directory deletion
	/// carries (see [`PlannedEntry::stage`]).
	///
	/// Takes the pass's rows and both its sides out of `self` to do it: the scan, the view and the
	/// snapshot are dropped here, and the pass's read transaction with the snapshot. What is left
	/// answers nothing about either side or any row, and nothing after the plan asks it to: only
	/// what the pass records once its apply is done.
	///
	/// Off the runtime thread: it writes a row per action, and a first sync plans the whole tree. A
	/// stage that cannot be written (a full disk) fails the pass before it has applied anything.
	async fn stage_plan(&mut self, screen: Screen) -> Result<StagedPlan, Error> {
		let at = screen.path().display().to_string();
		let (baseline, local, remote) = (
			mem::take(&mut self.baseline),
			mem::replace(&mut self.local_scan.nodes, Side::carried()),
			mem::replace(&mut self.remote_view.nodes, Side::carried()),
		);
		tokio::task::spawn_blocking(move || PlannedEntry::stage(screen, baseline, local, remote))
			.await
			.map_err(|e| {
				Error::custom(
					ErrorKind::Internal,
					format!("staging the pass's plan panicked: {e}"),
				)
			})?
			.map_err(|e| db_error(e, &format!("staging the pass's plan in {at}")))
	}

	/// The `rows` of the screened plan in the public [`PlannedAction`] shape, in plan order, each
	/// one's size resolved from whichever side of this pass knows it.
	fn planned(&self, screened: &Screened, rows: Rows) -> Result<Vec<PlannedAction>, Error> {
		let Some(plan) = &screened.plan else {
			return Ok(Vec::new());
		};
		let (local, remote) = (
			self.local_scan.nodes.of(&self.baseline),
			self.remote_view.nodes.of(&self.baseline),
		);
		let mut planned = Vec::new();
		plan.each(rows, |action| {
			planned.push(planned_action(&action, &local, &remote));
		})
		.map_err(|e| db_error(e, "reading the pass's screened plan"))?;
		Ok(planned)
	}

	/// Every path this pass will not act on, and why — reported identically by the dry run and by
	/// the pass itself, so a caller sees the same list either way.
	fn unsyncable(&self) -> Vec<UnsyncablePath> {
		let names = self
			.facts
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
				self.facts
					.unknown_remote
					.iter()
					.map(|(rel_path, reason)| UnsyncablePath {
						rel_path: rel_path.clone(),
						reason: reason.clone(),
					}),
			)
			.chain(self.facts.never_synced_remote.iter().cloned())
			.chain(
				self.facts
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
			|| plan::ancestors(path).chain([path]).any(|at| {
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
			.filter(|path| plan::ancestors(path).any(|parent| roots.contains_key(parent)))
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
			.map(|(path, (decision, tracked))| IgnoredPath {
				rel_path: path.to_owned(),
				level: decision.level.clone(),
				pattern: decision.pattern.clone(),
				tracked,
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

/// The failure streaks applying the screened plan `plan` can end, each as the path of the action
/// whose success ends it and the path whose streak that is: every action's own, then those of the
/// deletions a directory deletion carries out, which happen with it.
///
/// Only paths in `streaks` are kept, and a plan is not read at all when there is none. Ending a
/// streak that does not exist deletes no row, and a first sync's plan is the whole tree. `None` is a
/// store that could not say which paths have one: every path is then kept, as if each had.
fn endable_streaks(
	plan: &Screen,
	streaks: Option<&HashMap<String, PathFailure>>,
) -> rusqlite::Result<Vec<(String, String)>> {
	if streaks.is_some_and(HashMap::is_empty) {
		return Ok(Vec::new());
	}
	let has_streak = |path: &str| streaks.is_none_or(|streaks| streaks.contains_key(path));
	let mut ends = Vec::new();
	plan.each(Rows::Safe, |action| {
		let path = action.rel_path();
		if has_streak(path) {
			ends.push((path.to_owned(), path.to_owned()));
		}
	})?;
	plan.each_carried(|dir, path| {
		if has_streak(path) {
			ends.push((dir.to_owned(), path.to_owned()));
		}
	})?;
	Ok(ends)
}

/// Which side wins when a caller resolves a held two-way conflict via
/// [`SyncEngine::resolve_conflict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(import, no_ser, no_default, napi)]
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

/// Why [`SyncEngine::open`] refused a baseline DB: another engine, in this process or another, has
/// it open. Two engines on one baseline run passes over the same pairs with nothing ordering them
/// against each other, each planning from rows the other is rewriting under it.
///
/// Carried as the source of the returned [`Error`], of kind [`ErrorKind::IO`], so a caller can
/// recover it with [`Error::downcast`].
#[derive(Debug, thiserror::Error)]
#[error(
	"another sync engine has the baseline DB {} open, and only one may have it open at a time",
	path.display()
)]
pub struct BaselineInUse {
	/// The baseline DB that was asked for.
	pub path: PathBuf,
}

/// Why [`SyncEngine::open`] refused a baseline DB: it is stamped at a schema version this build
/// does not read. There is no migration chain, so a DB at any other version, older or newer, was
/// not written by this engine, and reading its rows under these rules would misplan them into
/// deletes. The DB is left as it was.
///
/// Carried as the source of the returned [`Error`], of kind [`ErrorKind::InvalidState`], so a
/// caller can recover it with [`Error::downcast`].
#[derive(Debug, thiserror::Error)]
#[error(
	"sync baseline DB is at schema version {found}, but this build reads only version \
	 {expected}: refusing to open it (it was not written by this engine; remove the baseline DB \
	 to re-sync from scratch)"
)]
pub struct BaselineSchemaMismatch {
	/// The baseline DB that was asked for.
	pub path: PathBuf,
	/// The schema version the DB is stamped at (its `PRAGMA user_version`).
	pub found: i64,
	/// The one schema version this build reads.
	pub expected: i64,
}

/// An exclusive OS lock on `<db>-lock`, the file beside a baseline DB that keeps every other engine
/// off it for as long as the lock is held (see [`BaselineLock::take`]).
struct BaselineLock(std::fs::File);

impl BaselineLock {
	/// Take the lock on the baseline DB at `db_path`, or go on without one where the platform has no
	/// file locks.
	///
	/// The lock is on a file of its own rather than on the DB, which SQLite locks in its own way: on
	/// Windows, where a lock is mandatory, one over the whole DB would refuse SQLite's own reads and
	/// writes. The OS lets the lock go when the file is closed, by a process that crashed as well, so
	/// a crash leaves no stale lock. The file stays behind: removing it would let one engine lock the
	/// file it had opened, no longer the one at the path, while another locks the new one there.
	///
	/// Where file locks are unsupported, std says so with [`std::io::ErrorKind::Unsupported`]: on
	/// Android on the pinned toolchain, or on a filesystem without locks. That is logged and answered
	/// with `None`, so nothing keeps a second engine off the baseline there; refusing instead would
	/// refuse every open on a target the engine is kept building for.
	fn take(db_path: &Path) -> Result<Option<Self>, Error> {
		let mut path = db_path.as_os_str().to_owned();
		path.push("-lock");
		let path = PathBuf::from(path);
		let file = std::fs::File::options()
			.create(true)
			.write(true)
			.truncate(false)
			.open(&path)
			.map_err(|error| lock_file_error(error, "opening", &path))?;
		Self::settle(file.try_lock(), file, db_path, &path)
	}

	/// What `locked`, the answer `file` gave to [`std::fs::File::try_lock`], means for the engine
	/// opening the baseline DB at `db_path` (see [`Self::take`]).
	fn settle(
		locked: Result<(), std::fs::TryLockError>,
		file: std::fs::File,
		db_path: &Path,
		lock_path: &Path,
	) -> Result<Option<Self>, Error> {
		match locked {
			Ok(()) => Ok(Some(Self(file))),
			Err(std::fs::TryLockError::WouldBlock) => Err(Error::custom_with_source(
				ErrorKind::IO,
				BaselineInUse {
					path: db_path.to_path_buf(),
				},
				None::<&str>,
			)),
			Err(std::fs::TryLockError::Error(error))
				if error.kind() == std::io::ErrorKind::Unsupported =>
			{
				tracing::warn!(
					"sync engine: no file lock here ({error}), so nothing keeps a second engine off \
					 the baseline DB {}",
					db_path.display()
				);
				Ok(None)
			}
			Err(std::fs::TryLockError::Error(error)) => {
				Err(lock_file_error(error, "locking", lock_path))
			}
		}
	}
}

impl Drop for BaselineLock {
	/// Unlocks before the file closes rather than leaving that to the close: Windows lets go of a
	/// lock it finds on a closing file only when its resources allow, and asks a process to unlock
	/// what it locked itself. An engine dropped and opened again at once relies on the lock being
	/// gone.
	fn drop(&mut self) {
		if let Err(error) = self.0.unlock() {
			tracing::warn!(
				"sync engine: could not unlock the baseline's lock file, which its close lets go of \
				 instead: {error}"
			);
		}
	}
}

/// An error opening or locking the sync engine's lock file at `lock_path`, naming it.
fn lock_file_error(error: std::io::Error, doing: &str, lock_path: &Path) -> Error {
	Error::custom_with_source(
		ErrorKind::IO,
		error,
		Some(format!(
			"{doing} the sync engine's lock file {}",
			lock_path.display()
		)),
	)
}

impl SyncEngine {
	/// Open the engine, creating the baseline DB at `db_path` if needed. The `client`'s cache must
	/// be configured (it supplies the remote view).
	///
	/// One engine at a time has a baseline DB open, in this process or any other: while one does,
	/// this fails with an [`ErrorKind::IO`] error carrying a [`BaselineInUse`]. The engine holds an OS
	/// lock on the file `<db_path>-lock` until it is dropped; the file itself stays. Where the
	/// platform has no file locks (Android, on this toolchain), the engine opens without one and
	/// logs a warning, and a second engine is not refused.
	///
	/// A baseline DB stamped at a schema version this build does not read is refused with an
	/// [`ErrorKind::InvalidState`] error carrying a [`BaselineSchemaMismatch`], and left as it was.
	pub async fn open(client: Arc<Client>, db_path: PathBuf) -> Result<Self, Error> {
		// First, so an engine that is refused touches nothing of the baseline another one holds.
		let lock = BaselineLock::take(&db_path)?;
		let (store, lock) = {
			let db_path = db_path.clone();
			tokio::task::spawn_blocking(move || {
				let store = BaselineStore::open(&db_path)?;
				// Before this engine has made a stage of its own, so none found is this engine's.
				stage::sweep(&db_path);
				// Handed back rather than held by `open`, so an open cancelled while this runs still
				// holds the lock until its sweep is done.
				Ok::<_, Error>((store, lock))
			})
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
			bin_pruned: Mutex::new(HashMap::new()),
			legacy_trash_looked: Mutex::new(HashMap::new()),
			user_ignore_changed: tokio::sync::watch::channel(0).0,
			stages: AtomicU64::new(0),
			_lock: lock,
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
	/// Best-effort: a root the cache refuses (deleted, trashed, unreachable) leaves that pair on the
	/// [`PENDING_CREATE_GRACE`] fallback rather than failing the whole engine, until a pass
	/// subscribes it again (see [`resubscribe`](Self::resubscribe)). The watch loop registers its
	/// own subscription for wakeups; the two are independent.
	async fn observe_pair(&self, pair: PairId, remote_root: Uuid) {
		if let Err(error) = self.subscribe(pair, remote_root).await {
			tracing::warn!(
				"sync pair {pair}: cache notifications are unavailable ({error}); its pending writes will retire on the grace window alone"
			);
		}
	}

	/// [`observe_pair`](Self::observe_pair), answering the cache's refusal instead of logging it.
	async fn subscribe(&self, pair: PairId, remote_root: Uuid) -> Result<(), Error> {
		let changes = self.pair_changes(pair).await;
		let registered = Arc::new(());
		let subscription = Arc::downgrade(&registered);
		let callback =
			observation_callback(Arc::clone(&self.observed), Arc::clone(&changes), registered);
		match Arc::clone(&self.client)
			.add_sync_root(remote_root, callback)
			.await
		{
			Ok(handle) => {
				self.roots.lock().await.insert(
					pair,
					Subscription {
						_handle: handle,
						registered: subscription,
					},
				);
				Ok(())
			}
			Err(error) => {
				// Without the subscription no remote change is announced at all, so no pass can
				// narrow its remote half down: every one reads the whole subtree, as they all do
				// today.
				changes.degrade(FullPassReason::RemoteEventsDegraded);
				Err(error)
			}
		}
	}

	/// Subscribe `pair` to the cache again if it has no subscription: one the cache refused when the
	/// pair was added or the engine opened (offline, say) is otherwise never asked for again, and
	/// the pair goes without remote announcements for the engine's life. Answers the cache's
	/// refusal if it refuses again; nothing is asked of it while the pair is subscribed.
	///
	/// A subscription the cache ended under its handle (see [`Subscription::live`]) counts as
	/// none, and is dropped before the cache is asked again. For a root the server deleted that
	/// answers the refusal a restarted engine gets, at once, rather than a wait for a root the
	/// cache no longer tracks; a root restored from the trash since is subscribed again.
	pub(super) async fn resubscribe(&self, pair: PairId) -> Result<(), Error> {
		{
			let mut roots = self.roots.lock().await;
			match roots.get(&pair) {
				Some(subscription) if subscription.live() => return Ok(()),
				Some(_) => {
					tracing::warn!(
						"sync pair {pair}: the cache ended its subscription (the remote folder was deleted, or the cache shut down); asking for it again"
					);
					roots.remove(&pair);
				}
				None => {}
			}
		}
		let remote_root = locked(&self.control)
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?
			.remote_root;
		self.subscribe(pair, remote_root).await
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
		// will not supply refuses the registration rather than let a pair through unchecked. The one
		// exception is a registered root the server reports gone, which has no chain to compare.
		let account_root = self.client.root().uuid();
		let candidate_chain = self.server_ancestry(remote_root, account_root).await?;
		for record in &others {
			let existing_chain = match self.server_ancestry(record.remote_root, account_root).await
			{
				Ok(chain) => chain,
				// Gone from the server: a folder that does not exist neither contains the candidate
				// nor sits inside it. Only a registered root gets this leniency; the candidate's own
				// walk above still refuses a root the server does not have.
				Err(error) if error.kind() == ErrorKind::FolderNotFound => {
					tracing::warn!(
						"add_pair: sync pair {}'s remote root {} is gone from the server ({error}); it relates to nothing",
						record.id,
						record.remote_root
					);
					continue;
				}
				Err(error) => return Err(error),
			};
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
	/// chain above it to compare against. A remote root someone else deleted reads as trashed until
	/// the backend purges it, so the walk yields the root alone, and as not found after that, which
	/// [`check_roots_free`](Self::check_roots_free) passes over for a registered pair. Either way
	/// that pair relates to nothing and does not wedge every later registration (CONTROL-09c).
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

	/// Register a sync pair, returning its id. `remote_root` is the drive root or a directory in
	/// it. A pair rooted at the drive root never syncs the `.filen` directory at its top, in any
	/// case, which Filen's apps keep the account's own data in.
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

	/// Import what the filen-sync pair `pair` replaces last recorded as synced, from that pair's
	/// state directory: `<dbPath>/state/v2/<pairUuid>/` of the old engine, which the caller resolves.
	/// Only its `previousLocalTree` and `previousRemoteTree` are read (see [`LegacyImport`] for
	/// what is counted).
	///
	/// Call it after [`add_pair`](Self::add_pair) and before the pair's first pass. What it imports
	/// is evidence for the paths this engine has no baseline row for, so it is refused once the
	/// pair has any row: by then the pair's own rows are the better record. A second import before
	/// that replaces the first. A state directory without both tree files is the old engine's own
	/// "nothing saved", and imports nothing.
	///
	/// A pass then records as synced a hashless remote file the import lists, at a path with no row,
	/// while the remote is still the version the old engine listed there and the local file still
	/// has the size and mtime it saw; anything else at the path is weighed as if nothing had been
	/// imported.
	///
	/// Waits for a pass still reading the pair, as [`resolve_conflict`](Self::resolve_conflict)
	/// does. A pass that read the pair before the import does not see it; its apply may then write
	/// rows after the import found none, which only leaves the evidence unused at those paths.
	///
	/// Errors if the pair is unknown, is being removed or has a baseline row, if `state_dir` is not
	/// a directory that can be read or a tree file in it cannot be read to the end
	/// ([`ErrorKind::IO`]), or if the store write fails.
	pub async fn import_legacy_state(
		&self,
		pair: PairId,
		state_dir: PathBuf,
	) -> Result<LegacyImport, Error> {
		// The gate refuses an unknown pair and keeps a removal waiting; the reading lock keeps the
		// import out of a pass's read. The retirement is read after the lock, as
		// `resolve_conflict` reads it.
		let gate = self.pass_gate(pair).await?;
		let _reading = self.reading_lock(pair).await.lock_owned().await;
		if gate.retired() {
			return Err(being_removed(pair));
		}
		let store = self.pair_store(pair).await?;
		// Off the runtime thread: the trees run to a line per item of the old pair.
		off_store(&store, move |store| {
			if has_synced(store, pair)? {
				return Err(Error::custom(
					ErrorKind::InvalidState,
					format!(
						"sync pair {pair} has already synced: filen-sync state is imported only \
						 before its first pass"
					),
				));
			}
			legacy::import(store, pair, &state_dir)
		})
		.await?
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
	/// scan, or a remote view that has never converged or came back wholly empty — or if the read
	/// fails, a failed baseline read included (see [`plan_pair`](Self::plan_pair)). A remote folder
	/// the cache has not finished listing fails it with [`ErrorKind::StaleState`] at once, as it
	/// fails [`plan_pair`](Self::plan_pair): unlike a pass, the call does not wait for the listing.
	/// The mode is left exactly as it was, so the call is safe to retry. Re-registering an existing pair through
	/// [`add_pair`](Self::add_pair) with a different mode is an error rather than a silent switch,
	/// so a mode change is always this explicit call.
	pub async fn reconfigure_pair(
		&self,
		pair: PairId,
		mode: SyncMode,
		backlog: Backlog,
	) -> Result<(), Error> {
		let reconfigured = self.reconfigure_pair_unreleased(pair, mode, backlog).await;
		// An adoption read the rows a pass reads, through the same parked reader, and wrote the rows
		// it adopted through the pair's writer: what those cached goes now, whatever came of the
		// switch, as a pass's does when it ends (see `release_pair_cache`). A plain switch writes one
		// row and reads none.
		if backlog == Backlog::AdoptDestination {
			self.release_pair_cache(pair).await;
		}
		reconfigured
	}

	/// [`reconfigure_pair`](Self::reconfigure_pair), up to giving back the page caches an adoption
	/// filled.
	async fn reconfigure_pair_unreleased(
		&self,
		pair: PairId,
		mode: SyncMode,
		backlog: Backlog,
	) -> Result<(), Error> {
		// Read both sides before touching the mode: the adoption is anchored to what the
		// destination holds NOW, and the switch must not take effect without it.
		let adopted = match backlog {
			Backlog::Propagate => Vec::new(),
			// A read that fails panics as a pass's does, and is this call's error as a pass's is.
			Backlog::AdoptDestination => {
				pass_panics_as_errors(pair, "the adoption read", async {
					// A read, not a pass: it seeds nothing, and weighs nothing either. A seeded row is
					// for a path both sides hold, and the adoption writes rows only where one side has
					// nothing, so what the stage would record changes nothing it adopts.
					let prep = self.prepare(pair, None, Seeding::Skip).await?;
					// The adoption rewrites baseline rows from this one read, and every row it writes
					// is for a path it reads as GONE on the source side — so it needs the same
					// evidence a pass needs before acting on an absence. Read through an incomplete
					// scan or an unconverged/transiently-empty remote view it does the opposite of
					// what was asked: it adopts nothing and the switch then propagates the very
					// backlog the caller wanted kept, or it adopts a whole pair from whichever side
					// still lists something and drops the other side's anchors. Refuse instead — the
					// mode is left as it was, and the caller can retry once the view is healthy.
					if let Some(reason) = adoption_refusal(refusal(&prep), &screen_state(&prep)) {
						return Err(Error::custom(
							ErrorKind::InvalidState,
							format!("cannot adopt the destination: {reason}"),
						));
					}
					Ok(plan::adopt_destination_rows(
						mode,
						&prep.baseline,
						&prep.local_scan.nodes.of(&prep.baseline),
						&prep.remote_view.nodes.of(&prep.baseline),
					))
				})
				.await?
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
	/// walking `.filen-sync-trash`'s day buckets and guessing among ` (N)` siblings — the same thing
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
				let moved = apply::rename_aside(
					Path::new(&record.local_root),
					self.pair_root(record.remote_root),
					rel_path,
				)?;
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
			quarantined = bin::quarantine_local(
				Path::new(&record.local_root),
				rel_path,
				Utc::now().date_naive(),
			)?;
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
				let (aside, path) =
					apply::aside_target(local_root, self.pair_root(record.remote_root), rel_path)?;
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
				quarantined = bin::quarantine_local(local_root, rel_path, Utc::now().date_naive())?;
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

	/// Confirm every push of ours `baseline` holds unconfirmed that the evidence vouches for, and
	/// hand back the rows that moved, each as it now stands, for the caller to put into its view of
	/// the rows ([`Baseline::confirm`]) and to persist — and to retire their push records with
	/// ([`Observations::forget_pushes`]) once it has — along with the evidence, whose side the
	/// caller takes back ([`Evidence::put_back`]). Reading a verdict does not consume it: `prepare`
	/// runs for [`plan_pair`](Self::plan_pair) too, which writes nothing.
	///
	/// A row is confirmed by any of, cheapest first:
	/// - rule (c), on a whole pass's raw snapshot only ([`plan::snapshot_confirms`]): the snapshot
	///   lists the row's own version at its path. It must be the RAW snapshot: a view with this
	///   engine's own writes folded in shows our version at its own path and would confirm every
	///   push against itself.
	/// - the cache's announcements. A version of ours that stood as the remote head for
	///   [`CONFIRM_TENURE`] before anything superseded it is content both sides held, whether or not
	///   a pass ever happened to run while it was the head. This is what confirms the backlog a
	///   PAUSED pair accumulated: no pass runs, so nothing else ever could.
	/// - the server's version chain, for a row the announcements say nothing about (this engine was
	///   restarted, or the announcement never arrived) whose path now carries a foreign version of
	///   the same file. The two versions' timestamps say how long ours stood. One lookup per such
	///   row, and only for a row that is actually about to be reconciled against a foreign version;
	///   a lookup that fails leaves the row unconfirmed, exactly as before.
	///
	/// The last two vouch for a version uuid, and every unconfirmed row carrying it is confirmed
	/// ([`plan::push_vouched`]).
	///
	/// `window` rows at a time: the rows are read a page at a time, asked about, and what the
	/// evidence accepts is added to the confirmed rows ([`stage::ConfirmedRows`]), which move into
	/// a stage of the pass's own past a window of them. On the pass after a whole-tree push the
	/// unconfirmed rows are the tree, so held whole they were the tree in memory — twice over, the
	/// rows and the markers. A version the chain vouches for after the page of a row carrying it
	/// was asked about confirms that row on a second read of the rows, which runs only when the
	/// server vouched for anything.
	///
	/// Each page is read and staged on a blocking thread (see [`Confirming`]), and only the
	/// server's chain is asked on the caller's: on that same pass, reading and staging the tree's
	/// rows takes seconds.
	async fn confirm(
		&self,
		pair: PairId,
		baseline: &Baseline,
		evidence: Evidence,
		window: NonZeroUsize,
	) -> Result<(Confirmed, Evidence), Error> {
		let at = self.stage_at(pair);
		let mut confirming = Confirming::new(
			// Shares the snapshot and the pass's edits, for the blocking threads to read.
			baseline.clone(),
			evidence,
			Arc::clone(&self.observed),
			// The path is cloned so the maker can be called again; a confirmation stages its rows
			// once at most, past its window.
			move || Stage::create(at.clone()),
			window,
		);
		loop {
			let asking;
			(confirming, asking) = off_runtime("confirming this engine's pushes", move || {
				let asking = confirming.page();
				(confirming, asking)
			})
			.await?;
			let Some(asking) = asking? else {
				break;
			};
			if !asking.is_empty() {
				let vouched = self.date_by_version_chain(asking).await;
				confirming.chained.extend(vouched);
			}
		}
		off_runtime("confirming this engine's pushes", move || {
			confirming.finish()
		})
		.await?
	}

	/// The version uuids of ours the server's version chain vouches for, out of `ask_server` —
	/// `(rel_path, ours, the foreign version at the path)` per row the announcements say nothing
	/// about (see [`observed_confirmations`]). A page's worth at a time: the foreign versions are
	/// read whole in one go, then each chain is one lookup.
	async fn date_by_version_chain(&self, ask_server: Vec<(String, Uuid, Uuid)>) -> HashSet<Uuid> {
		let mut confirmed = HashSet::new();
		if ask_server.is_empty() {
			return confirmed;
		}
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
		confirmed
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
	/// [`confirm`](Self::confirm) on the announcements alone: with no snapshot there is no foreign
	/// version to date a push against, so nothing here talks to the server. A pass does this
	/// itself; this is for the stretches where none runs.
	async fn sweep_confirmations(&self, pair: PairId) -> Result<(), Error> {
		let store = self.pair_store(pair).await?;
		let baseline = off_store(&store, move |store| store.baseline(pair))
			.await?
			.map_err(|e| db_error(e, "loading the baseline"))?;
		// A pair with nothing awaiting confirmation is the steady state, and the sweep runs in the
		// stretches where no pass does: it must not read a row to find that out. The
		// gate `prepare` uses, and the filter every confirmation step already applies.
		if !baseline.any_unconfirmed() {
			return Ok(());
		}
		let advanced = self
			.confirm(pair, &baseline, Evidence::Announcements, stage::WINDOW)
			.await
			.map(|(advanced, _)| advanced);
		// Its read transaction ends here, not after the write below: nothing reads the rows again.
		drop(baseline);
		let swept = match advanced {
			Ok(advanced) if advanced.is_empty() => Ok(()),
			Ok(advanced) => self.persist_confirmed(pair, &store, advanced).await,
			Err(error) => Err(error),
		};
		// It read every unconfirmed row, and wrote what it confirmed: what that cached goes now, as a
		// pass's does when it ends, rather than when the pair's next pass ends (see
		// `release_pair_cache`).
		self.release_pair_cache(pair).await;
		swept
	}

	/// Write the rows a confirmation advanced into the store in ONE transaction, read out of where
	/// it put them a page at a time — and only then retire the push records they name, plus any
	/// record no pass will ever read again (see [`PUSH_RECORD_TTL`]).
	///
	/// One transaction for the lot: the pass after a first sync confirms every row it pushed, and a
	/// transaction per row is a transaction per item of the tree — which is also why it does not run
	/// on a runtime thread. The records go only once the rows are written: a record is the only
	/// evidence a push was ever confirmed, and dropping it without recording what it proved leaves
	/// the next pass reading the row as an unconfirmed push again — with a foreign edit on top, a
	/// false conflict.
	async fn persist_confirmed(
		&self,
		pair: PairId,
		store: &SharedStore,
		confirmed: Confirmed,
	) -> Result<(), Error> {
		if confirmed.is_empty() {
			self.observed.forget_pushes(&[], Instant::now());
			return Ok(());
		}
		let observed = Arc::clone(&self.observed);
		off_store(store, move |store| {
			store
				.upsert_streamed(pair, |put| {
					confirmed.each_page(|rows| rows.iter().try_for_each(&mut *put))
				})
				.map_err(|e| db_error(e, "recording the confirmed agreed content"))?;
			// The rows are written, so a record this cannot read back to drop is only memory kept:
			// it goes with the push record TTL, and the pass carries on.
			if let Err(error) = confirmed.each_page(|rows| {
				observed.drop_pushes(rows.iter().filter_map(|row| row.remote_uuid));
				Ok(())
			}) {
				tracing::warn!(
					"sync_once[pair {pair}]: could not read back the confirmed rows to retire their \
					 push records, which now wait out their TTL: {error}"
				);
			}
			observed.forget_pushes(&[], Instant::now());
			Ok::<_, Error>(())
		})
		.await?
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

	/// Wait until the cache holds `pair`'s remote root converged — a whole, current view a pass may
	/// plan against (see `SnapshotMark::converged`) — and answer the generation it holds it at.
	///
	/// At once when it already does, which is every pass but a pair's first after the cache started
	/// and those during a relist. Otherwise `observer` hears [`SyncEvent::AwaitingRemote`] once, and
	/// this asks again every [`CONVERGENCE_POLL`] for up to [`CONVERGENCE_WAIT`], then fails with
	/// [`ErrorKind::StaleState`]. A pair with no cache subscription has a root the cache may not
	/// track, which would never converge: it is subscribed again before the wait (see
	/// [`resubscribe`](Self::resubscribe)), and a refusal fails the wait at once, as that error.
	/// `give_up` resolving fails it at once with [`ErrorKind::StaleState`] too, as a wait that ran
	/// out does.
	pub(super) async fn await_remote_view(
		&self,
		pair: PairId,
		observer: Option<&mut (dyn FnMut(SyncEvent) + Send)>,
		give_up: impl Future<Output = ()>,
	) -> Result<u64, Error> {
		let remote_root = locked(&self.control)
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?
			.remote_root;
		let before_waiting = async {
			self.resubscribe(pair).await?;
			if let Some(observer) = observer {
				observer(SyncEvent::AwaitingRemote);
			}
			Ok(())
		};
		await_convergence(
			|| self.client.sync_root_converged(remote_root),
			before_waiting,
			CONVERGENCE_POLL,
			CONVERGENCE_WAIT,
			give_up,
		)
		.await?
		.ok_or_else(|| remote_not_converged(pair, remote_root))
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
		// The pair's three reads in ONE hop off the runtime thread. The baseline is a snapshot of the
		// table — a read transaction on a reader connection and the pair's counts, read nothing else
		// (see [`BaselineStore::baseline`]); the other two ride along rather than paying for a hop
		// each.
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
	/// `scope` is the pass's changelists, taken before this runs, with the convergence they were
	/// taken under. `None` is a dry run, which always reads whole: it is rare, and its report has to
	/// show the whole picture (plan 3.5).
	///
	/// `seeding` says whether the read weighs the hashless files both sides agree on before it hands
	/// the pass over (see [`seed`]), and where it records them: only a pass that goes on to apply
	/// writes them into the store, and a dry run reads them into its own view.
	async fn prepare(
		&self,
		pair: PairId,
		scope: Option<ScopedRead<'_>>,
		seeding: Seeding,
	) -> Result<Prepared, Error> {
		let inputs = self.pass_inputs(pair).await?;
		// The per-pass DB prologue, bounded on its own: five statements and a `spawn_blocking` hop,
		// which read as part of the first phase inside `prepare_scoped` until this mark existed.
		super::step("pass_inputs");
		// The changelists' own reasons, plus the one row of the trigger table that needs the
		// baseline: an empty one is a first sync, and the whole tree is the evidence for it.
		let forced = match &scope {
			Some(read) => read.scope.full_pass_reason(inputs.baseline.len()),
			None => None,
		};
		if forced.is_none()
			&& let Some(read) = scope
		{
			match self.prepare_scoped(inputs, read, seeding).await? {
				Scoped::Prepared(prepared) => return Ok(*prepared),
				// The derivation could not answer for something it was handed.
				Scoped::Whole(inputs, reason) => {
					return self
						.prepare_whole(*inputs, PassRead::Whole(Some(reason)), seeding)
						.await;
				}
			}
		}
		self.prepare_whole(inputs, PassRead::Whole(forced), seeding)
			.await
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
		ScopedRead { scope, converged }: ScopedRead<'_>,
		seeding: Seeding,
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
		let pair_root = self.pair_root(root);
		let for_remote = inputs.baseline.clone();
		let nodes = mem::take(&mut derived.remote);
		// The delta comes back out with the observation: `observe_remote` only borrows it, and a
		// pass cut short hands it to the next one (see `Prepared::remote_delta`).
		let pair_store = Arc::clone(&inputs.store);
		let (observed, delta) = tokio::task::spawn_blocking(move || {
			let mut ancestry = |uuid| cache_ancestry(&cache_db, uuid);
			// Where the baseline records every item this delta can name, resolved off the store's
			// `(pair_id, remote_uuid)` index in ONE hold of the pair's lock, on the blocking thread
			// the derivation already runs on. The resident `Uuid -> row` map this replaces cost
			// 36.75 MiB at a million rows and answered exactly these lookups; an empty delta names
			// no id and runs no statement at all, which is the idle pass's shape.
			let observed = locked(&pair_store)
				.synced_paths(pair, &delta_uuids(&delta), &[])
				.map(|placed| {
					observe_remote(
						root,
						pair_root,
						&for_remote,
						nodes,
						&delta,
						placed,
						&mut ancestry,
					)
				});
			(observed, delta)
		})
		.await
		.map_err(|e| {
			Error::custom(
				ErrorKind::Internal,
				format!("deriving the remote view panicked: {e}"),
			)
		})?;
		let observed =
			observed.map_err(|e| db_error(e, "resolving where the delta's items are recorded"))?;
		// The delta is what the cache announced up to the take, which came after the cache held
		// the root converged at `converged`. A resync that started since — the generation moved, or
		// the root is not converged at all — may have announced part of its listing before the
		// take and the rest after it, and the rows derived here cannot tell the two apart. A root
		// that is not converged may also have taken an event its rows cannot follow (a dir moved in
		// or restored), which only its relist makes whole.
		if self.client.sync_root_converged(root).await? != Some(converged) {
			return Err(remote_not_converged(pair, root));
		}
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
		let rule_files = RuleFiles::for_pass(
			inputs.record.mode,
			&inputs.baseline,
			&view,
			remote_rules.root_body.as_ref(),
		);

		super::step("rule_files_only");

		// The local half: one stat per dirty path and its ancestors, one subtree walk per dirty
		// directory. The rules come back carrying only the `.filenignore` files this observation
		// read — the branches it visited — which is why the carried `ignored_remote` facts still
		// stand for everything it did not.
		let local_root = PathBuf::from(&inputs.record.local_root);
		let for_local = inputs.baseline.clone();
		let dirty = mem::take(&mut derived.dirty);
		let pass_rules = remote_rules.rules;
		let observed = tokio::task::spawn_blocking(move || {
			observe::observe_local(
				&local_root,
				pair_root,
				&for_local,
				pass_rules,
				&rule_files,
				&dirty,
			)
		})
		.await
		.map_err(|e| {
			Error::custom(
				ErrorKind::Internal,
				format!("re-observing the local paths panicked: {e}"),
			)
		})?;
		// A dirty path with a subtree too large to narrow this pass to.
		let (mut observations, rules) = match observed {
			Ok(observed) => observed,
			Err(reason) => return Ok(Scoped::Whole(Box::new(inputs), reason)),
		};
		super::step("observe_local");
		// Plan 3.6's self-check, bounded before the merge empties the walks it counts and checked
		// before anything plans against these maps.
		let accountable = assembly_bounds(&inputs.baseline, &observations, &held_rows);
		derive::merge_local(&mut derived, &inputs.baseline, &mut observations);
		super::step("merge_local");
		if !accountable.contains(&derived.local.of(&inputs.baseline).len()) {
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
		// the reconcile decides nothing at them. `Blocking` still drops anything planned at or
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
		// A derived view SKIPS nothing (`skipped` is empty by construction above), so
		// `unknown_remote_paths` returns before it looks anything up and there is nothing to
		// resolve for it. Asserted in EVERY build rather than left to the comment: a skipped item
		// arriving here would be looked up in an empty `SyncedPaths`, which answers "no row records
		// this" for an item whose row is right there — and that answer plans a local delete over a
		// remote item that still exists.
		assert!(
			view.skipped.is_empty(),
			"a change-scoped view skipped {} remote item(s), with nothing resolved to match them \
			 against the rows that record them",
			view.skipped.len()
		);
		facts.merge_remote_view(
			&observation.touched,
			&view,
			&inputs.baseline,
			&SyncedPaths::default(),
		);
		facts.observe_local(&observations);
		super::step("facts_merge");

		// Tenure and the server's version chain ONLY, never rule (c) (see `Evidence::Derived`).
		let mut confirmed = Confirmed::default();
		if inputs.baseline.any_unconfirmed() {
			// Read off the rows unedited, and put into them once read: a change-scoped view derives
			// its nodes from those very rows, so it cannot be read across the confirmation's edits.
			// Only the unconfirmed rows' own paths are looked up, each against the row in hand, so
			// this is the per-push work the confirmation already was, never the tree.
			let lent = Evidence::Derived(mem::replace(&mut view.nodes, Side::carried()));
			let (rows, lent) = self
				.confirm(pair, &inputs.baseline, lent, stage::WINDOW)
				.await?;
			lent.put_back(&mut view.nodes);
			confirmed = rows;
		}
		super::step("confirm_pushes");

		let mut holds = {
			let journal = locked(&inputs.store);
			let before = self.pending.uuids();
			// An EMPTY map, deliberately: `settle`'s third rule retires a write whose item the
			// SNAPSHOT shows at its path, and this pass has no snapshot. The derived view shows our
			// own write because the baseline row records it, so reading it there would retire the
			// record against itself (I5). The announcement rule and the grace ceiling still apply.
			let holds = self.pending.settle(
				pair,
				&announced,
				&Side::<RemoteNode>::default().of(&inputs.baseline),
			);
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
		// run: `assembly_bounds` above bounds what the maps HOLD, and this asks whether the pass
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
		// run, and the `confirmed` rows this pass found are dropped before they reach its view of
		// the rows. A whole pass redoes all three against a real snapshot, which is the stricter
		// evidence anyway — the cost is one wasted settle/retire round trip.
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
		// Past the last fallback to a whole read, which starts from the rows as the store holds them
		// and confirms on them itself: nothing before here reads a row's agreed content.
		inputs.baseline.confirm(&confirmed);
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
			// Left to the next whole read, which walks every directory a leftover can sit in: the
			// watcher announces no temp file, so a change-scoped pass is never asked to look at one.
			stale_downloads: Vec::new(),
		};
		let remote_emptied = remote_emptied(&view.nodes.of(&inputs.baseline), &inputs.baseline);
		let store = Arc::clone(&inputs.store);
		let prepared = Prepared {
			record: inputs.record,
			tracked_at_start: inputs.baseline.len(),
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
		// Before the fold, whose layers a fresh snapshot of the rows would lose.
		let mut prepared = prepared.seeded(&store, seeding).await?;
		if let Err(reason) = prepared.fold_dir_moves() {
			// The fold may already have moved rows in this pass's view of them, so the whole read
			// starts from the store's own — once this pass's maps and snapshot are let go. Those
			// include any rows this pass seeded, so the whole read counts them as tracked: it cannot
			// make a first sync of this one, which a change-scoped pass never is (an empty baseline
			// always reads whole).
			drop(prepared);
			return Ok(Scoped::Whole(
				Box::new(self.pass_inputs(pair).await?),
				reason,
			));
		}
		super::step("fold_dir_moves");
		Ok(Scoped::Prepared(Box::new(prepared)))
	}

	/// The whole-tree half of [`prepare`](Self::prepare): read both sides entire, which is the only
	/// read that can find a change nothing announced.
	async fn prepare_whole(
		&self,
		inputs: PassInputs,
		read: PassRead,
		seeding: Seeding,
	) -> Result<Prepared, Error> {
		let PassInputs {
			record,
			user_ignore,
			store,
			mut baseline,
			failures,
			last_ignored,
		} = inputs;
		let pair = record.id;
		let pair_root = self.pair_root(record.remote_root);

		// Copied BEFORE the snapshot is read: an event the cache commits afterwards describes a
		// state this snapshot predates, so it must not retire a pending write this pass.
		let observed = self.observed.snapshot();
		// The scan reads a row's kind, size, mtime and hash, none of which a confirmation changes, so
		// it reads the rows as the pass found them: its walk of the tree reads no marker.
		let scan_baseline = baseline.clone();
		// A mode that pushes reads every `.filenignore` on disk, whatever the remote holds, so the one
		// thing its walk needs from the remote is the rule files read there for a directory the disk
		// has none in, and almost every pair has none. So its walk starts now, with the user's rules
		// alone, and runs alongside the remote read below; the rules that read comes back with decide
		// whether it stands. A mode that pulls alone needs the view to know which rule files to read
		// on disk, so it walks after the read.
		let early_walk = if record.mode.pushes() {
			// Parsed again for the remote rules below, as an `IgnoreSource` is not `Clone`. A text that
			// does not parse is no rules on both, and blocks the pair below.
			let user = parse_user_ignore(&user_ignore).ok();
			Some(LocalWalk::start(
				self.stage(pair)?,
				PathBuf::from(&record.local_root),
				pair_root,
				scan_baseline.clone(),
				IgnoreRules::new(user),
				RuleFiles::Read,
			))
		} else {
			None
		};
		// One view, built once and unfiltered at first: what the snapshot confirms, which remote
		// rule files there are, which of this engine's own writes the cache has caught up to, and
		// whether the remote reads as emptied are facts about the remote as it is, whatever the
		// rules hide — and the rules cannot be known until the scan below has read the
		// `.filenignore` files on disk. Those reads come first; then the very rules the scan
		// matched with hide what they hide, in place (see `RemoteView::filter`).
		//
		// Copied from the cache's rows AS THEY ARRIVE into the pass's stage, inside the cache's one
		// read transaction, and placed there once the copy is done (see `stage::remote`): the view
		// is a row per item of the tree on disk, where a map held a node per item in memory from
		// here to the end of the plan.
		//
		// A read that fails partway takes the stage with it (see `stream_sync_root_snapshot`):
		// this pass gets an error, never a view with holes in it. So does a stage that cannot be
		// written (a full disk): the sink keeps the error of the first row it could not write, and
		// placing the copy returns it.
		let remote = async {
			let stage = self.stage(pair)?;
			let at = stage.path().display().to_string();
			let sink = stage::RemoteSink::new(stage, stage::Paging::PASS)
				.map_err(|e| db_error(e, &format!("staging the remote snapshot in {at}")))?;
			let (copied, mark) = self
				.client
				.stream_sync_root_snapshot(record.remote_root, sink)
				.await?;
			// A root the cache has not finished listing is not a view of the remote: every item it
			// is still missing reads as absent, which plans an upload over newer remote content (a
			// same-name upload replaces the remote file) as readily as a deletion. Read in the same
			// transaction as the rows, so a resync that starts after this read cannot pass.
			if !mark.converged {
				return Err(remote_not_converged(pair, record.remote_root));
			}
			// Every row the pass still needs is in the view now; the objects it ACTS on are read back
			// whole by uuid, for the handful a plan names.
			let root = record.remote_root;
			let mut view = tokio::task::spawn_blocking(move || copied.finish(root, pair_root))
				.await
				.map_err(|e| {
					Error::custom(
						ErrorKind::Internal,
						format!("placing the remote snapshot panicked: {e}"),
					)
				})?
				.map_err(|e| db_error(e, &format!("staging the remote snapshot in {at}")))?;

			// The confirmation runs against the RAW view — before this engine's own writes are
			// folded into it. A row the snapshot confirms is one both sides demonstrably hold, which
			// is what a later foreign edit is measured against. Asked before a row is read: the
			// confirmation advances the rows an unconfirmed push left behind, and a pair with none —
			// the steady state — must not read a row to find that out.
			let mut confirmed = Confirmed::default();
			if baseline.any_unconfirmed() {
				let lent = Evidence::Snapshot(mem::replace(&mut view.nodes, Side::carried()));
				let (rows, lent) = self.confirm(pair, &baseline, lent, stage::WINDOW).await?;
				lent.put_back(&mut view.nodes);
				confirmed = rows;
			}
			Ok::<_, Error>((view, mark.watermark.is_some(), confirmed))
		}
		.await;
		let (mut view, remote_converged, confirmed) = match remote {
			Ok(read) => read,
			Err(error) => {
				if let Some(walk) = early_walk {
					walk.abandon().await;
				}
				return Err(error);
			}
		};
		baseline.confirm(&confirmed);
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
		// Rules come from the side that is the source of truth, plus a pulling pair's local root file
		// (see `RuleFiles::for_pass`).
		let rule_files = RuleFiles::for_pass(
			record.mode,
			&baseline,
			&view,
			remote_rules.root_body.as_ref(),
		);
		// The early walk stands when the remote read added no rule file to the user's rules:
		// `load_remote_rules` starts from those and changes them only by adding one, so the rules it
		// came back with are then the very rules that walk matched with. Otherwise it is given up and
		// the tree walked again, with them.
		let walk = match early_walk {
			Some(walk) if !remote_rules.rules.has_files() => walk,
			early_walk => {
				if let Some(walk) = early_walk {
					walk.abandon().await;
				}
				LocalWalk::start(
					self.stage(pair)?,
					PathBuf::from(&record.local_root),
					pair_root,
					scan_baseline,
					remote_rules.rules,
					rule_files,
				)
			}
		};
		let (mut local_scan, rules) = walk.finish().await?;
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
		//
		// A skipped item has to be matched against the row that records it, or its absence from the
		// view reads as a deletion of a path whose item is still there. The ids come off the
		// store's own indexes now; a view that skipped nothing — the usual shape, and the only one
		// a derived view can have — resolves nothing and runs no statement.
		let (skipped_uuids, skipped_lineages) = plan::skipped_ids(&remote_view.skipped);
		let synced = if skipped_uuids.is_empty() && skipped_lineages.is_empty() {
			SyncedPaths::default()
		} else {
			off_store(&store, move |store| {
				store.synced_paths(pair, &skipped_uuids, &skipped_lineages)
			})
			.await?
			.map_err(|e| db_error(e, "resolving where the skipped remote items are recorded"))?
		};
		facts.merge_remote_view(
			&BTreeSet::from([String::new()]),
			&remote_view,
			&baseline,
			&synced,
		);

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

		let prepared = Prepared {
			record,
			tracked_at_start: baseline.len(),
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
		// Before the fold, whose layers a fresh snapshot of the rows would lose.
		let mut prepared = prepared.seeded(&store, seeding).await?;
		// AFTER the pending-write fold: a move of ours the cache has not shown yet must read as done,
		// not as the remote moving the directory back. On a blocking thread: the fold reads and
		// re-keys both staged sides.
		let (prepared, folded) = off_runtime("folding the pass's directory moves", move || {
			let folded = prepared.fold_dir_moves();
			(prepared, folded)
		})
		.await?;
		folded.expect(
			"a whole read's sides derive nothing from the rows, so no move asks one to stay put \
			 (should be impossible)",
		);
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
	/// persist the confirmations it observed, nor record as synced the hashless remote files a pass
	/// would (see [`sync_once`](Self::sync_once)): it weighs the same evidence at the same paths and
	/// plans from the rows a pass would record, held in its own read alone, so where a pass would
	/// plan nothing at a hashless file that agrees with the local one, so does the dry run. A
	/// [`paused`](Self::pause_pair) pair is planned like any other.
	///
	/// A read that fails is the call's error, as it is a pass's: a baseline read that fails panics
	/// on purpose, and that panic comes back here as an [`ErrorKind::Internal`] error. So is a remote
	/// folder the cache has not finished listing (its first listing, or a relist), which fails with
	/// [`ErrorKind::StaleState`] at once: every item the listing has not reached would plan as
	/// absent, and unlike a pass a dry run does not wait for the listing (see
	/// [`sync_once`](Self::sync_once)), so a caller retries once the cache has finished.
	pub async fn plan_pair(&self, pair: PairId) -> Result<PlanOutcome, Error> {
		let planned =
			pass_panics_as_errors(pair, "the dry run", self.plan_pair_unreleased(pair)).await;
		// The dry run read the rows a pass reads, through the same parked reader: what that cached
		// goes now, as a pass's does when it ends, rather than when the pair's next pass ends (see
		// `release_pair_cache`).
		self.release_pair_cache(pair).await;
		planned
	}

	/// [`plan_pair`](Self::plan_pair), up to giving back the page caches its read filled.
	async fn plan_pair_unreleased(&self, pair: PairId) -> Result<PlanOutcome, Error> {
		let prep = self.prepare(pair, None, Seeding::Preview).await?;
		if let Some(reason) = refusal(&prep) {
			return Ok(PlanOutcome {
				refused: Some(reason),
				..PlanOutcome::default()
			});
		}
		let state = screen_state(&prep);
		let at = self.stage_at(pair);
		// On a blocking thread, as a pass screens its plan: the reconcile, the screening and the
		// lists all read and write the plan's stage.
		off_runtime("planning the pair", move || {
			let mut screened = reconcile_and_screen(&prep, state, || Stage::create(at))?;
			// What the pass would apply, which takes each deletion under a deleted directory along
			// with it.
			screened.fold_subsumed()?;
			Ok(PlanOutcome {
				actions: prep.planned(&screened, Rows::Safe)?,
				held: prep.planned(&screened, Rows::Held)?,
				held_reason: screened.reason,
				pass_token: screened.pass_token,
				conflicts: screened.conflicts,
				unsyncable: prep.unsyncable(),
				ignored: prep.ignored(),
				refused: None,
			})
		})
		.await?
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
	/// either side half-written. This call waits until that pass can no longer write — until its
	/// gate closes, which it holds for every read and write it makes — up to
	/// [`REMOVE_CANCEL_GRACE`], after which the removal proceeds regardless and a straggler's write
	/// fails on its foreign key. The pass's task may still be running its tail when this returns:
	/// giving back its page cache and reporting how it ended, neither of which writes a row. A pass
	/// still READING the two sides is reached too, and waited for:
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
		self.bin_pruned.lock().await.remove(&pair);
		self.legacy_trash_looked.lock().await.remove(&pair);
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
	/// touching the persisted paused flag, and ends one waiting for the cache to finish listing the
	/// pair's remote folder, which then fails with [`ErrorKind::StaleState`] as a wait that ran out
	/// does. A watch being stopped and an engine being closed use it, so a pass ends instead of
	/// waiting for a resume that whoever stopped it is not going to send, or for a listing nobody
	/// is left to sync.
	pub(super) async fn cut_loose_pass(&self, pair: PairId) {
		if let Some(control) = self.paused.lock().await.get(&pair) {
			cancel_suspension(control);
		}
		if let Some(changes) = self.existing_pair_changes(pair).await {
			changes.stop_waiting();
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
	/// same token applies them; any other batch — different paths, or the same paths held for a
	/// different kind of reason — is held again under a fresh token, so an approval can never leak
	/// onto deletions the caller never saw, nor onto a hold it was never told about. The counts a
	/// reason carries are not part of the token: a volume limit that moved because the tracked set
	/// grew is still the same hold.
	///
	/// The pair's next pass is made due and whole, since a hold on its own leaves a watched pair
	/// idle (see [`FullPassReason::DeletionHold`]), and a [`watch`](Self::watch) running on the
	/// pair is woken for it, so the approved batch is applied within the watch's debounce rather
	/// than at its next change or safety-net pass. Without a watch, the next
	/// [`sync_once`](Self::sync_once) is that pass.
	pub async fn approve_deletions(&self, pair: PairId, pass_token: &str) {
		self.approvals
			.lock()
			.await
			.insert(pair, pass_token.to_string());
		// After the approval is stored, so the pass this wakes finds it. A pair with no changelist
		// is gone, or has not passed yet: its next pass reads everything anyway.
		if let Some(changes) = self.existing_pair_changes(pair).await {
			changes.force(FullPassReason::DeletionHold);
			changes.ring();
		}
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

	/// Record what this pass's actions did: a failure extends that path's streak — one on a remote
	/// outage only once per [`TRANSIENT_FAILURE_SPACING`] — and a success ends the streaks `ends`
	/// pairs it with (see [`endable_streaks`]). Once a streak reaches
	/// [`MAX_PATH_FAILURES`] the path stops being planned and is reported as
	/// [`UnsyncableReason::RepeatedFailure`] instead of failing on every pass forever — until
	/// [`PATH_FAILURE_RETRY_INTERVAL`] after its last failure, when it is tried once more.
	///
	/// A directory deletion that failed leaves the streaks of the deletions it carried alone, since
	/// none of them was attempted on its own.
	async fn note_path_outcomes(
		&self,
		pair: PairId,
		ends: Vec<(String, String)>,
		report: &mut SyncReport,
	) {
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
				ends.into_iter()
					.filter(|(by, _)| !failed.contains_key(by.as_str()))
					.map(|(_, ended)| ended)
					.collect()
			});
		let recorded: Vec<(String, String)> = failed
			.iter()
			.map(|(path, error)| ((*path).to_string(), (*error).to_string()))
			.collect();
		// Owned for the closure that writes them, as `recorded` is.
		let transient = report.transient_failures.clone();
		let spacing = i64::try_from(TRANSIENT_FAILURE_SPACING.as_millis()).unwrap_or(i64::MAX);
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
		// One statement per failure and per streak ended, so this scales with the failing paths.
		// Off the runtime thread, with the paths owned by the closure that writes them.
		let outcome = off_store(&store, move |store| {
			let mut problems = Vec::new();
			for (path, error) in &recorded {
				if let Err(e) = store.record_failure(pair, path, error, now) {
					problems.push(format!("{path}: recording the failure count failed: {e}"));
				}
			}
			for (path, error) in &transient {
				if let Err(e) = store.record_spaced_failure(pair, path, error, now, spacing) {
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

	/// Ask `pair`'s [`watch`](Self::watch) for a pass now — a "sync now" button — rather than at
	/// its next change or safety-net tick. The pass runs at the watch's debounce, after the pass in
	/// flight if there is one; a watch backing off after a failed pass cuts the backoff short and
	/// runs it at once, since the caller is asking for exactly that retry.
	///
	/// The pass is forced WHOLE ([`FullPassReason::Requested`]): it reads both sides in full, like
	/// a safety-net tick, so it also finds what both changelists silently missed. On a large tree
	/// that is the cost of a full pass, not of the changes since the last one.
	///
	/// Without a watch nothing runs: the request is recorded, and the next
	/// [`sync_once`](Self::sync_once) is that whole pass. Errors if the pair is unknown, being
	/// removed, or paused — a paused watch runs nothing, so the request would wait silently for a
	/// resume, which already forces a whole pass of its own.
	pub async fn request_pass(&self, pair: PairId) -> Result<(), Error> {
		if locked(&self.control)
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.is_none()
		{
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		// Read without creating the channel, as `set_control` does. A pair on its way out is not
		// `Run` either, but telling the caller to resume it would send them to a verb that refuses.
		let control = self
			.paused
			.lock()
			.await
			.get(&pair)
			.map(|control| *control.borrow());
		match control {
			Some(PassControl::Retired) => return Err(being_removed(pair)),
			Some(control) if control.is_paused() => {
				return Err(Error::custom(
					ErrorKind::InvalidState,
					format!("sync pair {pair} is paused: resume it before asking for a pass"),
				));
			}
			_ => {}
		}
		// A pair with no changelist has not passed in this process yet, and its first pass reads
		// everything anyway.
		if let Some(changes) = self.existing_pair_changes(pair).await {
			changes.force(FullPassReason::Requested);
			changes.request();
		}
		Ok(())
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
			.map(|entry| PlannedConflict {
				rel_path: entry.rel_path,
				local: entry.local_kind.map(PlannedNodeKind::from),
				remote: entry.remote_kind.map(PlannedNodeKind::from),
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
	///
	/// A pass plans only against a remote view the cache has finished listing: while the cache is
	/// still listing the pair's remote folder (its first listing, or a relist after it lost track of
	/// the drive), every item the listing has not reached would read as absent and plan an upload
	/// over it. So the pass first waits for the listing, reporting
	/// [`SyncEvent::AwaitingRemote`] once, for up to 15 minutes; a wait that runs out, or a
	/// listing that started after the wait ended, fails the pass with [`ErrorKind::StaleState`]
	/// and applies nothing. A cancel or removal reaches a pass in this wait as it does one reading,
	/// and so does a suspension, which ends the wait instead of parking it: the wait has nothing in
	/// flight to carry on with, so the pass reports itself paused as a cancelled one does. A watch
	/// being stopped, or the engine being closed, ends the wait as one that ran out.
	///
	/// Before it plans, a pass records as synced every path that has never synced where both sides
	/// hold a file, the remote one carries no BLAKE3 (another client uploaded it without one) and
	/// the two agree: the filen-sync state the pair imported
	/// ([`import_legacy_state`](Self::import_legacy_state)) lists the path at the remote's version
	/// and the local file's size and mtime (to the millisecond, or as filen-sync computed it), both
	/// files are empty, or their sizes are equal and the remote's modification time is the local
	/// one: to the millisecond, as filen-sync computed it
	/// from the local one (`Math.floor` of Node's floating-point `mtimeMs`), or anywhere in the
	/// same second where the local one has no sub-second part, as on a file system that keeps whole
	/// seconds. A tree another client synced before this engine then reads as synced rather than
	/// as created on both sides. Those records stand even when the pass is cancelled before it
	/// plans.
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
		// A pass that panics — a baseline read that failed, which is a panic on purpose so that it is
		// never read as an absent row, or any broken invariant — fails as a PASS: the error goes to
		// the caller, and a watch loop backs off and runs the next one instead of dying with the
		// pass and leaving the pair silently unsynced. Everything the pass held is released by the
		// unwind; the next pass is forced whole below, like any failed one.
		let result =
			pass_panics_as_errors(pair, "the pass", self.run_pass(pair, observer, when_idle)).await;
		// What this pass's outcome means for the NEXT one's scope, wherever it ended. A pass that
		// failed outright took both changelists and applied nothing, so what it took is reflected
		// nowhere and the next pass cannot rely on them.
		let next_scope = match &result {
			Ok(report) => next_pass_scope(report),
			Err(_) => Some(FullPassReason::InterruptedPass),
		};
		match next_scope {
			// A hold makes the next pass whole without making it due (see
			// `PairChanges::note_deletion_hold`).
			Some(FullPassReason::DeletionHold) => {
				if let Some(changes) = self.existing_pair_changes(pair).await {
					changes.note_deletion_hold();
				}
			}
			Some(reason) => self.force_full_pass(pair, reason).await,
			None => {}
		}
		if let Err(error) = &result {
			observer(SyncEvent::PassFailed {
				error: error.to_string(),
				kind: error.kind(),
			});
		}
		result
	}

	/// Give back the page caches a finished pass left on `pair`'s writing connection and its parked
	/// reader (see [`BaselineStore::release_page_caches`]), wherever the pass ended — and those a dry
	/// run ([`plan_pair`](Self::plan_pair)), an adoption
	/// ([`reconfigure_pair`](Self::reconfigure_pair) with [`Backlog::AdoptDestination`]) and a
	/// confirmation sweep left, which read as much.
	///
	/// A release reaches every connection in the process, so one landing while another pair's pass
	/// reads makes that pass read its interior pages back from the OS page cache: a few MiB at most,
	/// against the reader's and the writer's whole caches (`baseline::READER_CACHE_KIB`,
	/// `baseline::WRITER_CACHE_KIB`) the pair would otherwise keep until its next pass ends — on a
	/// watched pair nothing changes in, the watch's next safety-net read, and on an unwatched one,
	/// whenever its caller next syncs it. The end of every pass that reads already releases while
	/// other pairs read.
	///
	/// Only a store that is already open: a pass that never opened one filled no cache, and opening
	/// one here would pay the schema parse that keeping it open saves. Off the runtime thread,
	/// because the store's lock may be held by another verb's whole-tree work.
	async fn release_pair_cache(&self, pair: PairId) {
		let Some(store) = self.stores.lock().await.get(&pair).map(Arc::clone) else {
			return;
		};
		if let Err(error) = off_store(&store, BaselineStore::release_page_caches).await {
			tracing::warn!(
				"sync_once[pair {pair}]: giving back the pass's page cache failed: {error}"
			);
		}
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
	/// (the optimization plan's section 3.4 step 7, and its carry-over set in 3.6). `streaks` is
	/// every failure streak the pair has, not only the blocking ones.
	///
	/// Not called by a pass that REFUSED: a refusal forces the next pass whole, which rebuilds all
	/// of this from a read of both sides.
	///
	/// `released` is what the plan released past the guard (see [`released_paths`]).
	async fn carry_forward(
		&self,
		pair: PairId,
		prep: &Prepared,
		report: &SyncReport,
		released: &BTreeSet<String>,
		streaks: &HashMap<String, PathFailure>,
	) {
		if let Some(changes) = self.existing_pair_changes(pair).await {
			changes.note_owed(facts::carry_over(
				released.iter().map(String::as_str),
				&report.conflicts,
				&prep.holds.held_remote,
				streaks,
			));
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

	/// Prune `pair`'s local bin under `local_root` as of `today` (see [`bin::prune_bin`]), unless it
	/// was already pruned on `today`. Returns what the prune did, or `None` when it did not run.
	///
	/// The prune stops between bin entries once `gate` says the pass is paused or cancelled; a
	/// prune that stopped does not count, so the pair's next pass prunes again. One that hit errors
	/// does count — each was logged, and retrying them every pass would only log them again.
	///
	/// Housekeeping, so nothing here fails the pass: a prune that panicked is logged and left for
	/// the next pass.
	async fn prune_bin_daily(
		&self,
		pair: PairId,
		local_root: &Path,
		today: NaiveDate,
		gate: &PassGate,
	) -> Option<bin::PruneSummary> {
		if self.bin_pruned.lock().await.get(&pair) == Some(&today) {
			return None;
		}
		// Owned copies for the blocking thread: the root is one path, and the gate's clone is two
		// channel handles.
		let root = local_root.to_path_buf();
		let gate = gate.clone();
		let summary = match off_runtime("pruning the sync bin", move || {
			bin::prune_bin(&root, today, || gate.running())
		})
		.await
		{
			Ok(summary) => summary,
			Err(e) => {
				tracing::warn!("sync_once[pair {pair}]: the local bin was not pruned: {e}");
				return None;
			}
		};
		let bin::PruneSummary {
			removed,
			adopted,
			errors,
			stopped,
		} = summary;
		if removed > 0 || adopted > 0 || errors > 0 {
			tracing::info!(
				"sync_once[pair {pair}]: pruned the local bin as of {today}: deleted {removed} \
				 bucket(s) older than {} days, filed {adopted} undated item(s) under today, \
				 {errors} error(s){}",
				bin::RETENTION_DAYS,
				if stopped { "; stopped by a pause" } else { "" },
			);
		}
		if !stopped {
			self.bin_pruned.lock().await.insert(pair, today);
		}
		Some(summary)
	}

	/// Move filen-sync's local trash into `pair`'s bin (see [`bin::migrate_legacy_trash`]) on the
	/// pair's first pass: while it has no baseline row (see [`has_synced`]). A pair moved over from
	/// filen-sync has its old deletions binned, under the day of this pass, before anything reads
	/// the tree; any other pair finds nothing, and creates nothing. The walk runs once per pair and
	/// engine (see [`legacy_trash_looked`](Self::legacy_trash_looked)): a first pass refused or
	/// cancelled after it does not walk the tree again. An engine opened later walks it again while
	/// the pair has no row, as one does for a pair whose rows have all gone again, which moves only
	/// what has been given filen-sync's name since. A folder the walk found but could not move is
	/// moved again on each later pass of this engine, until the move lands or the folder is gone.
	///
	/// Before the pass has read anything, so a pass refused afterwards has still migrated: the
	/// folders are never synced, so no refusal is about them, a moved one is recoverable from the
	/// bin, and the read can write seeded rows, after which [`has_synced`] would never let a later
	/// pass walk the tree for them. The walk
	/// stops between directories and moves once `gate` says the pass is cancelled or its pair
	/// removed, never at a suspension: the read after it runs on through one and can write rows,
	/// after which no pass would look again (see [`PassGate::not_cancelled`]). A cancelled pass
	/// writes no row, so the next pass, still without one, takes up the rest.
	///
	/// Housekeeping, so nothing here fails the pass: what is left behind is never synced anyway.
	async fn migrate_legacy_trash(&self, pair: PairId, gate: PassGate) {
		// An owned copy for the blocking thread: the few folders a move failed for, if any.
		let owed = match self.legacy_trash_looked.lock().await.get(&pair) {
			Some(owed) if owed.is_empty() => return,
			owed => owed.cloned(),
		};
		let record = locked(&self.control).pair(pair);
		let (local_root, pair_root) = match record {
			Ok(Some(record)) => (
				PathBuf::from(record.local_root),
				self.pair_root(record.remote_root),
			),
			// The read that follows reports an unknown pair, and a failed registry read, itself.
			Ok(None) => return,
			Err(e) => {
				tracing::warn!(
					"sync_once[pair {pair}]: filen-sync's trash was not looked for: cannot read the \
					 pair: {e}"
				);
				return;
			}
		};
		// A retry moves what the walk left whether or not the pair has synced since: those folders
		// are filen-sync's, found while it had not.
		let retrying = owed.is_some();
		if !retrying {
			let synced = match self.pair_store(pair).await {
				Ok(store) => off_store(&store, move |store| has_synced(store, pair))
					.await
					.and_then(|synced| synced),
				Err(e) => Err(e),
			};
			match synced {
				Ok(false) => {}
				Ok(true) => {
					self.legacy_trash_looked
						.lock()
						.await
						.insert(pair, Vec::new());
					return;
				}
				Err(e) => {
					tracing::warn!(
						"sync_once[pair {pair}]: filen-sync's trash was not looked for: {e}"
					);
					return;
				}
			}
		}
		// One day for the whole migration, so everything it moves lands in one bucket.
		let day = Utc::now().date_naive();
		let summary = match off_runtime("moving filen-sync's trash into the bin", move || {
			let keep_going = || gate.not_cancelled();
			match owed {
				Some(owed) => bin::retry_legacy_trash(&local_root, owed, day, keep_going),
				None => bin::migrate_legacy_trash(&local_root, pair_root, day, keep_going),
			}
		})
		.await
		{
			Ok(summary) => summary,
			Err(e) => {
				tracing::warn!("sync_once[pair {pair}]: filen-sync's trash was not moved: {e}");
				return;
			}
		};
		let bin::MigrateSummary {
			moved,
			errors,
			stopped,
			unmoved,
		} = summary;
		let left = unmoved.len();
		// A walk cut short is taken up whole by the next pass that still has no row; a retry, and a
		// walk that ran to its end, leave the next pass exactly the folders still in place.
		if retrying || !stopped {
			self.legacy_trash_looked.lock().await.insert(pair, unmoved);
		}
		if moved > 0 || errors > 0 || stopped {
			tracing::info!(
				"sync_once[pair {pair}]: moved {moved} filen-sync trash folder(s) into the sync \
				 bin's bucket for {day}, {errors} error(s), {left} left to move on a later \
				 pass{}",
				if stopped {
					"; stopped by a cancel or the pair's removal (after a cancel, the rest moves on \
					 a later pass)"
				} else {
					""
				},
			);
		}
	}

	/// Whether a pair whose remote root is `remote_root` is rooted at the drive root, whose account
	/// data directory it never syncs (see [`scan::is_account_data`]).
	pub(super) fn pair_root(&self, remote_root: Uuid) -> PairRoot {
		if remote_root == self.client.root().uuid() {
			PairRoot::Drive
		} else {
			PairRoot::Directory
		}
	}

	/// Where a new stage for a pass of `pair` goes (see [`stage`](super::stage)), numbered by this
	/// engine so that no two of its passes share one — nor a dry run that stages its own work,
	/// whatever pass of the same pair is running. A number never made into a stage is skipped.
	fn stage_at(&self, pair: PairId) -> PathBuf {
		let number = self.stages.fetch_add(1, Ordering::Relaxed);
		stage::stage_path(&self.db_path, pair, number)
	}

	/// A new stage for a pass of `pair`, at [`stage_at`](Self::stage_at).
	fn stage(&self, pair: PairId) -> Result<Stage, Error> {
		Stage::create(self.stage_at(pair))
	}

	/// The body of [`sync_pass`](Self::sync_pass), reporting to an observer whose panics are
	/// already contained.
	///
	/// A pass that reads gives back the pair's page caches once it is over (see
	/// [`release_pair_cache`](Self::release_pair_cache)); one that never starts does not. A paused
	/// pair, and a watch wake with nothing announced, read neither side, so their release would give
	/// back none of the pair's pages and only make every other connection in the process read its
	/// own back.
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
		let changes = self.pair_changes(pair).await;
		// Nothing announced on either side, nothing owed by the last plan, and nothing forcing a
		// whole read: there is no pass to run. A watch wake that lands here does nothing at all —
		// no `PassStarted`, and the safety net goes on measuring from the last whole read — because
		// reading two trees to discover there was nothing to do is the cost this scoping exists to
		// remove. An explicit `sync_once` still runs: its caller asked for a pass and wants the
		// report, and may well have beaten its own watcher's event.
		//
		// Asked BEFORE anything is taken, so a skipped wake leaves every list and reason to the
		// pass that runs — a deletion hold among them, which by itself is no reason to run one:
		// two trees nothing has changed in would only hold the identical batch again (see
		// `FullPassReason::DeletionHold`).
		if when_idle == WhenIdle::Skip && changes.is_idle() {
			tracing::debug!(
				"sync_once[pair {pair}]: nothing announced since the last pass — no pass run"
			);
			return Ok(SyncReport::default());
		}
		// From here the pass reads: whichever way it ends — a panic included, which is how a failed
		// baseline read ends — it gives back what that read cached.
		let ran = pass_panics_as_errors(
			pair,
			"the pass",
			self.run_started_pass(pair, observer, gate, reading, changes),
		)
		.await;
		self.release_pair_cache(pair).await;
		ran
	}

	/// The pass [`run_pass`](Self::run_pass) found to run, from its first read on, holding `gate`
	/// and `reading` as that does.
	async fn run_started_pass(
		&self,
		pair: PairId,
		observer: &mut (dyn FnMut(SyncEvent) + Send),
		gate: PassGate,
		reading: Arc<Mutex<()>>,
		changes: Arc<PairChanges>,
	) -> Result<SyncReport, Error> {
		// The remote view this pass may plan against, waited for BEFORE the changelists are taken:
		// a change-scoped read checks that the cache still holds the root at this generation once
		// it has read the delta, which only proves that nothing relisted the root across the take
		// if the generation was read before it. Under the gate, so a cancel reaches the wait, and
		// outside the read's cost, which the wait for a first listing would swamp.
		//
		// A suspension ends the wait as well, where the gate would let it run on: the wait has
		// nothing in flight for a resume to carry on with, and parked behind it the pass would hold
		// its caller for up to the whole wait. So does a watch being stopped or the engine being
		// closed (`cut_loose_pass`), which leave nobody to wait for.
		let given_up = async {
			tokio::select! {
				() = changes.waiting_stopped() => {}
				() = gate.halted() => {}
			}
		};
		let Some(converged) = gate
			.guard(self.await_remote_view(pair, Some(&mut *observer), given_up))
			.await
		else {
			return cancelled_reading(&gate, pair);
		};
		let converged = match converged {
			Ok(converged) => converged,
			// Ended by a pause, or failed while one landed: either way the pair is paused, and the
			// pass reports so as one cancelled while reading does.
			Err(_) if !gate.running() => return cancelled_reading(&gate, pair),
			Err(error) => return Err(error),
		};
		// Both changelists, taken BEFORE either side is read: everything in them is either already
		// in the snapshot this pass is about to read (a harmless duplicate) or not yet, and this
		// pass has it. What arrives from here on belongs to the next pass (see `changes`).
		let mut scope = changes.take();
		let (dirty_local, dirty_remote) = scope.sizes();
		// What reading both sides costs this pair — measured over the read and nothing else, since
		// the watch's safety net scales its interval by it (see `SyncReport::read_cost`). The
		// migration's walk is housekeeping that runs once, and is taken back out.
		let read_started = Instant::now();
		// A clone for the migration's blocking thread: two channel handles.
		let migrating = gate.clone();
		let Some((recording, migration, prepared)) = gate
			.guard(async move {
				let recording = reading.lock_owned().await;
				// Before the local side is read, so the read sees the tree as the migration left it,
				// and inside the reading lock, so no import lands between its look at the baseline
				// and the read's.
				let migration_started = Instant::now();
				self.migrate_legacy_trash(pair, migrating).await;
				let migration = migration_started.elapsed();
				let read = ScopedRead {
					scope: &mut scope,
					converged,
				};
				let prepared = self.prepare(pair, Some(read), Seeding::Write).await;
				(recording, migration, prepared)
			})
			.await
		else {
			return cancelled_reading(&gate, pair);
		};
		// The read ends here: everything above it is the baseline, the scan, the snapshot and the
		// view; everything below writes something.
		let read_cost = read_started.elapsed().saturating_sub(migration);
		let mut prep = prepared?;
		// A pair the cache refused to subscribe asks again, so a refusal while offline lasts until
		// the first pass after the cache can be reached; this pass read whole either way. After a
		// read that worked, so the read is not held up by it and a pass that could not read asks
		// nothing, and still under the gate, so a removal cannot drop the pair between this and
		// the subscription it records.
		if let Err(error) = self.resubscribe(pair).await {
			tracing::warn!(
				"sync pair {pair}: the cache refused its subscription again ({error}); the next pass reads the remote whole too"
			);
		}
		let store = self.pair_store(pair).await?;
		// Persist what this pass confirmed, before anything is reconciled. Only a real pass writes
		// it: `plan_pair` stays a pure read, so a dry run inside the confirmation window just leaves
		// it for the next pass — and leaves the evidence with it, which is why the records are
		// retired HERE and not in the reading step.
		self.persist_confirmed(pair, &store, mem::take(&mut prep.confirmed))
			.await?;
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

		// After the refusal: a pass that will not act on what it read (a name collision on either
		// side) leaves the bin alone as well. A root that is missing or unmounted has no bin under
		// it, and the prune creates none (see `bin::prune_bin`).
		self.prune_bin_daily(
			pair,
			Path::new(&prep.record.local_root),
			Utc::now().date_naive(),
			&gate,
		)
		.await;
		// After the refusal for the same reason, and before the apply: its downloads write temp
		// files of their own, though none old enough to be taken for a leftover.
		remove_stale_downloads(pair, mem::take(&mut prep.local_scan.stale_downloads)).await;

		let state = screen_state(&prep);
		let at = self.stage_at(pair);
		// Every read and write of the plan's stage from here to the apply scales with the plan, so
		// each runs on a blocking thread, handed what it reads and handing it back.
		let screened;
		(prep, screened) = off_runtime("screening the pass's plan", move || {
			let screened = reconcile_and_screen(&prep, state, || Stage::create(at));
			(prep, screened)
		})
		.await?;
		let mut screened = screened?;
		// A one-shot approval releases the held batch it names — and only that batch.
		if let Some(token) = screened.pass_token.clone()
			&& self.take_approval(pair, &token).await
		{
			let released;
			(screened, released) = off_runtime("releasing the approved batch", move || {
				let released = screened.approve();
				(screened, released)
			})
			.await?;
			// Unwrapped before the log: tracing evaluates an event's arguments only when it is
			// enabled.
			let released = released?;
			tracing::debug!(
				"sync_once[pair {pair}]: approval {token} matched — applying {released} held \
				 deletion(s)"
			);
		}
		report.conflicts = mem::take(&mut screened.conflicts);
		// `held` can also carry the create half of a held type flip; `held_deletions()` counts only
		// the deletions.
		let held;
		(prep, screened, held) = off_runtime("listing the held actions", move || {
			let held = prep.planned(&screened, Rows::Held);
			(prep, screened, held)
		})
		.await?;
		report.held = held?;
		report.guard = screened.reason.clone();
		report.deletion_token = screened.pass_token.take();
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
				if apply::record_not_written(&error) {
					report.store_failed = Some(apply::StoreFailure::RecordNotWritten(error.kind()));
				}
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
		// The UNFILTERED streaks, not `Prepared::failures`: a path whose attempts are below the
		// blocking threshold is filtered out of the pass, and filtering it out of the carry-over too
		// would mean no later pass ever looks at it again and its retry never comes. A success ends
		// such a streak as well, so the apply below reads the same set.
		let streaks = match self.pair_store(pair).await {
			Ok(store) => off_store(&store, move |store| store.failures(pair))
				.await
				.ok()
				.and_then(Result::ok),
			Err(_) => None,
		};
		// Before the early return below: a pass with nothing to apply still made a plan, and still
		// owes the next one everything that plan named. A plan that cannot be read back is the pass's
		// error, as any read of its stage is.
		let released;
		(screened, released) = off_runtime("reading the pass's screened plan", move || {
			let released = released_paths(&screened);
			(screened, released)
		})
		.await?;
		let released = released.map_err(|e| db_error(e, "reading the pass's screened plan"))?;
		self.carry_forward(
			pair,
			&prep,
			&report,
			&released,
			streaks.as_ref().unwrap_or(&HashMap::new()),
		)
		.await;
		drop(released);
		// After the guard counted every item the plan deletes and the carry-over owed each of their
		// paths: what is left is what the apply runs.
		let applies;
		(screened, applies) = off_runtime("folding the pass's deletions", move || {
			let applies = screened
				.fold_subsumed()
				.and_then(|()| screened.count(Rows::Safe));
			(screened, applies)
		})
		.await?;
		let applies = applies?;
		observer(SyncEvent::Planned { actions: applies });

		// The return below hands back the LOCAL half alone: `note_owed_remote` sits after the
		// apply, since `report.interrupted` is what gates it and nothing has applied anything yet.
		// That is the shape this stage's first attempt was reverted for, and it is safe here only
		// because a pass with nothing safe to apply either planned nothing at all or had every
		// action held or dropped — a hold sets the guard's reason, which `next_pass_scope` reads as
		// `DeletionHold`, and a drop counts in `dropped_actions`, which it reads as
		// `UnappliedWork`. Both send the next pass to a whole read, which needs no hand-back. A
		// change that let an action reach `held` without a reason would break that silently.
		let Some(screen) = screened.plan.take().filter(|_| applies > 0) else {
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
		};

		// Resolve the sync root (every top-level parent). The drive root is the client's own; any
		// other directory is read from the server, which errors if it is gone.
		let root_remote = match self.pair_root(prep.record.remote_root) {
			PairRoot::Drive => DirType::Root(Cow::Borrowed(self.client.root())),
			PairRoot::Directory => DirType::Dir(Cow::Owned(
				self.client.get_dir(prep.record.remote_root).await?,
			)),
		};

		let (screen, ends) = off_runtime("reading the pass's screened plan", move || {
			let ends = endable_streaks(&screen, streaks.as_ref());
			(screen, ends)
		})
		.await?;
		let ends = ends.map_err(|e| db_error(e, "reading the pass's screened plan"))?;
		// The plan is made: each action takes what the apply reads for it into the pass's stage, and
		// the rest of the scan, the view and the rows goes — the rows' read transaction with them,
		// before the first action commits, since holding it open would pin the WAL through every
		// upload (see `Baseline::close`).
		let plan = prep.stage_plan(screen).await?;
		let local_root = PathBuf::from(&prep.record.local_root);
		let ctx = ApplyContext {
			client: &self.client,
			// Before the first action, and never again this pass (see `ApplyContext::cache_db`).
			cache_db: self.client.cache_slot.lock().await.db_path(),
			local_root: &local_root,
			pair,
			mode: prep.record.mode,
			store: &store,
			root_remote,
			absence_trusted: state.absence_trusted(),
			pending: &self.pending,
			observed: &self.observed,
			gate: &gate,
			lock_budget: self.lock_budget,
			bin_day: Utc::now().date_naive(),
		};
		apply::apply(ctx, plan, &mut report, observer).await;
		// The pass let go of its hold of the drive lock; what it looked up during it goes too.
		self.client.forget_ended_hold_targets();
		self.note_path_outcomes(pair, ends, &mut report).await;
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

/// `pass` — a pass, or a call that reads the pair as one does — with a panic inside it answered as
/// its error, which `what` names (see [`SyncEngine::sync_pass`]).
async fn pass_panics_as_errors<T>(
	pair: PairId,
	what: &str,
	pass: impl std::future::Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
	std::panic::AssertUnwindSafe(pass)
		.catch_unwind()
		.await
		.unwrap_or_else(|panic| {
			let message = panic
				.downcast_ref::<&str>()
				.map(|message| (*message).to_string())
				.or_else(|| panic.downcast_ref::<String>().cloned())
				.unwrap_or_else(|| "a panic with no message".to_string());
			tracing::error!("sync pair {pair}: {what} panicked: {message}");
			Err(Error::custom(
				ErrorKind::Internal,
				format!("{what} panicked: {message}"),
			))
		})
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
	// (or, past the cap, made the next pass whole itself — `PairChanges::note_owed`) and the apply
	// handed its announced changes back (`note_owed_remote`), so the next pass
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
	// action screened out before it ran (`Blocking`, `Withholding`).
	// Each leaves paths owed with nothing left holding them: a dropped action's evidence was an
	// observation or an announcement this pass consumed, and a derived map rebuilds neither — the
	// baseline row it would derive from is the one that says the change never happened.
	if report.lock_failed.is_some()
		|| report.store_failed.is_some()
		|| report.halted.is_some()
		|| !report.failed_paths.is_empty()
		|| report.dropped_actions > 0
	{
		return Some(FullPassReason::UnappliedWork);
	}
	// EVERY deletion hold, including the volume threshold and the first-sync hold: an approval
	// names one exact batch, and the pass that offers it again has to reproduce it from the same
	// absences — which were observations the holding pass consumed (see
	// [`FullPassReason::DeletionHold`]). The caller records this row apart from the others: it
	// makes the next pass whole without making it due, so a wake that announced nothing new does
	// not re-read two unchanged trees only to hold the identical batch again.
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
		} else if state.remote == guard::RemoteEvidence::Unconverged {
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
	/// Every executable action in apply order, screened in the pass's stage: what the guard holds
	/// ([`Rows::Held`]) and what it lets through ([`Rows::Safe`]) — what an APPROVED pass runs is
	/// both. `None` for a change-scoped pass that planned nothing, which makes no stage.
	plan: Option<Screen>,
	/// Why the guard holds part of the plan, exactly when it holds any.
	reason: Option<GuardReason>,
	/// Identifies the held batch, when the guard held one.
	pass_token: Option<String>,
	/// Paths this pass deliberately left alone (see [`SyncReport::deferred_paths`]).
	deferred_paths: usize,
	/// Actions this pass planned and then dropped (see [`SyncReport::dropped_actions`]).
	dropped: usize,
}

impl Screened {
	/// How many actions of `rows` the plan holds.
	fn count(&self, rows: Rows) -> Result<usize, Error> {
		self.plan
			.as_ref()
			.map_or(Ok(0), |plan| plan.count(rows))
			.map_err(|e| db_error(e, "reading the pass's screened plan"))
	}

	/// Release the held batch, which an approval names: the pass runs it all. Says how many actions
	/// that releases.
	fn approve(&mut self) -> Result<usize, Error> {
		let released = match &mut self.plan {
			Some(plan) => plan
				.release_held()
				.map_err(|e| db_error(e, "releasing the approved batch"))?,
			None => 0,
		};
		self.reason = None;
		self.pass_token = None;
		Ok(released)
	}

	/// Fold every deletion a directory deletion of the same side carries out anyway into that
	/// directory's (see [`Screen::fold_subsumed`]): what is left is what the apply runs. After the
	/// guard counted every item the plan deletes, and the carry-over owed each of their paths.
	fn fold_subsumed(&mut self) -> Result<(), Error> {
		if let Some(plan) = &mut self.plan {
			plan.fold_subsumed()
				.map_err(|e| db_error(e, "folding the pass's deletions"))?;
		}
		Ok(())
	}
}

/// Reconcile the prepared inputs, and screen the plan in the pass's stage: the actions this pass
/// may not run screened out of it, the conflicts split out, and the deletions through the guard.
///
/// A pass that read both sides whole into its stage reconciles them window by window into a stage
/// of its own, which `new_stage` makes (see [`stage::reconcile_windows`]): the plan is decided
/// without a set of the tree's paths in memory. Any other pass reconciles in memory, over what it
/// read, and writes the plan it made into a stage `new_stage` makes, unless it planned nothing.
/// Either way the plan is screened where it is staged, a page at a time (see [`screen_plan`]), and
/// that stage is the one the plan the apply runs is staged in.
///
/// A stage that cannot be made, written or read (a full disk) is an error, before anything is
/// applied.
fn reconcile_and_screen(
	prep: &Prepared,
	state: guard::ScreenState,
	new_stage: impl FnOnce() -> Result<Stage, Error>,
) -> Result<Screened, Error> {
	let (screen, deferred) = match (
		prep.read.paths(),
		&prep.local_scan.nodes,
		&prep.remote_view.nodes,
	) {
		(plan::PassPaths::Whole, Side::Staged(local), Side::Staged(remote)) => {
			let stage = new_stage()?;
			let at = stage.path().display().to_string();
			// The directory moves run before everything else in the plan, which already names their
			// subtrees by the paths they move to.
			stage::reconcile_windows(
				prep.record.mode,
				&prep.baseline,
				local,
				remote,
				&prep.holds,
				stage,
				stage::WINDOW,
			)
			.and_then(|windows| windows.screen(&prep.holds, &prep.dir_moves))
			.map_err(|e| db_error(e, &format!("reconciling the pass in {at}")))?
		}
		_ => {
			let plan = plan::reconcile(
				prep.record.mode,
				&prep.baseline,
				&prep.local_scan.nodes.of(&prep.baseline),
				&prep.remote_view.nodes.of(&prep.baseline),
				&prep.holds,
				prep.read.paths(),
			);
			if plan.actions.is_empty() && prep.dir_moves.is_empty() {
				return Ok(Screened {
					conflicts: Vec::new(),
					plan: None,
					reason: None,
					pass_token: None,
					deferred_paths: plan.deferred_paths,
					dropped: 0,
				});
			}
			let stage = new_stage()?;
			let at = stage.path().display().to_string();
			let screen = Screen::new(stage, &prep.dir_moves, &plan.actions)
				.map_err(|e| db_error(e, &format!("staging the pass's plan in {at}")))?;
			(screen, plan.deferred_paths)
		}
	};
	let context = format!("screening the pass's plan in {}", screen.path().display());
	screen_plan(prep, state, screen, deferred).map_err(|e| match e {
		ScreenError::Sql(e) => db_error(e, &context),
		invariant => Error::custom_with_source(ErrorKind::Internal, invariant, Some(context)),
	})
}

/// Screen `screen`, the reconciled plan with the directory moves leading it, where it is staged —
/// `deferred` being the paths the reconcile deferred — in the order a pass always screened it:
///
/// - drop what this pass may not act on ([`Blocking`]), and the deletions, and the creates of a
///   type flip, it withholds ([`Withholding`]); each is decided by the paths the pass blocks and
///   withholds and a first sweep of the moves or the deletions, which are all a decision reads
///   beyond the action itself;
/// - run each directory move right after the creates above its destination
///   ([`Screen::lead_creates`]);
/// - split the conflicts out;
/// - COUNT the deletions, which is all the guard decides from ([`guard::reason_for`]); when it holds
///   them, mark what its reason holds ([`Screen::hold`]) and name the held batch by a token hashed
///   from its lines as they stream out of the stage in sorted order ([`BatchToken`]).
///
/// What this holds is a page of the plan and those sets of paths; never the plan, a copy of it, or a
/// line per held deletion.
fn screen_plan(
	prep: &Prepared,
	state: guard::ScreenState,
	mut screen: Screen,
	deferred: usize,
) -> Result<Screened, ScreenError> {
	// Both filters only ever REMOVE actions, and what they remove is what this pass planned and will
	// not carry out. The next pass has to read everything to find those paths again: the evidence
	// that planned them — a local observation, an announced remote change — was consumed here.
	let mut dropped = 0;
	let (blocked, ignored) = (prep.blocked_paths(), prep.ignored_roots());
	if let Some(mut blocking) = Blocking::new(&blocked, &ignored) {
		screen.each(Rows::Moves, |action| blocking.note(&action))?;
		dropped += screen.retain(Rows::All, |action| blocking.keeps(action))?;
	}
	let blocked_rules = prep.blocked_rules();
	let mut withholding = Withholding::new(
		&prep.facts.unknown_remote,
		&prep.holds.held_remote,
		[&prep.facts.ignored_local, &prep.facts.ignored_remote],
		&blocked_rules,
	);
	if withholding.may_withhold() {
		screen.each(Rows::Deletes, |action| withholding.note(&action))?;
		if withholding.withholds() {
			dropped += screen.retain(Rows::All, |action| withholding.keeps(action))?;
		}
	}
	let deferred_paths = deferred + withholding.over_ignored;
	screen.lead_creates()?;
	let (local, remote) = (
		prep.local_scan.nodes.of(&prep.baseline),
		prep.remote_view.nodes.of(&prep.baseline),
	);
	let mut conflicts = Vec::new();
	screen.take_conflicts(|path| conflicts.push(planned_conflict(path, &local, &remote)))?;
	// A path that stops being ignored has no rows and syncs like a first sync, whose deletions are held
	// for approval: the rows the pair tracks elsewhere say nothing about what sits there.
	let mut first_sync = state.first_sync;
	let unignored = prep.unignored();
	if !first_sync && !unignored.is_empty() {
		screen.each_while(Rows::Deletes, |action| {
			let path = action.rel_path();
			first_sync = plan::ancestors(path)
				.chain([path])
				.any(|at| unignored.contains(at));
			!first_sync
		})?;
	}
	let reason = guard::reason_for(
		screen.count(Rows::Deletes)?,
		guard::ScreenState {
			first_sync,
			..state
		},
		prep.record.delete_guard,
	);
	// The guard names a reason exactly when it holds something, so a held batch always has one.
	let pass_token = match &reason {
		None => None,
		Some(reason) => {
			let held = screen.hold()?;
			tracing::debug!("guard: holding {held} action(s) back this pass — {reason:?}");
			let mut token = BatchToken::new(reason);
			screen.each_held_by_line(|action| token.line(action))?;
			Some(token.finish())
		}
	};
	Ok(Screened {
		conflicts,
		plan: Some(screen),
		reason,
		pass_token,
		deferred_paths,
		dropped,
	})
}

/// The paths the screened plan released past the guard — both ends of each action it lets through —
/// read only until they number more than [`DIRTY_CAP`], past which
/// [`PairChanges::note_owed`] keeps no set at all: what the next pass owes a look at costs that
/// many paths, whatever the plan holds (see [`facts::carry_over`]).
fn released_paths(screened: &Screened) -> rusqlite::Result<BTreeSet<String>> {
	let mut released = BTreeSet::new();
	if let Some(plan) = &screened.plan {
		plan.each_while(Rows::Safe, |action| {
			let (from, to) = action.endpoints();
			for end in [from, to] {
				if !released.contains(end) {
					released.insert(end.to_owned());
				}
			}
			!past_cap(released.len(), DIRTY_CAP)
		})?;
	}
	Ok(released)
}

/// Drop every action at a blocked path, and everything under it, decided an action at a time.
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
/// (see below): an unrelocatable item must not be deleted from the side that still holds it. So
/// every move is [`note`](Self::note)d before any action is asked about.
///
/// A move whose SOURCE is at or under an `ignored` root (a subset of `blocked`) goes too: the tracked
/// copy there is no longer synced, so it must not be moved away. Nothing is stranded by that, since
/// the deletions at the source are blocked already, and whether a directory above it may still be
/// deleted is for [`Withholding`] to decide.
struct Blocking<'s> {
	blocked: &'s BTreeSet<String>,
	ignored: &'s BTreeSet<String>,
	/// The sources of the moves this drops: their content is staying exactly where it is.
	stranded: Vec<String>,
}

impl<'s> Blocking<'s> {
	/// What drops actions for `blocked` and `ignored`; `None` when both are empty, and nothing is
	/// dropped.
	fn new(blocked: &'s BTreeSet<String>, ignored: &'s BTreeSet<String>) -> Option<Self> {
		(!blocked.is_empty() || !ignored.is_empty()).then(|| Self {
			blocked,
			ignored,
			stranded: Vec::new(),
		})
	}

	/// Whether `path` is at or under one of `roots`. `""` is the pair root, whose rules can be
	/// unreadable too: everything is under it. Every other root is found by walking the path's own
	/// ancestors, rather than reading every root per action.
	fn at_or_under(roots: &BTreeSet<String>, path: &str) -> bool {
		roots.contains("") || plan::at_or_under_root(roots, path)
	}

	/// Whether `action` is a move whose source is ignored.
	fn moves_ignored(&self, action: &SyncAction) -> bool {
		matches!(
			action,
			SyncAction::MoveRemote { from_path, .. } | SyncAction::MoveLocal { from_path, .. }
				if Self::at_or_under(self.ignored, from_path)
		)
	}

	/// Note the source `action` strands, when it is a move this drops for the path it moves to.
	fn note(&mut self, action: &SyncAction) {
		if let SyncAction::MoveRemote { from_path, .. } | SyncAction::MoveLocal { from_path, .. } =
			action && !self.moves_ignored(action)
			&& Self::at_or_under(self.blocked, action.rel_path())
		{
			self.stranded.push(from_path.clone());
		}
	}

	/// Whether `action` is kept.
	fn keeps(&self, action: &SyncAction) -> bool {
		if self.moves_ignored(action) {
			tracing::debug!(
				"reconcile: skipping {} — it moves an ignored item",
				action.describe()
			);
			return false;
		}
		let path = action.rel_path();
		if Self::at_or_under(self.blocked, path) {
			tracing::debug!(
				"reconcile: skipping {} — its path is blocked (a name the remote rejects, \
				 {MAX_PATH_FAILURES} consecutive failures, or an ignore rule)",
				action.describe()
			);
			return false;
		}
		// A move the pass could not make leaves its source in place — and the source reads as "gone"
		// on the other side, which is what planned the deletion in the first place. Deleting it now
		// (or deleting the directory above it, which is recursive) would destroy the only copy of
		// content this pass just refused to relocate. Renaming a synced item into a name the remote
		// rejects is exactly that shape.
		let strands = action.is_delete()
			&& self
				.stranded
				.iter()
				.any(|from| from == path || plan::is_under(from, path));
		if strands {
			tracing::debug!(
				"reconcile: skipping {} — it still holds content a blocked move could not relocate",
				action.describe()
			);
		}
		!strands
	}
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
///   each Mac for its `.DS_Store`. Counted as deferred (`over_ignored`).
/// - Over a subtree whose ignore rules could not be read (`ignore_blocked`), on either side, the
///   same: what those rules hide cannot be told apart from what they do not.
///
/// A deletion around a path blocked for any other reason (a rejected name, an alias, a park) runs as
/// it did, into the recoverable quarantine or trash.
///
/// Which paths wait is decided by the deletions alone, so every deletion is
/// [`note`](Self::note)d before any action is asked about.
struct Withholding<'s> {
	unknown: &'s BTreeMap<String, UnsyncableReason>,
	held: &'s BTreeSet<String>,
	ignored: [&'s BTreeMap<String, IgnoreDecision>; 2],
	ignore_blocked: &'s BTreeSet<String>,
	/// The paths whose deletions and creates wait: the directories withheld, and the `.filenignore`
	/// files whose rules keep one — deleted, they would leave the next pass no rule there, and the
	/// ignored content would go down with the directory after all.
	withheld: BTreeSet<String>,
	/// How many deletions wait over ignored content alone, which the pass counts as deferred.
	over_ignored: usize,
}

impl<'s> Withholding<'s> {
	fn new(
		unknown: &'s BTreeMap<String, UnsyncableReason>,
		held: &'s BTreeSet<String>,
		ignored: [&'s BTreeMap<String, IgnoreDecision>; 2],
		ignore_blocked: &'s BTreeSet<String>,
	) -> Self {
		Self {
			unknown,
			held,
			ignored,
			ignore_blocked,
			withheld: BTreeSet::new(),
			over_ignored: 0,
		}
	}

	/// Whether any deletion can wait: none does with nothing held, unknown, ignored or under rules
	/// that could not be read.
	fn may_withhold(&self) -> bool {
		!self.held.is_empty()
			|| !self.unknown.is_empty()
			|| self.ignored.iter().any(|ignored| !ignored.is_empty())
			|| !self.ignore_blocked.is_empty()
	}

	/// Whether a deletion noted so far waits.
	fn withholds(&self) -> bool {
		!self.withheld.is_empty()
	}

	/// Whether the directory at `dir` keeps content `ignored` hides. By level alone: which line
	/// matched says nothing about whether the content must be kept. Each question is "is anything
	/// under this directory", which a sorted map answers by seeking to it.
	fn keeps_ignored(&self, ignored: &BTreeMap<String, IgnoreDecision>, dir: &str) -> bool {
		self.ignore_blocked
			.range(plan::subtree_bounds(dir))
			.next()
			.is_some()
			|| plan::under_dir(ignored, dir)
				.any(|(_, decision)| decision.level != IgnoreLevel::Default)
	}

	/// Note `action`, when it is a deletion that waits.
	fn note(&mut self, action: &SyncAction) {
		let dir = action.rel_path();
		let above_held = self.held.range(plan::subtree_bounds(dir)).next().is_some();
		let [ignored_local, ignored_remote] = self.ignored;
		let (unreachable, ignored, side) = match action {
			SyncAction::DeleteLocal { .. } => (
				above_held || plan::under_dir(self.unknown, dir).next().is_some(),
				self.keeps_ignored(ignored_local, dir),
				ignored_local,
			),
			SyncAction::TrashRemote { .. } => (
				above_held,
				self.keeps_ignored(ignored_remote, dir),
				ignored_remote,
			),
			_ => return,
		};
		if ignored {
			self.withheld
				.extend(side.values().filter_map(|decision| match &decision.level {
					IgnoreLevel::File { dir: rules }
						if rules == dir || plan::is_under(rules, dir) =>
					{
						Some(Origin::File { dir: rules }.to_string())
					}
					IgnoreLevel::Default | IgnoreLevel::User | IgnoreLevel::File { .. } => None,
				}));
			if !unreachable {
				self.over_ignored += 1;
			}
		}
		if unreachable || ignored {
			self.withheld.insert(dir.to_owned());
		}
	}

	/// Whether `action` is kept.
	fn keeps(&self, action: &SyncAction) -> bool {
		let waits =
			(action.is_delete() || action.is_create()) && self.withheld.contains(action.rel_path());
		if waits {
			tracing::debug!(
				"reconcile: skipping {} — the directory holds an item this pass cannot act on",
				action.describe()
			);
		}
		!waits
	}
}

/// A stable identifier for one held deletion batch, hashed as it is read: the kind of reason the
/// guard held it for, then the `(side, path)` line of each held action, in sorted order. The same
/// deletions held for the same kind of reason always yield the same token; adding, dropping or
/// re-siding a single deletion yields a different one, and so does holding the very same deletions
/// for another reason — which is what makes an approval specific to the batch AND to what the caller
/// was told about it. Approving deletions over the volume limit must not release the same paths once
/// they are held because the remote listed nothing at all.
///
/// The reason enters by its kind alone: the counts it carries are not something a caller approves.
/// The deletion count is the batch's own size, which the lines already pin, and the limit moves with
/// the tracked-item count, which an unrelated upload landing between the hold and the approval
/// changes.
///
/// The lines are fed in as a staged plan reads them out ([`Screen::each_held_by_line`]), so a
/// batch of any size is hashed without a line of it held.
struct BatchToken(blake3::Hasher);

impl BatchToken {
	fn new(reason: &GuardReason) -> Self {
		let mut hasher = blake3::Hasher::new();
		hasher.update(token_kind(reason).as_bytes());
		hasher.update(b"\n");
		Self(hasher)
	}

	/// Hash the line of `action`, the next held action in the lines' sorted order.
	fn line(&mut self, action: &SyncAction) {
		let (side, path) = token_line(action);
		self.0.update(side.as_bytes());
		self.0.update(b"\t");
		self.0.update(path.as_bytes());
		self.0.update(b"\n");
	}

	fn finish(self) -> String {
		self.0.finalize().to_hex().chars().take(16).collect()
	}
}

/// The kind of `reason`, as a held batch's token names it.
fn token_kind(reason: &GuardReason) -> &'static str {
	match reason {
		GuardReason::ScanIncomplete => "scan-incomplete",
		GuardReason::FirstSyncWithDeletions { .. } => "first-sync",
		GuardReason::RemoteUnconverged { .. } => "remote-unconverged",
		GuardReason::RemoteEmptied { .. } => "remote-emptied",
		GuardReason::ExceededThreshold { .. } => "over-limit",
	}
}

/// The line a held action is named by in its batch's token: its side and its path. The sides sort
/// `local`, `other`, `remote`, and no side is a prefix of another, so the lines sort by side and
/// then by path.
fn token_line(action: &SyncAction) -> (&'static str, &str) {
	match action {
		SyncAction::DeleteLocal { rel_path, .. } => ("local", rel_path),
		SyncAction::TrashRemote { rel_path, .. } => ("remote", rel_path),
		other => ("other", other.rel_path()),
	}
}

/// How trustworthy this pass's inputs are, for the guard.
fn screen_state(prep: &Prepared) -> guard::ScreenState {
	guard::ScreenState {
		scan_complete: prep.local_scan.complete,
		// A wholly empty remote snapshot while the baseline still tracks remote items: what a
		// transient backend/cache fault looks like, and it would otherwise delete the whole pair.
		// A view that never converged is untrustworthy before it is anything else.
		remote: match (prep.remote_converged, prep.remote_emptied) {
			(false, _) => guard::RemoteEvidence::Unconverged,
			(true, true) => guard::RemoteEvidence::Emptied,
			(true, false) => guard::RemoteEvidence::Listed,
		},
		// The rows as the pass found them, not as its seeding left them (see
		// `Prepared::tracked_at_start`).
		first_sync: prep.tracked_at_start == 0,
		tracked: prep.tracked_at_start,
	}
}

fn overlap_error(overlap: PairOverlap) -> Error {
	Error::custom_with_source(
		ErrorKind::InvalidState,
		overlap,
		Some("registering a sync pair whose roots overlap an existing one".to_string()),
	)
}

/// A failed SQLite call on a sync database, as the engine's error, with the `rusqlite::Error` kept
/// as its source. Every rusqlite error the engine reports goes through here.
///
/// What the disk did is [`ErrorKind::IO`], as the same failure of a local file is: a full disk, an
/// I/O error of any extended kind, a file SQLite cannot open, and a database it cannot write to
/// (`SQLITE_READONLY`, plain or extended): SQLite opens a database file it may not write, such as a
/// 0444 file or one on a read-only mount, read-only and answers every write to it with the plain
/// code. Everything else is [`ErrorKind::Internal`].
pub(super) fn db_error(error: rusqlite::Error, context: &str) -> Error {
	let kind = match error.sqlite_error_code() {
		Some(
			rusqlite::ErrorCode::DiskFull
			| rusqlite::ErrorCode::SystemIoFailure
			| rusqlite::ErrorCode::CannotOpen
			| rusqlite::ErrorCode::ReadOnly,
		) => ErrorKind::IO,
		_ => ErrorKind::Internal,
	};
	Error::custom_with_source(kind, error, Some(context.to_string()))
}

/// What a pass cancelled while still reading returns: no plan was made, so nothing was applied —
/// the journal records the read had already retired stay retired, in memory and in the DB alike,
/// which is what keeps the two halves in step. The one kind of baseline row a read writes is a
/// seeded one (see `Prepared::seed_unhashed`), which runs on a blocking thread and commits even
/// when the cancel drops the pass: each records an agreement the read observed, and the next pass
/// reads it as any synced row. A removal took the pair with it and is said so; a plain cancel
/// leaves the pair paused, which is the report a pause made a moment earlier would have produced.
fn cancelled_reading(gate: &PassGate, pair: PairId) -> Result<SyncReport, Error> {
	if gate.retired() {
		return Err(being_removed(pair));
	}
	tracing::debug!("sync_once[pair {pair}]: cancelled while reading the two sides");
	Ok(SyncReport {
		paused: true,
		..SyncReport::default()
	})
}

/// Ask `converged` until it answers a generation, every `poll`, for at most `wait`: `None` when it
/// never did. `before_waiting` runs once, after a first answer of none and before the first wait,
/// so a caller hears of a wait only when there is one; its error ends the wait. `give_up`
/// resolving ends it too, wherever it stands, with the `None` of a wait that ran out — unless the
/// answer it was racing came first.
async fn await_convergence<A, F>(
	mut converged: A,
	before_waiting: impl Future<Output = Result<(), Error>>,
	poll: Duration,
	wait: Duration,
	give_up: impl Future<Output = ()>,
) -> Result<Option<u64>, Error>
where
	A: FnMut() -> F,
	F: Future<Output = Result<Option<u64>, Error>>,
{
	let waiting = async {
		if let Some(generation) = converged().await? {
			return Ok(Some(generation));
		}
		before_waiting.await?;
		let deadline = tokio::time::Instant::now() + wait;
		loop {
			tokio::time::sleep(poll).await;
			if let Some(generation) = converged().await? {
				return Ok(Some(generation));
			}
			if tokio::time::Instant::now() >= deadline {
				return Ok(None);
			}
		}
	};
	tokio::select! {
		biased;
		answer = waiting => answer,
		() = give_up => Ok(None),
	}
}

/// The error of a pass that found the cache had not finished listing `pair`'s remote root
/// `remote_root`: it plans nothing against a partial view. Stale state, as the view is: the next pass
/// reads it again.
fn remote_not_converged(pair: PairId, remote_root: Uuid) -> Error {
	Error::custom(
		ErrorKind::StaleState,
		format!(
			"sync pair {pair}: the cache has not finished listing the remote folder {remote_root}, \
			 so no pass plans against it yet"
		),
	)
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

/// A whole walk of a pair's local tree into a stage of the pass, on a blocking thread (see
/// [`scan::scan_local_staged`]): a row per entry rather than a node per entry in memory for the rest
/// of the plan, since a whole walk is the tree. Dropped unfinished — a pass cancelled or failed
/// while it runs — it tells the walk to stop, so the walk does not go on reading the tree for
/// nobody.
struct LocalWalk {
	handle: tokio::task::JoinHandle<rusqlite::Result<(LocalScan, IgnoreRules)>>,
	/// Shared with the walk's thread, which outlives any borrow of this.
	stop: Arc<AtomicBool>,
	/// Where the walk is staged, for the error a stage that cannot be written fails the pass with.
	at: String,
}

impl LocalWalk {
	fn start(
		stage: Stage,
		root: PathBuf,
		pair_root: PairRoot,
		baseline: Baseline,
		rules: IgnoreRules,
		rule_files: RuleFiles,
	) -> Self {
		let at = stage.path().display().to_string();
		let stop = Arc::new(AtomicBool::new(false));
		let walking = Arc::clone(&stop);
		let handle = tokio::task::spawn_blocking(move || {
			scan::scan_local_staged(
				ScanRoot {
					path: &root,
					pair_root,
				},
				&baseline,
				rules,
				rule_files,
				stage,
				&walking,
			)
		});
		Self { handle, stop, at }
	}

	/// What the walk found, and the rules it matched with.
	async fn finish(mut self) -> Result<(LocalScan, IgnoreRules), Error> {
		(&mut self.handle)
			.await
			.map_err(|e| Error::custom(ErrorKind::Internal, format!("local scan panicked: {e}")))?
			.map_err(|e| db_error(e, &format!("staging the local scan in {}", self.at)))
	}

	/// Stop the walk and wait until it has let go of the tree and its stage, which takes until each
	/// read it has under way has ended (see [`scan::scan_local_staged`]). What it found, or failed to
	/// write, is no part of the pass: a pass that walks again stages its own.
	async fn abandon(mut self) {
		self.stop.store(true, Ordering::Relaxed);
		match (&mut self.handle).await {
			Ok(Ok(_)) => {}
			Ok(Err(e)) => tracing::warn!(
				"a local scan given up before it finished could not be staged in {}: {e}",
				self.at
			),
			Err(e) => tracing::warn!("a local scan given up before it finished panicked: {e}"),
		}
	}
}

impl Drop for LocalWalk {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
	}
}

#[cfg(test)]
mod tests;
